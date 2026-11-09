//! 「在浏览器打开某个会话」的安全边界。
//!
//! ## 为什么这个功能需要一整组测试
//!
//! 这条命令的最终效果是让**用户系统默认的浏览器**打开一个地址。它挂在 HTTP
//! RPC 上，对任何已登录的控制台会话开放，因此控制台页面里的任何一段脚本（包括
//! 将来某次 XSS，或某个被塞进来的第三方资源）都能触发它。如果调用方可以指定
//! URL，一次点击就能把用户的浏览器指向 `file://` 下的本地文件或一个钓鱼站点，
//! 而用户以为自己点的只是「在浏览器打开这个会话」。
//!
//! 所以设计上只有一句话：**前端只说会话 id，URL 由后端拼**。这组测试守的就是
//! 这句话的两半——
//!
//! 1. 拼出来的地址永远是 `http://127.0.0.1:<console_port>/admin/#sessions/<uuid>`；
//! 2. 入参里额外的 `url` / `host` / `port` 字段被**忽略**，不是被采用。
//!
//! 第二半是重点：只测「正常入参会拼出正确 URL」是不够的，那种测试在「顺便接受
//! 调用方传 url」的实现下同样会绿。
//!
//! ## 为什么不经过 HTTP
//!
//! 这些测试直接调 [`proxy_rs::admin_rpc::dispatch`]，即 HTTP 路由背后的那一层。
//! 会话鉴权、cookie、`PROXY_API_TOKEN` 由 `tests/admin_api.rs` 覆盖，它们属于
//! 另一条边界；在这里重复一遍只会引入进程级 env 竞争，而让本文件真正想证明的
//! 东西（参数处理 + 钩子接线）淹没在噪声里。

use proxy_rs::admin_auth::AdminSessions;
use proxy_rs::admin_routes::AdminState;
use proxy_rs::client_accounts::ClientAuthState;
use proxy_rs::desktop_hooks::{self, DesktopHooks};
use proxy_rs::settings::LogBuffer;
use proxy_rs::stats::StatsDb;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

/// A real, lowercase hex UUID — the only shape `is_valid_session_id` accepts.
const SESSION_ID: &str = "b9c3a1d4-0e5f-4a7b-8c9d-1e2f3a4b5c6d";

/// The console port used in the happy path. Deliberately **not** the data-plane
/// port these tests put in `ProcessInfo` (3456): if the implementation ever read
/// the wrong port, this test would fail instead of passing by coincidence.
const CONSOLE_PORT: u16 = 4123;

/// `desktop_hooks` is process-global, so tests that register/unregister must not
/// interleave. Every such test takes this lock for its whole body.
static HOOK_LOCK: Mutex<()> = Mutex::const_new(());

/// Owns the global hook table for the duration of one test.
///
/// Unregisters on drop — including on a panic — so a failing test cannot leave a
/// fake browser launcher installed for its neighbours.
struct HookTable {
    _guard: tokio::sync::MutexGuard<'static, ()>,
}

impl Drop for HookTable {
    fn drop(&mut self) {
        desktop_hooks::unregister();
    }
}

async fn take_hook_table() -> HookTable {
    let guard = HOOK_LOCK.lock().await;
    // Start from "nothing registered", whatever a previous test left behind.
    desktop_hooks::unregister();
    HookTable { _guard: guard }
}

/// The admin state `dispatch` needs. Only `dispatch`'s signature is exercised
/// here, so the values are the cheapest honest ones.
fn test_state() -> Arc<AdminState> {
    Arc::new(AdminState {
        sessions: Arc::new(AdminSessions::new()),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(50)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: proxy_rs::admin::ProcessInfo {
            version: "test",
            // The **data-plane** port. Every assertion below that mentions a
            // port must not see this number; see `CONSOLE_PORT`.
            bound_port: Arc::new(AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: false,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    })
}

/// Register a desktop whose console is listening on `port`, capturing whatever
/// URL the RPC layer hands to the launcher.
///
/// Returns the capture buffer and the port the hook will report, so a test can
/// **change the port after registration** and prove the port is read at call
/// time. That is the real desktop sequence: `register` happens in `setup`, and
/// the console listener binds on a spawned task afterwards — so a registration
/// that snapshotted the port would freeze the not-yet-bound `0` and make the
/// feature permanently report "控制台未就绪".
fn register_capturing_desktop(port: u16) -> (Arc<std::sync::Mutex<Vec<String>>>, Arc<AtomicU16>) {
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = captured.clone();
    let port_holder = Arc::new(AtomicU16::new(port));
    // The hook reads this handle on every call rather than capturing the number.
    let port_for_hook = port_holder.clone();
    desktop_hooks::register(DesktopHooks {
        start_service: None,
        stop_service: None,
        restart_service: None,
        open_logs_dir: None,
        reveal_path: None,
        status: None,
        console_port: Some(Arc::new(move || port_for_hook.load(Ordering::SeqCst))),
        open_url: Some(Arc::new(move |url: String| {
            sink.lock().unwrap().push(url);
            Ok("已在浏览器中打开".to_string())
        })),
    });
    (captured, port_holder)
}

async fn call_rpc(args: Value) -> anyhow::Result<Value> {
    let state = test_state();
    proxy_rs::admin_rpc::dispatch(&state, "open_session_in_browser", &args).await
}

// ── The one allowed URL shape ──────────────────────────────────────────────

/// The composed URL is always the local console deep link, byte for byte.
#[test]
fn the_session_url_is_always_the_local_console_deep_link() {
    let url = desktop_hooks::session_console_url(SESSION_ID, CONSOLE_PORT).expect("valid inputs");
    assert_eq!(
        url,
        format!("http://127.0.0.1:{CONSOLE_PORT}/admin/#sessions/{SESSION_ID}"),
        "the only URL this feature may open"
    );
    // The `#` is load-bearing: it is the in-page deep link the console's router
    // reads. A URL that lost it points at the console root and silently drops
    // the session the user clicked.
    assert!(url.contains("/admin/#sessions/"), "{url}");
}

/// A console that has not bound yet is an error, not a `:0` URL.
///
/// The port is read from a hook at call time, so this is the "desktop is up but
/// the listener has not bound" window. Composing `http://127.0.0.1:0/…` would
/// produce an address that fails to connect with no explanation, and falling
/// back to the data-plane port would open something that answers 404 — both
/// harder to diagnose than a message that names the actual situation.
#[test]
fn a_zero_console_port_is_reported_not_composed() {
    let err = desktop_hooks::session_console_url(SESSION_ID, 0).expect_err("port 0 is an error");
    assert!(err.contains("控制台未就绪"), "{err}");
    assert!(!err.contains(":0"), "must not suggest a :0 URL: {err}");
}

/// Invalid ids never reach URL composition — that is where injection would
/// happen if the id were interpolated unchecked.
#[test]
fn an_invalid_session_id_never_reaches_a_url() {
    for bad in [
        "",
        "../x",
        "sessions/../../etc",
        "B9C3A1D4-0E5F-4A7B-8C9D-1E2F3A4B5C6D",  // upper case
        "b9c3a1d4-0e5f-4a7b-8c9d-1e2f3a4b5c6",   // 35 chars
        "b9c3a1d4-0e5f-4a7b-8c9d-1e2f3a4b5c6dd", // 37 chars
        "file:///etc/passwd",
        "https://evil.example",
        "//evil.example",
        "b9c3a1d4 0e5f 4a7b 8c9d 1e2f3a4b5c6d", // non-hex separators
        "b9c3a1d4-0e5f-4a7b-8c9d-1e2f3a4b5c6g", // non-hex digit
    ] {
        assert!(
            desktop_hooks::session_console_url(bad, CONSOLE_PORT).is_err(),
            "must reject {bad:?}"
        );
    }
}

/// The independent shape check used as the second gate before spawning anything.
///
/// This is what makes the guard real rather than decorative: it runs on the
/// string *after* composition, so a refactor that starts building URLs elsewhere
/// still has to pass it.
#[test]
fn only_the_local_console_url_passes_the_shape_check() {
    let good = desktop_hooks::session_console_url(SESSION_ID, CONSOLE_PORT).unwrap();
    assert!(desktop_hooks::is_console_url(&good), "{good}");

    for bad in [
        // Anything that is not the local console, however plausible it looks.
        "file:///etc/passwd",
        "file:///Users/me/.ssh/id_rsa",
        "https://evil.example/admin/#sessions/x",
        "http://evil.example/admin/",
        // Host-parsing tricks: the literal must be followed by a numeric port.
        "http://127.0.0.1.evil.example:4123/admin/",
        "http://127.0.0.1@evil.example:4123/admin/",
        "http://user@127.0.0.1:4123/admin/",
        // Scheme and port edge cases.
        "https://127.0.0.1:4123/admin/",
        "http://127.0.0.1:0/admin/",
        "http://127.0.0.1:/admin/",
        "http://127.0.0.1:4123x/admin/",
        "http://127.0.0.1:999999/admin/",
        // Right host, wrong path: the data plane is not the console.
        "http://127.0.0.1:4123/v1/messages",
        "http://127.0.0.1:4123/admin", // no trailing slash → not the console page
        // A `file://` URL that merely names the console in its query must not
        // pass — this is why the check is a prefix test, not a substring test.
        "file:///tmp/x?next=http://127.0.0.1:4123/admin/",
    ] {
        assert!(
            !desktop_hooks::is_console_url(bad),
            "must refuse to open {bad:?}"
        );
    }
}

// ── Login deep link (`sessions/<uuid>`) ────────────────────────────────────

/// A form login that was carrying a session deep link lands back on it.
#[tokio::test]
async fn landing_path_honours_a_session_deep_link() {
    // Login is only reachable through the router, so this goes through it —
    // `landing_path` itself is `pub(crate)`, and the form post is the real path
    // a password manager submits anyway.
    let resp = post_form_login(&format!("password=tok&next=sessions/{SESSION_ID}")).await;
    assert_eq!(
        resp.0,
        axum::http::StatusCode::SEE_OTHER,
        "a form login must navigate"
    );
    assert_eq!(
        resp.1.as_deref(),
        Some(format!("/admin/#sessions/{SESSION_ID}").as_str()),
        "the deep link must survive the login round trip"
    );
}

/// The security test: the allow-list cannot express a host, a scheme, or a
/// traversal, so it stays a hash on our own origin.
#[tokio::test]
async fn the_deep_link_allow_list_is_not_an_open_redirect() {
    for evil in [
        "sessions/../../etc",
        "sessions/../../../admin/login",
        "sessions/B9C3A1D4-0E5F-4A7B-8C9D-1E2F3A4B5C6D", // upper case
        "sessions/not-a-uuid",
        "sessions/",
        // 注意：裸 `sessions` 不在这里 —— 它是合法的 tab 名，由上面
        // `bare_tab_names_still_land_where_they_did` 覆盖。
        "https://evil.example",
        "//evil.example",
        "/admin/logs",
        "sessions/b9c3a1d4-0e5f-4a7b-8c9d-1e2f3a4b5c6d?x=1",
        "sessions/b9c3a1d4-0e5f-4a7b-8c9d-1e2f3a4b5c6d#x",
    ] {
        let body = format!("password=tok&next={}", evil.replace(' ', "%20"));
        let (status, location) = post_form_login(&body).await;
        assert_eq!(status, axum::http::StatusCode::SEE_OTHER, "{evil}");
        assert_eq!(
            location.as_deref(),
            Some("/admin/"),
            "{evil} must not become a redirect target"
        );
    }
}

/// A failed login carries the deep link forward, so the retry returns to it.
#[tokio::test]
async fn a_failed_login_keeps_the_deep_link() {
    let (status, location) =
        post_form_login(&format!("password=wrong&next=sessions/{SESSION_ID}")).await;
    assert_eq!(status, axum::http::StatusCode::SEE_OTHER);
    let location = location.unwrap_or_default();
    assert!(location.contains("e=invalid_token"), "{location}");
    assert!(
        location.contains(&format!("next=sessions%2F{SESSION_ID}"))
            || location.contains(&format!("next=sessions/{SESSION_ID}")),
        "the retry must come back to the same conversation: {location}"
    );
}

/// Bare tab names keep working — the deep link is an extension, not a
/// replacement, and the existing console relies on them.
#[tokio::test]
async fn bare_tab_names_still_land_where_they_did() {
    for tab in ["overview", "logs", "sessions", "settings"] {
        let (status, location) = post_form_login(&format!("password=tok&next={tab}")).await;
        assert_eq!(status, axum::http::StatusCode::SEE_OTHER, "{tab}");
        assert_eq!(
            location.as_deref(),
            Some(format!("/admin/#{tab}").as_str()),
            "{tab}"
        );
    }
}

/// Post the login form and return `(status, location)`.
///
/// The token is set for the duration of the call because the gate reads it from
/// the process environment; the router is built fresh so no session leaks
/// between cases.
///
/// # Why this redirects the data directory
///
/// The login handler reads `GuiSettings::load()` (for the trusted-proxy
/// decision), and [`proxy_rs::settings::settings_path`] resolves through
/// [`proxy_rs::settings::data_dir`], which calls `create_dir_all` on the way, and
/// falls back to writing a migrated `gui-settings.json`. An integration test is a
/// separate crate, so the `cfg!(test)` temp-directory branch inside `settings`
/// does not apply: without an explicit redirect this would touch the developer's
/// real `~/.proxy-rs`. `PROXY_DATA_DIR` plus the `.env` override keep the whole
/// call inside a temp directory, and both are restored on drop.
async fn post_form_login(body: &str) -> (axum::http::StatusCode, Option<String>) {
    use tower::ServiceExt;
    // Serialise on the hook lock as well: this mutates process-global env, and
    // the hook tests in this same binary also depend on env-derived behaviour.
    // One lock for "process-global state" is simpler to reason about than two
    // that must be taken in a fixed order.
    let _guard = HOOK_LOCK.lock().await;
    let _env = EnvIsolation::new("login");
    std::env::set_var("PROXY_API_TOKEN", "tok");
    // Do not let a developer's real `.env` supply a *different* token.
    proxy_rs::env::set_dotenv_override_for_tests(None);

    let state = test_state();
    let app = proxy_rs::admin_routes::build_admin_router(state.clone())
        .merge(proxy_rs::console::build_console_router(state));
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/admin/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(axum::body::Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.expect("router responds");
    let status = resp.status();
    let location = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    (status, location)
}

/// Points `PROXY_DATA_DIR` at a temp directory and restores the previous value.
///
/// Restoring (rather than just removing) matters: another test in this binary
/// that had its own override would otherwise see it cleared by this one, and the
/// failure would appear in *that* test, at a distance from the cause.
struct EnvIsolation {
    dir: std::path::PathBuf,
    saved: Option<std::ffi::OsString>,
}

impl EnvIsolation {
    fn new(name: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("proxy-rs-it-openurl-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir");
        let saved = std::env::var_os("PROXY_DATA_DIR");
        std::env::set_var("PROXY_DATA_DIR", &dir);
        Self { dir, saved }
    }
}

impl Drop for EnvIsolation {
    fn drop(&mut self) {
        std::env::remove_var("PROXY_API_TOKEN");
        match self.saved.take() {
            Some(v) => std::env::set_var("PROXY_DATA_DIR", v),
            None => std::env::remove_var("PROXY_DATA_DIR"),
        }
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

// ── The RPC command ────────────────────────────────────────────────────────

/// The happy path: the desktop composes the URL from the id and its own console
/// port, and hands exactly that string to the launcher.
#[tokio::test]
async fn the_rpc_opens_the_console_url_the_desktop_composed() {
    let _hooks = take_hook_table().await;
    let (captured, _port) = register_capturing_desktop(CONSOLE_PORT);

    let result = call_rpc(json!({ "id": SESSION_ID }))
        .await
        .expect("must open");

    let expected = format!("http://127.0.0.1:{CONSOLE_PORT}/admin/#sessions/{SESSION_ID}");
    assert_eq!(result["url"], expected, "{result}");
    assert_eq!(result["opened"], true, "{result}");
    assert_eq!(result["ok"], true, "{result}");

    let opened = captured.lock().unwrap().clone();
    assert_eq!(opened, vec![expected], "the launcher saw a different URL");
}

/// **The boundary test.** Extra `url` / `host` / `port` fields must be ignored.
///
/// A naive implementation that passed `args["url"]` through would open
/// `file:///etc/passwd` here while still passing every happy-path assertion, so
/// this case is the one that actually pins the design.
#[tokio::test]
async fn the_rpc_ignores_client_supplied_url_host_and_port() {
    let _hooks = take_hook_table().await;
    let (captured, _port) = register_capturing_desktop(CONSOLE_PORT);

    let result = call_rpc(json!({
        "id": SESSION_ID,
        "url": "file:///etc/passwd",
        "host": "evil.example",
        "port": 9999,
        "href": "https://evil.example/admin/",
        "next": "https://evil.example",
    }))
    .await
    .expect("the command itself is legitimate");

    let expected = format!("http://127.0.0.1:{CONSOLE_PORT}/admin/#sessions/{SESSION_ID}");
    assert_eq!(
        result["url"], expected,
        "a client-supplied url must never win"
    );
    assert_eq!(
        captured.lock().unwrap().clone(),
        vec![expected],
        "the launcher must only ever see the backend-composed URL"
    );
}

/// A headless server has no browser to launch: the console the operator is
/// clicking in is already a tab, so say so instead of pretending.
#[tokio::test]
async fn a_server_build_says_the_console_is_already_in_the_browser() {
    let _hooks = take_hook_table().await; // nothing registered
    let err = call_rpc(json!({ "id": SESSION_ID }))
        .await
        .expect_err("a server cannot open a browser");
    let message = err.to_string();
    assert!(message.contains("浏览器"), "{message}");
    assert!(
        message.contains("直接复制") || message.contains("已在浏览器"),
        "the message must tell the operator what to do instead: {message}"
    );
}

/// Invalid ids are refused at the RPC layer too, not only in the composer.
#[tokio::test]
async fn the_rpc_refuses_an_invalid_session_id() {
    let _hooks = take_hook_table().await;
    let (captured, _port) = register_capturing_desktop(CONSOLE_PORT);

    for bad in [
        "../x",
        "",
        "B9C3A1D4-0E5F-4A7B-8C9D-1E2F3A4B5C6D",
        "not-a-uuid",
    ] {
        let err = call_rpc(json!({ "id": bad }))
            .await
            .expect_err(&format!("{bad:?} must be rejected"));
        assert!(
            err.to_string().contains("无效") || err.to_string().contains("缺少参数"),
            "{bad:?}: {err}"
        );
    }
    // A missing id is an error as well, not an empty-string session.
    assert!(call_rpc(json!({})).await.is_err());
    // Nothing was ever launched.
    assert!(captured.lock().unwrap().is_empty());
}

/// The port is read from the hook **at call time**. A registered desktop whose
/// console has not bound yet must get the "not ready" message, not `:0`.
#[tokio::test]
async fn a_not_yet_bound_console_port_is_an_error_not_a_url() {
    let _hooks = take_hook_table().await;
    let (captured, _port) = register_capturing_desktop(0);

    let err = call_rpc(json!({ "id": SESSION_ID }))
        .await
        .expect_err("port 0 means the console is not listening");
    assert!(err.to_string().contains("控制台未就绪"), "{err}");
    assert!(
        captured.lock().unwrap().is_empty(),
        "nothing may be spawned"
    );
}

/// The port is read from the hook **at call time**, not captured at
/// registration.
///
/// This is the real desktop sequence: `desktop_hooks::register` runs in Tauri's
/// `setup`, while the console listener binds on a spawned task and only then
/// stores its port. A registration that snapshotted the number would keep
/// reporting the initial `0` forever — the feature would say "控制台未就绪" even
/// though the console is up. Mutating the port after `register` and expecting
/// the new value is what distinguishes the two.
#[tokio::test]
async fn the_console_port_is_read_at_call_time_not_at_registration() {
    let _hooks = take_hook_table().await;
    // Register while "not bound yet", exactly like startup does.
    let (captured, port) = register_capturing_desktop(0);

    // Nothing bound: the error is the honest answer.
    assert!(call_rpc(json!({ "id": SESSION_ID })).await.is_err());
    assert!(captured.lock().unwrap().is_empty());

    // The listener binds: same registration, new port.
    port.store(CONSOLE_PORT, Ordering::SeqCst);
    let result = call_rpc(json!({ "id": SESSION_ID }))
        .await
        .expect("once bound, the same registration must work");
    assert_eq!(
        result["url"],
        format!("http://127.0.0.1:{CONSOLE_PORT}/admin/#sessions/{SESSION_ID}"),
        "{result}"
    );
}

/// The desktop never sees a URL it did not compose — and the shape check rejects
/// anything else before the launcher runs.
///
/// The hook is the last place that can stop a bad string before an OS program
/// acts on it, so this pins the contract from the other side: given a hook that
/// records what it receives, what it receives is always the checked shape. (The
/// third check lives in `src-tauri/src/main.rs` and cannot be exercised from a
/// test that does not link Tauri; it calls the same
/// [`proxy_rs::desktop_hooks::is_console_url`], which
/// `only_the_local_console_url_passes_the_shape_check` covers exhaustively.)
#[tokio::test]
async fn the_launcher_only_ever_receives_the_checked_shape() {
    let _hooks = take_hook_table().await;
    let (captured, _port) = register_capturing_desktop(CONSOLE_PORT);

    // Call it several times with hostile extra fields; every recorded URL must
    // still pass the independent check.
    for extra in [
        json!({}),
        json!({ "url": "file:///etc/passwd" }),
        json!({ "url": "https://evil.example/x", "host": "evil.example" }),
        json!({ "port": 1, "href": "//evil.example" }),
    ] {
        let mut args = extra;
        args["id"] = json!(SESSION_ID);
        call_rpc(args).await.expect("legitimate id");
    }

    let opened = captured.lock().unwrap().clone();
    assert_eq!(opened.len(), 4, "{opened:?}");
    for url in &opened {
        assert!(
            desktop_hooks::is_console_url(url),
            "the launcher received a URL outside the allowed shape: {url}"
        );
        assert!(
            !url.contains("evil.example") && !url.starts_with("file:"),
            "client-supplied host/scheme leaked into the opened URL: {url}"
        );
    }
}

/// A launcher failure surfaces as an error. Reporting `opened: true` when the
/// OS refused would leave the user clicking a button that does nothing.
#[tokio::test]
async fn a_failing_browser_launcher_is_reported_not_swallowed() {
    let _hooks = take_hook_table().await;
    desktop_hooks::register(DesktopHooks {
        start_service: None,
        stop_service: None,
        restart_service: None,
        open_logs_dir: None,
        reveal_path: None,
        status: None,
        console_port: Some(Arc::new(|| CONSOLE_PORT)),
        open_url: Some(Arc::new(|_url: String| {
            Err("调用 open 失败: No such file or directory".to_string())
        })),
    });

    let err = call_rpc(json!({ "id": SESSION_ID }))
        .await
        .expect_err("a failed spawn must not look like success");
    assert!(err.to_string().contains("open"), "{err}");
}
