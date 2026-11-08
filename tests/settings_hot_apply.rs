//! Saving settings must reach the **running** proxy, not just the file.
//!
//! ## The regression this pins
//!
//! `Config` is built once, by `Config::from_settings`, when the listener is
//! constructed. Everything derived from settings — the upstream URL,
//! `model_map`, `sanitize_terms`, `force_stream`, `intercept_safety_classifier`
//! — lives inside that snapshot for the life of the listener.
//!
//! Up to 3.4.0 the desktop's own `save_settings` command ended with
//! `start_proxy_server(...)`, which rebuilt the listener, so "保存配置" meant what
//! it said. 3.4.0 A4 deleted that command body as one of 53 duplicates, and the
//! rebuild went with it. Nothing failed, because every remaining test asserted
//! on the *file*:
//!
//!   * the file gets the new value ✓
//!   * the live inbound gate is hot-applied ✓ (that one has its own test)
//!   * the running proxy picks up the new `Config` ✗ — never asserted
//!
//! It surfaced in 3.5.2 as a switch that did nothing: turning
//! `intercept_safety_classifier` on through the console wrote the file, reported
//! success, and left the classifier still being forwarded upstream.
//!
//! ## What is asserted, and why at this level
//!
//! The rebuild itself is a Tauri lifecycle call and cannot run in a test. What
//! *can* be pinned is the contract that makes the rebuild reachable: a
//! `save_settings` RPC must invoke the `restart_service` hook. That is the exact
//! link that was deleted — a hook that exists but is never called looks
//! identical to a missing one from the user's seat, and a test on the hook's
//! existence alone would have stayed green through the whole regression.
//!
//! A second test pins the server-side wording: with no hook registered (a
//! headless deployment, where systemd owns the restart) the save must still
//! succeed and must say that a restart is needed. Failing the save would be
//! wrong — the file was written, and the operator's next `systemctl restart` is
//! what applies it.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use proxy_rs::admin_routes::{build_admin_router, AdminState};
use proxy_rs::client_accounts::ClientAuthState;
use proxy_rs::desktop_hooks::{self, DesktopHooks};
use proxy_rs::settings::LogBuffer;
use proxy_rs::stats::StatsDb;
use serde_json::Value;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;

/// The admin gate reads `PROXY_API_TOKEN` from process-global state, so tests
/// that want different tokens must not run concurrently.
static GLOBAL_LOCK: Mutex<()> = Mutex::const_new(());

const TOKEN_ENV: &str = "PROXY_API_TOKEN";

/// Serialises env mutation and restores the data directory afterwards.
struct Isolated {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
}

impl Drop for Isolated {
    fn drop(&mut self) {
        std::env::remove_var(TOKEN_ENV);
        std::env::remove_var("PROXY_DATA_DIR");
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

async fn isolate(name: &str) -> Isolated {
    let guard = GLOBAL_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!(
        "proxy-rs-it-hotapply-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::env::set_var("PROXY_DATA_DIR", &dir);
    std::env::set_var(TOKEN_ENV, "tok");
    Isolated { _guard: guard, dir }
}

fn app_for(state: Arc<AdminState>) -> axum::Router {
    // The console router is merged in because that is what serves the real page
    // and it is what `tests/admin_api.rs` drives; the RPC endpoint is on the
    // admin router.
    build_admin_router(state.clone()).merge(proxy_rs::console::build_console_router(state))
}

fn state_with(logs: Arc<LogBuffer>) -> Arc<AdminState> {
    Arc::new(AdminState {
        sessions: Arc::new(proxy_rs::admin_auth::AdminSessions::new()),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs,
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: proxy_rs::admin::ProcessInfo {
            version: "test",
            bound_port: Arc::new(std::sync::atomic::AtomicU16::new(3457)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: true,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    })
}

async fn login_cookie(app: axum::Router) -> String {
    let response = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/admin/api/login")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"tok"}"#))
                .expect("login request"),
        )
        .await
        .expect("login response");
    assert_eq!(response.status(), StatusCode::OK, "login must succeed");
    response
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("a session cookie")
        .split(';')
        .next()
        .expect("cookie pair")
        .to_string()
}

/// POST one command to the console's RPC endpoint.
///
/// `args` is the argument object the UI would pass to `invoke` — the route is
/// `POST /admin/api/rpc/<command>` and the body *is* that object (see
/// `console::rpc`), not a `{command, args}` envelope.
async fn post_rpc(
    app: axum::Router,
    command: &str,
    args: &str,
    cookie: &str,
) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/admin/api/rpc/{command}"))
                .header("content-type", "application/json")
                .header("cookie", cookie)
                .body(Body::from(args.to_string()))
                .expect("rpc request"),
        )
        .await
        .expect("rpc response");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// **The regression.** A settings save on the desktop must ask for a rebuild.
///
/// Without this assertion the switch can be wired to the file and to nothing
/// else: `save_settings` returns 200, the form says 「✓ 配置保存成功」, and the
/// running proxy keeps serving the old snapshot. That is exactly what shipped in
/// 3.5.2.
#[tokio::test]
async fn saving_settings_rebuilds_the_running_proxy_on_the_desktop() {
    let _iso = isolate("desktop-rebuild").await;

    let restarts = Arc::new(AtomicUsize::new(0));
    let restarts_c = restarts.clone();
    desktop_hooks::register(DesktopHooks {
        restart_service: Some(Arc::new(move || {
            restarts_c.fetch_add(1, Ordering::SeqCst);
            Ok("代理服务已按新配置重启".into())
        })),
        // Not under test here; present so the shape stays the desktop's.
        start_service: None,
        stop_service: None,
        open_logs_dir: None,
        reveal_path: None,
        status: None,
    });

    let app = app_for(state_with(Arc::new(LogBuffer::new(100))));
    let cookie = login_cookie(app.clone()).await;

    let (status, body) = post_rpc(
        app,
        "save_settings",
        r#"{"body":{"intercept_safety_classifier":true}}"#,
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        restarts.load(Ordering::SeqCst) >= 1,
        "saving settings must rebuild the running listener, otherwise the value \
         reaches the file and never reaches the proxy: {body}"
    );

    desktop_hooks::unregister();
}

/// A headless deployment has no restartable listener, and must still save.
///
/// Failing here would be worse than the bug: the file *is* written, the operator
/// applies it with `systemctl restart proxy-rs`, and a red error would send them
/// looking for a problem that does not exist.
#[tokio::test]
async fn a_server_save_succeeds_and_mentions_the_restart() {
    let _iso = isolate("server-rebuild").await;
    // No hooks registered: this is what a server build looks like.
    desktop_hooks::unregister();

    let logs = Arc::new(LogBuffer::new(200));
    let app = app_for(state_with(logs.clone()));
    let cookie = login_cookie(app.clone()).await;

    let (status, body) = post_rpc(
        app,
        "save_settings",
        r#"{"body":{"intercept_safety_classifier":true}}"#,
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "a server save must succeed: {body}");

    // The log line is the only place an operator learns a restart is pending.
    let entries = logs.snapshot().await;
    let line = entries
        .iter()
        .find(|e| e.message.contains("设置已保存"))
        .map(|e| e.message.clone())
        .expect("the save must be logged");
    assert!(
        line.contains("systemctl restart"),
        "a server save must say how it takes effect, got: {line}"
    );
}
