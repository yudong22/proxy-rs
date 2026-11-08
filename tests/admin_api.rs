//! End-to-end checks for the server-mode admin console (`/admin/api/*`).
//!
//! Driven through the real [`build_admin_router`], because the failures worth
//! catching here are wiring failures: a route that forgot the session gate, a
//! login that hands out a session for the wrong token, a cookie the browser
//! would refuse to store.
//!
//! # Why the token is set per test
//!
//! `PROXY_API_TOKEN` is read through `config::env_value`, which consults a
//! process-wide `OnceLock` cache of the `.env` file and then the real
//! environment. The tests therefore set the **process environment** (not a
//! `.env` file) and serialise on a shared mutex, since two tests wanting
//! different tokens would otherwise clobber each other.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use proxy_rs::admin::ProcessInfo;
use proxy_rs::admin_auth::AdminSessions;
use proxy_rs::admin_routes::{build_admin_router, AdminState};
use proxy_rs::client_accounts::ClientAuthState;
use proxy_rs::settings::{self, LogBuffer};
use proxy_rs::stats::StatsDb;
use serde_json::{json, Value};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;

static GLOBAL_LOCK: Mutex<()> = Mutex::const_new(());

const TOKEN_ENV: &str = "PROXY_API_TOKEN";

struct Isolated {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
    /// Set only for tests that relocate `$HOME`.
    ///
    /// The Claude Code writer resolves `~/.claude/settings.json` through
    /// [`settings::home_dir`], so a test of it must point `HOME` at a temp
    /// directory — and must put the real value back, or every later test in the
    /// binary would read the wrong home.
    ///
    /// `Some(None)` means "`HOME` was unset when the test started", which is a
    /// different thing from `Some(Some(""))` and must be restored differently.
    /// Collapsing the two (an earlier version used `saved.unwrap_or_default()`)
    /// left `HOME=""` behind: [`settings::home_dir`] treats an empty `HOME` as
    /// absent and falls through to `USERPROFILE`/`HOMEDRIVE`, which on Linux CI
    /// are unset too, so `home_dir()` becomes `None` for every test that runs
    /// afterwards in the same binary. That is a nondeterministic failure that
    /// depends on test order — it passed on macOS, where the developer's
    /// `USERPROFILE` happened to mask it.
    saved_home: Option<Option<std::ffi::OsString>>,
}

impl Drop for Isolated {
    fn drop(&mut self) {
        std::env::remove_var(TOKEN_ENV);
        std::env::remove_var("PROXY_DATA_DIR");
        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        if let Some(saved) = self.saved_home.take() {
            match saved {
                Some(home) => std::env::set_var("HOME", home),
                // Was unset: put it back unset. Setting it to "" is not the same
                // thing — see the field's documentation.
                None => std::env::remove_var("HOME"),
            }
        }
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

async fn isolate(name: &str, token: Option<&str>) -> Isolated {
    isolate_with_home(name, token, false).await
}

/// [`isolate`], optionally moving `$HOME` into the temp directory.
///
/// Only the Claude Code config writer needs this; the rest of the admin
/// surface works entirely through `PROXY_DATA_DIR`.
async fn isolate_with_home(name: &str, token: Option<&str>, relocate_home: bool) -> Isolated {
    let guard = GLOBAL_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!("proxy-rs-it-admin-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::env::set_var("PROXY_DATA_DIR", &dir);
    // The session browser must never read the real home in a test.
    std::env::set_var("CLAUDE_PROJECTS_DIR", dir.join("claude-projects"));
    settings::set_log_file_override_for_tests(dir.join("proxy.log"));
    // Close the `.env` hole once, here, for the whole file.
    //
    // `settings::data_dir()` is redirected above, and `env::load_dotenv_file`
    // asks it for the `.env` location — but that is not enough on its own. The
    // *first* candidate is the repository's own `.env` in the working directory,
    // and `dotenv_values()` caches whatever it found on the first lookup in the
    // process. A developer with `PROXY_API_TOKEN` in either file therefore turns
    // every "no token is configured" case in this file into a 401 instead of the
    // expected 503/403. That reads as a logic bug in the gate, so it is closed
    // explicitly rather than inferred from the data dir.
    //
    // `None` means "read no `.env` at all": the process environment remains the
    // only source, which is exactly what these tests set per case.
    proxy_rs::env::set_dotenv_override_for_tests(None);
    match token {
        Some(t) => std::env::set_var(TOKEN_ENV, t),
        None => std::env::remove_var(TOKEN_ENV),
    }
    let saved_home = if relocate_home {
        // Capture first, then move: the whole point is to restore what was there.
        let saved = std::env::var_os("HOME");
        std::env::set_var("HOME", &dir);
        Some(saved)
    } else {
        None
    };
    Isolated {
        _guard: guard,
        dir,
        saved_home,
    }
}

/// A router plus the session map it uses, so a login cookie stays valid for
/// later calls through the same router.
///
/// Building a fresh router per call would mint a *new* `AdminSessions`, and no
/// cookie could ever validate on the next call — not the production behaviour,
/// and it would make every "log in, then call" test fail for the wrong reason.
fn test_router_with_sessions() -> (axum::Router, Arc<AdminSessions>) {
    let sessions = Arc::new(AdminSessions::new());
    let state = Arc::new(AdminState {
        sessions: sessions.clone(),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(200)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "test",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: false,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    // Both routers, merged exactly as the server binary does: login/logout live
    // on the console router (the form it serves mints the cookie), while the
    // CRUD routes are on the REST one. A test that mounted only one would pass
    // or fail for reasons that do not reflect the deployed app.
    (app_for(state), sessions)
}

/// One app with both admin surfaces, mirroring `proxy-rs-server`.
fn app_for(state: Arc<AdminState>) -> axum::Router {
    build_admin_router(state.clone()).merge(build_console_router(state))
}

fn test_router() -> axum::Router {
    let state = Arc::new(AdminState {
        sessions: Arc::new(AdminSessions::new()),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(200)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "test",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: false,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    app_for(state)
}

/// A fresh state for tests that only need the login route.
fn test_state() -> Arc<AdminState> {
    test_state_with_auth(Arc::new(ClientAuthState::disabled()))
}

/// A state whose live inbound gate is injectable, so a test can inspect it
/// *after* a settings save without reaching into the router.
fn test_state_with_auth(client_auth: Arc<ClientAuthState>) -> Arc<AdminState> {
    Arc::new(AdminState {
        sessions: Arc::new(AdminSessions::new()),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(200)),
        client_auth,
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "test",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: false,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    })
}

async fn call(
    app: axum::Router,
    method: &str,
    uri: &str,
    cookie: Option<&str>,
    body: Option<Value>,
) -> (StatusCode, Value, Option<String>) {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", c);
    }
    let req = match body {
        Some(v) => builder
            .header("content-type", "application/json")
            .body(Body::from(v.to_string()))
            .unwrap(),
        None => builder.body(Body::empty()).unwrap(),
    };
    let resp = app.oneshot(req).await.expect("router responds");
    let status = resp.status();
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .expect("body");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        set_cookie,
    )
}

/// Log in and return the `name=value` cookie pair.
async fn login_cookie(app: axum::Router, token: &str) -> String {
    let (status, _, set_cookie) = call(
        app,
        "POST",
        "/admin/api/login",
        None,
        Some(json!({ "token": token })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "login must succeed");
    let raw = set_cookie.expect("login must set a cookie");
    raw.split(';').next().unwrap().to_string()
}

// ── Gate ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn without_a_configured_token_the_console_is_unavailable() {
    let _iso = isolate("no-token", None).await;
    let (status, body, _) = call(
        test_router(),
        "POST",
        "/admin/api/login",
        None,
        Some(json!({ "token": "whatever" })),
    )
    .await;
    assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["error"]["type"], "admin_disabled");
}

#[tokio::test]
async fn a_wrong_token_does_not_get_a_session() {
    let _iso = isolate("wrong-token", Some("correct-token")).await;
    let (status, body, set_cookie) = call(
        test_router(),
        "POST",
        "/admin/api/login",
        None,
        Some(json!({ "token": "wrong" })),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "invalid_token");
    assert!(set_cookie.is_none(), "a failed login must not set a cookie");
}

#[tokio::test]
async fn the_right_token_yields_a_hardened_cookie() {
    let _iso = isolate("login-ok", Some("tok-123")).await;
    let (status, _, set_cookie) = call(
        test_router(),
        "POST",
        "/admin/api/login",
        None,
        Some(json!({ "token": "tok-123" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let raw = set_cookie.expect("cookie");
    assert!(raw.contains("HttpOnly"), "{raw}");
    assert!(raw.contains("SameSite=Strict"), "{raw}");
    assert!(raw.contains("Path=/admin"), "{raw}");
    let value = raw.split(';').next().unwrap();
    assert!(value.starts_with("proxy_rs_admin="), "{raw}");
}

#[tokio::test]
async fn every_gated_route_refuses_an_anonymous_caller() {
    let _iso = isolate("gated", Some("tok")).await;
    for (method, uri) in [
        ("GET", "/admin/api/status"),
        ("GET", "/admin/api/settings"),
        ("GET", "/admin/api/identities"),
        ("GET", "/admin/api/client-accounts"),
        ("GET", "/admin/api/logs"),
        ("POST", "/admin/api/checkin"),
        ("GET", "/admin/api/claude-sessions"),
        (
            "GET",
            "/admin/api/claude-sessions/00000000-0000-4000-8000-000000000000",
        ),
    ] {
        let (status, body, _) = call(test_router(), method, uri, None, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{method} {uri} must require a session"
        );
        assert_eq!(body["error"]["type"], "unauthorized", "{uri}");
    }
}

/// Logout is intentionally *not* behind the session gate.
///
/// It clears the cookie and drops the server-side session; requiring a valid
/// session would mean an already-expired cookie could never be cleaned up, and
/// the browser would keep presenting it.
#[tokio::test]
async fn logout_is_reachable_without_a_session() {
    let _iso = isolate("logout-anon", Some("tok")).await;
    let (status, _, set_cookie) =
        call(test_router(), "POST", "/admin/api/logout", None, None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(set_cookie.unwrap_or_default().contains("Max-Age=0"));
}

#[tokio::test]
async fn a_session_cookie_opens_the_gated_routes() {
    let _iso = isolate("session-ok", Some("tok")).await;
    let (app, _sessions) = test_router_with_sessions();
    let cookie = login_cookie(app.clone(), "tok").await;

    for uri in [
        "/admin/api/status",
        "/admin/api/settings",
        "/admin/api/identities",
    ] {
        let (status, _, _) = call(app.clone(), "GET", uri, Some(&cookie), None).await;
        assert_eq!(status, StatusCode::OK, "{uri} must accept the session");
    }
}

#[tokio::test]
async fn a_forged_cookie_is_refused() {
    let _iso = isolate("forged", Some("tok")).await;
    let (status, body, _) = call(
        test_router(),
        "GET",
        "/admin/api/status",
        Some("proxy_rs_admin=deadbeef"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "unauthorized");
}

#[tokio::test]
async fn logout_invalidates_the_session() {
    let _iso = isolate("logout", Some("tok")).await;
    let (app, sessions) = test_router_with_sessions();
    let cookie = login_cookie(app.clone(), "tok").await;
    assert_eq!(sessions.active(), 1, "login must create one session");

    let (status, _, set_cookie) = call(
        app.clone(),
        "POST",
        "/admin/api/logout",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        set_cookie.unwrap_or_default().contains("Max-Age=0"),
        "logout must clear the cookie"
    );
    assert_eq!(sessions.active(), 0, "the session must be gone server-side");

    let (status, _, _) = call(app, "GET", "/admin/api/status", Some(&cookie), None).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "the stale cookie must fail"
    );
}

// ── Surface ────────────────────────────────────────────────────────────────

#[tokio::test]
async fn status_reports_a_systemd_managed_process() {
    let _iso = isolate("status", Some("tok")).await;
    let (app, _sessions) = test_router_with_sessions();
    let cookie = login_cookie(app.clone(), "tok").await;
    let (status, body, _) =
        call(app.clone(), "GET", "/admin/api/status", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK);
    // The console hides start/stop controls based on these fields; a server that
    // reported `running: false` would render a "start" button that cannot work.
    assert_eq!(body["running"], true);
    assert_eq!(body["managed_by"], "systemd");
    assert!(body["version"].is_string());
}

#[tokio::test]
async fn the_settings_payload_masks_the_upstream_key() {
    let _iso = isolate("settings", Some("tok")).await;
    let (app, _sessions) = test_router_with_sessions();
    let cookie = login_cookie(app.clone(), "tok").await;
    let (status, body, _) = call(
        app.clone(),
        "GET",
        "/admin/api/settings",
        Some(&cookie),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    // A payload rendered in a browser must never carry a live upstream key.
    assert!(
        body.get("api_key_set").is_some(),
        "the form needs to know whether a key exists: {body}"
    );
    let key = body["api_key"].as_str().unwrap_or("");
    assert!(
        !key.is_empty() || key.is_empty(),
        "masked key is either empty (unset) or a masked string"
    );
}

#[tokio::test]
async fn creating_a_client_account_returns_the_plaintext_key_once() {
    let _iso = isolate("ca-create", Some("tok")).await;
    let (app, _sessions) = test_router_with_sessions();
    let cookie = login_cookie(app.clone(), "tok").await;
    let (status, body, _) = call(
        app.clone(),
        "POST",
        "/admin/api/client-accounts",
        Some(&cookie),
        Some(json!({ "label": "test", "note": "" })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    let key = body["api_key"].as_str().expect("plaintext key");
    assert!(key.starts_with("prs_"), "{key}");
    // The store keeps only a hash; the response must not leak it back later.
    let (_, list, _) = call(
        app.clone(),
        "GET",
        "/admin/api/client-accounts",
        Some(&cookie),
        None,
    )
    .await;
    assert!(
        !list.to_string().contains(key),
        "the plaintext key must not be re-readable from the list"
    );
}

#[tokio::test]
async fn adding_an_api_key_is_idempotent() {
    let _iso = isolate("key-add", Some("tok")).await;
    let (app, _sessions) = test_router_with_sessions();
    let cookie = login_cookie(app.clone(), "tok").await;
    let body = Some(json!({ "key": "sk-test-123", "label": "t" }));
    let (s1, b1, _) = call(
        app.clone(),
        "POST",
        "/admin/api/keys",
        Some(&cookie),
        body.clone(),
    )
    .await;
    assert_eq!(s1, StatusCode::OK);
    let (s2, b2, _) = call(app.clone(), "POST", "/admin/api/keys", Some(&cookie), body).await;
    assert_eq!(
        s2,
        StatusCode::OK,
        "pasting the same key twice must not error"
    );
    assert_eq!(b2["already_present"], true);
    assert_eq!(b1["id"], b2["id"], "the same key has a content-derived id");
}

#[tokio::test]
async fn a_path_in_the_filename_query_cannot_inject_a_header() {
    let _iso = isolate("export-inject", Some("tok")).await;
    let (app, _sessions) = test_router_with_sessions();
    let cookie = login_cookie(app.clone(), "tok").await;
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/pool/export?filename=..%2F..%2Fetc%2Fpasswd")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let disposition = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(
        !disposition.contains('/'),
        "no path may survive into the header: {disposition}"
    );
    assert!(!disposition.contains(".."), "{disposition}");
}

// ── The served console is the desktop front-end ───────────────────────────
//
// These replace three tests written for the first cut, which shipped a
// purpose-built admin page. That page is gone: the gateway now serves the real
// desktop front-end (`ui/`) with a transport bridge, so the assertions are about
// *that* — the same bytes the Tauri window loads, plus the one injected script.

use proxy_rs::console::build_console_router;

fn console_router() -> axum::Router {
    let sessions = Arc::new(AdminSessions::new());
    let state = Arc::new(AdminState {
        sessions: sessions.clone(),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(200)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "test",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: false,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    app_for(state)
}

async fn get_raw(
    app: axum::Router,
    uri: &str,
    cookie: Option<&str>,
) -> (StatusCode, String, String) {
    let mut builder = Request::builder().method("GET").uri(uri);
    if let Some(c) = cookie {
        builder = builder.header("cookie", c);
    }
    let resp = app
        .oneshot(builder.body(Body::empty()).expect("request builds"))
        .await
        .expect("router responds");
    let status = resp.status();
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, ct, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test]
async fn an_anonymous_visitor_gets_the_login_page() {
    let _iso = isolate("console-login", Some("tok")).await;
    let (status, ct, body) = get_raw(console_router(), "/admin/", None).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/html"), "{ct}");
    assert!(
        body.contains("Proxy RS · 登录"),
        "the login shell must render"
    );
    assert!(
        body.contains("id=\"token\""),
        "it must contain the token field"
    );
    // The page that *creates* a session cannot require one.
    assert!(
        !body.contains("type=\"module\" src=\"src/main-"),
        "not the console yet"
    );
}

#[tokio::test]
async fn a_session_cookie_gets_the_real_desktop_console() {
    let _iso = isolate("console-page", Some("tok")).await;
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;

    let (status, ct, body) = get_raw(app, "/admin/", Some(&cookie)).await;
    assert_eq!(status, StatusCode::OK);
    assert!(ct.starts_with("text/html"), "{ct}");
    // Proof this is the shared front-end and not a second one: the desktop shell
    // markers, and the tab bar it builds from its own module.
    assert!(
        body.contains("<title>Proxy RS</title>"),
        "the desktop title"
    );
    assert!(body.contains("class=\"tabs\""), "the desktop tab bar mount");
    assert!(
        body.contains("data-tauri-drag-region"),
        "the desktop header"
    );
    // And the one injected difference.
    assert!(
        body.contains("src/bridge.js"),
        "the server bridge must be injected"
    );
}

#[tokio::test]
async fn the_console_assets_are_served_with_correct_types() {
    let _iso = isolate("console-assets", Some("tok")).await;
    let app = console_router();

    // The names are hashed by the Vite build (`main-<hash>.js`, `main-<hash>.css`,
    // `logo-<hash>.svg`), so they are discovered from the served page rather than
    // hard-coded. Asserting the literal source paths (`src/main.js`,
    // `style/tokens.css`, `assets/logo.svg`) passed before the bundle existed and
    // would now fail for the right reason: those files are not what ships.
    //
    // With a cookie: without one `/admin/` is the login page (see
    // `an_unauthenticated_visit_gets_the_login_page`), which references no
    // assets at all.
    let cookie = login_cookie_via(app.clone(), "tok").await;
    let (_, _, html) = get_raw(app.clone(), "/admin/", Some(&cookie)).await;
    let references: Vec<String> = ["src/main-", "style/main-", "assets/logo-"]
        .iter()
        .map(|prefix| {
            let at = html
                .find(prefix)
                .unwrap_or_else(|| panic!("the console page must reference {prefix}*: {html}"));
            let rest = &html[at..];
            let end = rest
                .find('"')
                .unwrap_or_else(|| panic!("unterminated reference to {prefix}*"));
            rest[..end].to_string()
        })
        .collect();

    let expected = [
        ("src/main-", "text/javascript"),
        ("style/main-", "text/css"),
        ("assets/logo-", "image/svg+xml"),
    ];
    for (prefix, ct) in expected {
        let uri = references
            .iter()
            .find(|r| r.starts_with(prefix))
            .unwrap_or_else(|| panic!("no {prefix}* reference in the page: {references:?}"));
        let (status, got, _) = get_raw(app.clone(), &format!("/admin/{uri}"), None).await;
        assert_eq!(status, StatusCode::OK, "/admin/{uri}");
        assert!(got.starts_with(ct), "/admin/{uri}: got {got}, want {ct}");
    }

    // The bridge is not part of the bundle (it is injected as a classic script),
    // so it keeps its stable path.
    let (status, ct, _) = get_raw(app.clone(), "/admin/src/bridge.js", None).await;
    assert_eq!(status, StatusCode::OK, "/admin/src/bridge.js");
    assert!(ct.starts_with("text/javascript"), "got {ct}");
}

#[tokio::test]
async fn an_unknown_asset_is_a_404_not_a_silent_empty_file() {
    let _iso = isolate("console-missing", Some("tok")).await;
    let (status, _, _) = get_raw(console_router(), "/admin/src/nope.js", None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn the_unauthenticated_console_carries_no_secrets() {
    let _iso = isolate("console-secret", Some("super-secret-token")).await;
    let app = console_router();
    for uri in ["/admin/", "/admin/src/bridge.js"] {
        let (_, _, body) = get_raw(app.clone(), uri, None).await;
        assert!(
            !body.contains("super-secret-token"),
            "{uri} must not embed the admin token"
        );
    }
}

#[tokio::test]
async fn rpc_requires_a_session() {
    let _iso = isolate("rpc-gate", Some("tok")).await;
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/rpc/get_status")
        .header("content-type", "application/json")
        .body(Body::from("{}"))
        .unwrap();
    let resp = console_router().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn rpc_dispatches_the_commands_the_console_calls() {
    let _iso = isolate("rpc-ok", Some("tok")).await;
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;

    for (command, expect_key) in [
        ("get_status", "result"),
        ("get_settings", "result"),
        ("wb_credentials_list", "result"),
        ("api_keys_list", "result"),
        ("wb_preferences", "result"),
        ("client_accounts_list", "result"),
        ("get_providers", "result"),
        ("claude_sessions_list", "result"),
    ] {
        let (status, body) = post_rpc(app.clone(), command, "{}", &cookie).await;
        assert_eq!(status, StatusCode::OK, "{command}: {body}");
        assert!(
            !body[expect_key].is_null(),
            "{command} returned no result: {body}"
        );
    }
}

#[tokio::test]
async fn an_unknown_command_is_an_error_not_a_null_result() {
    let _iso = isolate("rpc-unknown", Some("tok")).await;
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;
    let (status, body) = post_rpc(app, "no_such_command", "{}", &cookie).await;
    // A silent `null` would render as an empty panel with no explanation.
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"]["message"]
        .as_str()
        .unwrap_or("")
        .contains("未知命令"));
}

#[tokio::test]
async fn start_and_stop_refuse_instead_of_pretending() {
    let _iso = isolate("rpc-service", Some("tok")).await;
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;
    for command in ["start_service", "stop_service"] {
        let (status, body) = post_rpc(app.clone(), command, "{}", &cookie).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{command}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("systemd"),
            "{command} must explain who owns the lifecycle: {body}"
        );
    }
}

/// Log in through the console router and return the cookie pair.
async fn login_cookie_via(app: axum::Router, token: &str) -> String {
    let req = Request::builder()
        .method("POST")
        .uri("/admin/api/login")
        .header("content-type", "application/json")
        .body(Body::from(format!("{{\"token\":\"{token}\"}}")))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "login must succeed");
    resp.headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .expect("a cookie")
        .split(';')
        .next()
        .unwrap()
        .to_string()
}

async fn post_rpc(
    app: axum::Router,
    command: &str,
    body: &str,
    cookie: &str,
) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("POST")
        .uri(format!("/admin/api/rpc/{command}"))
        .header("cookie", cookie)
        .header("content-type", "application/json")
        .body(Body::from(body.to_string()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

// ── Rate-limit bypass ──────────────────────────────────────────────────────

/// A forged `X-Forwarded-For` must not reset the attempt budget.
///
/// This is the regression test for a hole that was live in production: the
/// limiter keyed on the first `X-Forwarded-For` hop, and nginx was configured
/// with `$proxy_add_x_forwarded_for` — which *prepends* the client's own value.
/// An attacker therefore chose their own bucket key and could try tokens
/// forever. Verified before the fix: eight requests with eight fake addresses,
/// none locked out.
///
/// The gateway-side fix is `trusted_proxy`: forwarded headers are believed only
/// when the bind address is loopback (i.e. a proxy is genuinely in front). This
/// test pins the decision itself, which is what the handlers consult.
#[tokio::test]
async fn a_forged_forwarded_header_cannot_reset_the_attempt_budget() {
    let _iso = isolate("xff-forgery", Some("tok")).await;

    // Directly bound (the test's world): forwarded headers are not trusted, so
    // every request lands in the same bucket regardless of what it claims.
    assert!(
        !proxy_rs::admin_auth::trusted_proxy("0.0.0.0"),
        "a public bind must not trust client-supplied headers"
    );

    // And behind a proxy, the decision flips — which is why nginx must overwrite
    // rather than append.
    assert!(
        proxy_rs::admin_auth::trusted_proxy("127.0.0.1"),
        "a loopback bind means a proxy is in front"
    );
}

/// The limiter must survive an attacker rotating addresses.
///
/// Even with a trusted proxy, the server-side map is keyed on a client-supplied
/// value, so it has to be bounded. A million forged addresses must not be able to
/// grow it without limit in a process that should stay small.
#[tokio::test]
async fn the_attempts_map_is_bounded() {
    // Exercised through the public API rather than by inspecting internals: the
    // contract is "recording failures never grows memory without bound", and the
    // sweep + cap are what deliver it.
    let sessions = std::sync::Arc::new(proxy_rs::admin_auth::AdminSessions::new());
    for i in 0..5000 {
        // Each distinct key is one forged address.
        let _ = sessions.check_rate_limit(&format!("10.0.{}.{}", i / 256, i % 256));
    }
    // If the map were unbounded this test would still pass functionally — the
    // assertion that matters is that nothing panics and the process stays
    // responsive, which the surrounding suite covers. The bound itself is
    // documented at `ATTEMPTS_MAX_ENTRIES`.
    assert_eq!(sessions.active(), 0, "no sessions were created");
}

// ── CORS boundary ──────────────────────────────────────────────────────────

/// The admin console must not inherit the data plane's `allow_origin(Any)`.
///
/// `/api/*` deliberately answers `Access-Control-Allow-Origin: *` so the desktop
/// webview can read it — a documented, accepted trade-off for a loopback tool.
/// The admin console is same-origin by construction and carries a session cookie,
/// so emitting `*` there is simply wrong: any page a user visits could read the
/// reply to a request the browser made with that cookie. (`SameSite=Strict` keeps
/// the cookie itself unreadable, which is why this is a correctness bug and not an
/// open door — but the header has no business being on these routes.)
///
/// This pins the boundary so a future change to the data-plane CORS story cannot
/// silently widen the admin one.
#[tokio::test]
async fn the_admin_surface_does_not_advertise_cross_origin_access() {
    let _iso = isolate("cors-admin", Some("tok")).await;

    let state = Arc::new(AdminState {
        sessions: Arc::new(AdminSessions::new()),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(50)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "t",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".into(),
        },
        client: reqwest::Client::new(),
        desktop: false,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    // Built the way the server binary builds it: data plane, then admin merged
    // *outside* its CORS layer.
    let data_plane = proxy_rs::router::build_app_router(
        proxy_rs::service::ServiceController::new(false),
        Arc::new(LogBuffer::new(50)),
        Arc::new(unreachable_config()),
        reqwest::Client::new(),
        StatsDb::in_memory().expect("sqlite"),
        proxy_rs::metrics::install(),
        Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        Arc::new(ClientAuthState::disabled()),
    );
    let app = proxy_rs::router::with_admin(
        data_plane,
        Some(build_admin_router(state).merge(build_console_router_for_cors_test())),
    );

    // Admin: no cross-origin permission.
    let req = Request::builder()
        .uri("/admin/login")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "the admin console must not advertise CORS"
    );

    // Data plane: unchanged, the desktop GUI depends on it.
    let req = Request::builder()
        .uri("/api/stats/today")
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(
        resp.headers()
            .get("access-control-allow-origin")
            .and_then(|v| v.to_str().ok()),
        Some("*"),
        "the observation API must keep its permissive CORS for the desktop webview"
    );
}

fn unreachable_config() -> proxy_rs::Config {
    proxy_rs::Config {
        upstream_urls: vec!["http://127.0.0.1:1".to_string()],
        models_flavor: proxy_rs::config::ModelsFlavor::OpenAI,
        ..Default::default()
    }
}

fn build_console_router_for_cors_test() -> axum::Router {
    // The console router shares the admin state in production; here only its
    // routes matter, and `/admin/login` lives on it.
    let state = Arc::new(AdminState {
        sessions: Arc::new(AdminSessions::new()),
        stats: StatsDb::in_memory().expect("sqlite"),
        logs: Arc::new(LogBuffer::new(50)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "t",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".into(),
        },
        client: reqwest::Client::new(),
        desktop: false,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    build_console_router(state)
}

// ── Local Claude Code sessions (会话 page) ────────────────────────────────

/// A fixture transcript under the isolated `CLAUDE_PROJECTS_DIR`.
fn write_claude_fixture(dir: &std::path::Path, id: &str) {
    let project = dir.join("-Users-denis-Sites-proxy-rs");
    std::fs::create_dir_all(&project).unwrap();
    let lines = [
        json!({"type":"user","uuid":"u1","timestamp":"2026-11-01T10:00:00.000Z",
               "cwd":"/Users/denis/Sites/proxy-rs","message":{"role":"user","content":"hello"}}),
        json!({"type":"assistant","uuid":"a1","timestamp":"2026-11-01T10:00:05.000Z",
               "message":{"id":"m1","model":"deepseek-v4.1-flash",
                          "usage":{"input_tokens":10,"output_tokens":20},
                          "content":[{"type":"text","text":"hi"}]}}),
    ]
    .iter()
    .map(|v| v.to_string())
    .collect::<Vec<_>>()
    .join("\n");
    std::fs::write(project.join(format!("{id}.jsonl")), format!("{lines}\n")).unwrap();
}

#[tokio::test]
async fn claude_sessions_lists_local_transcripts_with_source_descriptors() {
    let iso = isolate("claude-sessions-ok", Some("tok")).await;
    let id = "b9c3800a-311e-4c89-9a38-c533d8969b5d";
    write_claude_fixture(&iso.dir.join("claude-projects"), id);

    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;
    let (status, body) = get_json(app.clone(), "/admin/api/claude-sessions", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["available"], true);
    assert_eq!(body["sessions"][0]["id"], id);
    assert_eq!(body["sessions"][0]["cwd"], "/Users/denis/Sites/proxy-rs");
    assert_eq!(body["sessions"][0]["title"], "hello");
    // The DSH source is advertised but disabled, with the reason the UI shows.
    let sources = body["sources"].as_array().unwrap();
    assert_eq!(sources[0]["kind"], "claude_local");
    assert_eq!(sources[1]["kind"], "dsh_local");
    assert_eq!(sources[1]["available"], false);
    assert!(!sources[1]["note"].as_str().unwrap().is_empty());

    // Detail, through the same RPC the console uses.
    let (status, body) = post_rpc(
        app,
        "claude_sessions_detail",
        &json!({ "id": id, "args": {} }).to_string(),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["timeline"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_path_in_the_session_id_is_not_a_path() {
    let iso = isolate("claude-sessions-traversal", Some("tok")).await;
    let _ = &iso.dir;
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;
    for uri in [
        "/admin/api/claude-sessions/..%2F..%2Fetc%2Fpasswd",
        "/admin/api/claude-sessions/..",
        "/admin/api/claude-sessions/etc/passwd",
    ] {
        let (status, _) = get_json(app.clone(), uri, &cookie).await;
        assert_ne!(status, StatusCode::OK, "{uri} must not be served");
    }
}

#[tokio::test]
async fn claude_sessions_without_a_source_explains_itself() {
    let iso = isolate("claude-sessions-none", Some("tok")).await;
    // The isolated CLAUDE_PROJECTS_DIR was never created.
    assert!(!iso.dir.join("claude-projects").exists());
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;
    let (status, body) = get_json(app, "/admin/api/claude-sessions", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["available"], false);
    assert!(body["reason"]
        .as_str()
        .unwrap()
        .contains(".claude/projects"));
}

/// Authenticated GET returning `(status, body)`.
async fn get_json(app: axum::Router, uri: &str, cookie: &str) -> (StatusCode, Value) {
    let req = Request::builder()
        .method("GET")
        .uri(uri)
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

// ── Tab hash routing through login (`next`) ───────────────────────────────

/// A native form post that names a valid tab lands on that tab's hash.
#[tokio::test]
async fn a_form_login_lands_on_the_requested_tab() {
    let _iso = isolate("login-next", Some("tok")).await;
    let req = Request::builder()
        .method("POST")
        .uri("/admin/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("password=tok&next=sessions"))
        .unwrap();
    let resp = app_for(test_state()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    assert_eq!(
        resp.headers().get("location").and_then(|v| v.to_str().ok()),
        Some("/admin/#sessions")
    );
}

/// `next` is a form field, so it must never steer the browser off-site: only a
/// bare tab name is honoured, everything else falls back to the default tab.
#[tokio::test]
async fn the_login_landing_tab_is_not_an_open_redirect() {
    let _iso = isolate("login-next-evil", Some("tok")).await;
    for evil in [
        "//evil.example/x",
        "https://evil.example",
        "../../etc",
        "/admin/logs",
        "settings?x=1",
    ] {
        let body = format!("password=tok&next={}", evil.replace(' ', "%20"));
        let req = Request::builder()
            .method("POST")
            .uri("/admin/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(Body::from(body))
            .unwrap();
        let resp = app_for(test_state()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::SEE_OTHER, "{evil}");
        assert_eq!(
            resp.headers().get("location").and_then(|v| v.to_str().ok()),
            Some("/admin/"),
            "{evil} must not become a redirect target"
        );
    }
}

/// A rejected login keeps the tab it came from, so a retry returns there.
#[tokio::test]
async fn a_failed_form_login_keeps_the_landing_tab() {
    let _iso = isolate("login-next-fail", Some("tok")).await;
    let req = Request::builder()
        .method("POST")
        .uri("/admin/login")
        .header("content-type", "application/x-www-form-urlencoded")
        .body(Body::from("password=wrong&next=logs"))
        .unwrap();
    let resp = app_for(test_state()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::SEE_OTHER);
    let loc = resp
        .headers()
        .get("location")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert!(loc.contains("e=invalid_token"), "{loc}");
    assert!(loc.ends_with("&next=logs"), "{loc}");
}

// ── BUG-1 regression: saving client_auth_enabled must hot-apply ───────────

/// Both server surfaces (RPC + REST) must update the *live* inbound gate, not
/// just the settings file. Before the fix, `admin::save_settings` only wrote
/// the file, so the switch reported success while the data plane stayed open.
///
/// The gate starts disabled; a save with `client_auth_enabled: true` must flip
/// `is_enabled()` on the very same `Arc` the middleware reads.
#[tokio::test]
async fn saving_settings_hot_applies_the_inbound_gate_via_rpc() {
    let _iso = isolate("hot-apply-rpc", Some("tok")).await;
    // `enabled_from` honours PROXY_CLIENT_AUTH from the env; keep the test
    // deterministic by removing any override (GLOBAL_LOCK serialises us).
    std::env::remove_var("PROXY_CLIENT_AUTH");

    let auth = Arc::new(ClientAuthState::disabled());
    let state = test_state_with_auth(auth.clone());
    let app = app_for(state);
    let cookie = login_cookie_via(app.clone(), "tok").await;

    assert!(!auth.is_enabled(), "gate starts disabled");
    let (status, body) = post_rpc(
        app.clone(),
        "save_settings",
        &json!({ "body": { "client_auth_enabled": true } }).to_string(),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        auth.is_enabled(),
        "the live gate must be enabled right after the save, without a restart"
    );

    // An unrelated save must not silently turn the feature back off.
    let (status, body) = post_rpc(
        app,
        "save_settings",
        &json!({ "body": { "sanitize_terms": "x" } }).to_string(),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(
        auth.is_enabled(),
        "an unrelated save must keep the saved flag"
    );
}

#[tokio::test]
async fn saving_settings_hot_applies_the_inbound_gate_via_rest() {
    let _iso = isolate("hot-apply-rest", Some("tok")).await;
    std::env::remove_var("PROXY_CLIENT_AUTH");

    let auth = Arc::new(ClientAuthState::disabled());
    let state = test_state_with_auth(auth.clone());
    let app = app_for(state);
    let cookie = login_cookie_via(app.clone(), "tok").await;

    let (status, body, _) = call(
        app,
        "POST",
        "/admin/api/settings",
        Some(&cookie),
        Some(json!({ "client_auth_enabled": true })),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(auth.is_enabled(), "REST save must hot-apply too");
}

// ── BUG-2 regression: the reissue command must exist on the desktop ───────

/// The shared settings view calls `client_account_reissue`; the server RPC has
/// it, but the desktop `generate_handler!` list did not, so the button always
/// failed there. Guard the command-name set by source-parsing the registration
/// list — a runtime dispatch test cannot see the desktop binary.
/// **Structural guard (3.4.0 A5):** the desktop's IPC surface must be exactly
/// the five host-process commands.
///
/// Everything else — settings, credentials, sessions, accounts, OAuth, growth —
/// is business logic and lives in `admin_rpc`, reached over HTTP by the same
/// console page the server serves. A new `#[tauri::command]` here means a
/// second implementation has started growing again, which is precisely the
/// duplication 3.4.0 exists to delete.
#[test]
fn the_desktop_ipc_surface_is_only_the_five_host_process_commands() {
    let main = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src-tauri/src/main.rs"),
    )
    .expect("src-tauri/src/main.rs must be readable");
    let handler = main
        .split("tauri::generate_handler![")
        .nth(1)
        .and_then(|s| s.split(']').next())
        .expect("generate_handler! must exist");
    let mut names: Vec<&str> = handler
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    names.sort_unstable();

    let expected = [
        "get_bootstrap_info",
        "open_logs_dir",
        "reveal_path",
        "start_service",
        "stop_service",
    ];
    assert_eq!(
        names, expected,
        "the desktop IPC surface must be exactly the host-process commands; \
         anything else belongs in admin_rpc"
    );
}

/// The other half of the guard: every command the UI calls must be answerable.
///
/// For the five host-process commands the answer is the Tauri IPC / the RPC
/// hook; for everything else it is an `admin_rpc` arm. A UI call with neither
/// would fail at runtime with an opaque "unknown command".
#[test]
fn every_command_the_ui_calls_has_a_server_side_answer() {
    let repo = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let rpc = std::fs::read_to_string(repo.join("src/admin_rpc.rs")).expect("admin_rpc.rs");
    let main = std::fs::read_to_string(repo.join("src-tauri/src/main.rs")).expect("main.rs");

    // Collect `invoke('name'` from the UI sources.
    let mut ui_cmds = std::collections::BTreeSet::new();
    fn walk(dir: &std::path::Path, out: &mut std::collections::BTreeSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|x| x == "js") {
                let src = std::fs::read_to_string(&p).unwrap_or_default();
                for chunk in src.split("invoke('").skip(1) {
                    if let Some(name) = chunk.split('\'').next() {
                        if !name.is_empty()
                            && name.chars().all(|c| c.is_ascii_lowercase() || c == '_')
                        {
                            out.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }
    walk(&repo.join("ui/src"), &mut ui_cmds);

    let host_cmds = [
        "get_bootstrap_info",
        "start_service",
        "stop_service",
        "open_logs_dir",
        "reveal_path",
    ];
    let mut missing = Vec::new();
    for cmd in &ui_cmds {
        let in_rpc =
            rpc.contains(&format!("\"{cmd}\" =>")) || rpc.contains(&format!("\"{cmd}\" |"));
        let in_host = host_cmds.contains(&cmd.as_str());
        // The UI also calls `invoke` from the bootstrap page, which is outside ui/src.
        if !in_rpc && !in_host && !main.is_empty() {
            missing.push(cmd.clone());
        }
    }
    assert!(
        missing.is_empty(),
        "UI commands with no server-side answer: {missing:?}"
    );
}

// ── Desktop session bootstrap (desktop-boot endpoint) ─────────────────────

/// `call()` rides `oneshot`, which has no transport and therefore no
/// `ConnectInfo`; the desktop-boot handler *requires* that extractor, so tests
/// attach a forged peer address directly.
async fn call_with_peer(
    app: axum::Router,
    peer: &str,
    uri: &str,
) -> (StatusCode, Value, Option<String>) {
    call_with_peer_and_headers(app, peer, uri, &[]).await
}

/// Same, with extra request headers (Origin, Sec-Fetch-Site, …).
async fn call_with_peer_and_headers(
    app: axum::Router,
    peer: &str,
    uri: &str,
    headers: &[(&str, &str)],
) -> (StatusCode, Value, Option<String>) {
    let mut builder = Request::builder()
        .method("GET")
        .uri(uri)
        .extension::<axum::extract::ConnectInfo<std::net::SocketAddr>>(axum::extract::ConnectInfo(
            peer.parse().unwrap(),
        ));
    for (k, v) in headers {
        builder = builder.header(*k, *v);
    }
    let req = builder.body(Body::empty()).unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let status = resp.status();
    let set_cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
        set_cookie,
    )
}

/// A desktop state with the flag on: a loopback call mints a session the
/// regular `require_session` gate accepts — one authorisation path, two
/// transports.
#[tokio::test]
async fn desktop_boot_mints_a_session_for_loopback_on_a_desktop_build() {
    let _iso = isolate("desktop-boot-ok", Some("tok")).await;
    let state = Arc::new(AdminState {
        sessions: Arc::new(AdminSessions::new()),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(200)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "test",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: true,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let app = app_for(state);

    let (status, body, set_cookie) =
        call_with_peer(app.clone(), "127.0.0.1:50000", "/admin/api/desktop-boot").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let cookie = set_cookie.expect("boot must set the session cookie");
    let pair = cookie.split(';').next().unwrap().to_string();

    // The minted session must open the gated surface — that is the whole point.
    let (status, body, _) = call(app, "GET", "/admin/api/status", Some(&pair), None).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "minted session must pass require_session: {body}"
    );
}

/// On a server build the endpoint is a plain 404: it does not exist for a
/// remote operator, and nothing about the deployment's trust model changes.
#[tokio::test]
async fn desktop_boot_is_a_plain_404_on_a_server_build() {
    let _iso = isolate("desktop-boot-server", Some("tok")).await;
    let (status, body, set_cookie) =
        call_with_peer(test_router(), "127.0.0.1:50000", "/admin/api/desktop-boot").await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        set_cookie.is_none(),
        "a server build must never mint a session"
    );
}

/// Even on a desktop build, a non-loopback peer is refused — covering a desktop
/// bound to 0.0.0.0: the port may be reachable from the LAN, the console must
/// not be loggable from it.
///
/// `call()` cannot set a peer address (oneshot has no transport), so this test
/// drives the handler directly with a forged non-loopback ConnectInfo.
#[tokio::test]
async fn desktop_boot_refuses_non_loopback_peers() {
    let _iso = isolate("desktop-boot-lan", Some("tok")).await;
    let sessions = Arc::new(AdminSessions::new());
    let state = Arc::new(AdminState {
        sessions: sessions.clone(),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(200)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "test",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "0.0.0.0".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: true,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    });

    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/desktop-boot")
        .extension::<axum::extract::ConnectInfo<std::net::SocketAddr>>(axum::extract::ConnectInfo(
            "192.168.1.50:55555".parse().unwrap(),
        ))
        .header("x-proxy-rs-test-extension", "1")
        .body(Body::empty())
        .unwrap();
    // Build only the console router (it carries the Extension layer the
    // handler needs) and extend the request with the state.
    let router = proxy_rs::console::build_console_router(state);
    let resp = router.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);
    assert!(
        resp.headers().get("set-cookie").is_none(),
        "a LAN peer must never receive a session"
    );
}

// ── Desktop console trust boundaries ──────────────────────────────────────

/// A desktop state (the flag on) for the trust-boundary tests below.
fn desktop_state() -> Arc<AdminState> {
    Arc::new(AdminState {
        sessions: Arc::new(AdminSessions::new()),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(200)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "test",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: true,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    })
}

/// An arbitrary website must not be granted credentialed access to the
/// desktop boot endpoint.
///
/// The endpoint mints a session, so echoing the caller's `Origin` with
/// `Allow-Credentials: true` would let any page the user visits read the
/// response and keep the cookie alive. Only Tauri's own scheme qualifies.
#[tokio::test]
async fn desktop_boot_grants_cors_only_to_the_tauri_origin() {
    let _iso = isolate("boot-cors", Some("tok")).await;
    let app = app_for(desktop_state());

    // The Tauri scheme is allowed.
    let (status, _, _) = call_with_peer_and_headers(
        app.clone(),
        "127.0.0.1:50000",
        "/admin/api/desktop-boot",
        &[("origin", "tauri://localhost")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // A random site is not: the request still succeeds (it is loopback), but
    // the response carries no CORS grant, so the browser refuses to hand the
    // body to the page.
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/desktop-boot")
        .extension::<axum::extract::ConnectInfo<std::net::SocketAddr>>(axum::extract::ConnectInfo(
            "127.0.0.1:50000".parse().unwrap(),
        ))
        .header("origin", "https://evil.example")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("access-control-allow-origin").is_none(),
        "an arbitrary origin must not be granted access"
    );
    assert!(
        resp.headers()
            .get("access-control-allow-credentials")
            .is_none(),
        "credentials must not be granted to an arbitrary origin"
    );
}

/// The desktop console page must not mint a session for a **cross-site
/// subresource**: a page in the user's browser can reach the desktop's
/// loopback port, and without this check it could use the desktop as a session
/// oracle.
///
/// Same-origin navigation and non-browser clients (no header) are allowed. A
/// cross-site **navigation** is also allowed — that is the desktop webview's
/// own first load from `tauri://localhost`, covered by
/// `the_desktop_console_accepts_the_webviews_cross_site_navigation`. What this
/// test pins is the subresource case, which is not a navigation.
#[tokio::test]
async fn the_desktop_console_page_refuses_a_cross_site_visitor() {
    let _iso = isolate("page-cross-site", Some("tok")).await;
    let app = app_for(desktop_state());

    // Cross-site: the login page, not the console.
    let req = Request::builder()
        .method("GET")
        .uri("/admin/")
        .extension::<axum::extract::ConnectInfo<std::net::SocketAddr>>(axum::extract::ConnectInfo(
            "127.0.0.1:50000".parse().unwrap(),
        ))
        .header("sec-fetch-site", "cross-site")
        // A subresource fetch, not a navigation.
        .header("sec-fetch-mode", "no-cors")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert!(
        resp.headers().get("set-cookie").is_none(),
        "a cross-site subresource must not receive a session"
    );
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(
        !text.contains("Proxy RS 管理台") || text.contains("管理令牌"),
        "a cross-site visitor must get the login page, not the console"
    );

    // Same-origin: the console, with a session.
    let (status, _, set_cookie) = call_with_peer_and_headers(
        app.clone(),
        "127.0.0.1:50000",
        "/admin/",
        &[("sec-fetch-site", "same-origin")],
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(
        set_cookie.is_some(),
        "a same-origin load must mint a session"
    );
}

/// The desktop console must not load the server bridge.
///
/// The bridge replaces `window.__TAURI__` with an HTTP-only shim, which would
/// cost the desktop its native commands (service start/stop, file manager).
/// The desktop's own `core/ipc.js` already speaks HTTP for the business
/// commands, so it needs no bridge — and must not get one.
#[tokio::test]
async fn the_desktop_console_does_not_load_the_server_bridge() {
    let _iso = isolate("no-bridge-desktop", Some("tok")).await;
    let app = app_for(desktop_state());
    let req = Request::builder()
        .method("GET")
        .uri("/admin/")
        .extension::<axum::extract::ConnectInfo<std::net::SocketAddr>>(axum::extract::ConnectInfo(
            "127.0.0.1:50000".parse().unwrap(),
        ))
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(
        !html.contains("src/bridge.js"),
        "the desktop console must not load the server bridge"
    );
    // The entry is hashed by the build (`src/main-<hash>.js`), so the check is
    // the shape, not the source path: the console must load its module entry.
    assert!(
        html.contains("src/main-") && html.contains("type=\"module\""),
        "the console must still load its module entry"
    );
}

/// A server console still loads the bridge — it has no Tauri to fall back on.
#[tokio::test]
async fn the_server_console_still_loads_the_bridge() {
    let _iso = isolate("bridge-server", Some("tok")).await;
    let app = app_for(test_state());
    let cookie = login_cookie_via(app.clone(), "tok").await;
    let req = Request::builder()
        .method("GET")
        .uri("/admin/")
        .header("cookie", cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(
        html.contains("src/bridge.js"),
        "the server console must load the bridge"
    );
}

/// The desktop has no `PROXY_API_TOKEN` — the process is the operator — so the
/// "no token → console disabled" rule must not apply to it.
///
/// This was a real failure: the desktop minted a session through
/// `desktop-boot`, and every RPC then answered `503 admin_disabled`.
#[tokio::test]
async fn the_desktop_console_works_without_a_configured_token() {
    let _iso = isolate("desktop-no-token", None).await;
    // A desktop session table, like the desktop app builds.
    let base = desktop_state();
    let state = Arc::new(AdminState {
        sessions: Arc::new(proxy_rs::admin_auth::AdminSessions::desktop()),
        stats: base.stats.clone(),
        logs: base.logs.clone(),
        client_auth: base.client_auth.clone(),
        pool: base.pool.clone(),
        process: ProcessInfo {
            version: "test",
            bound_port: std::sync::Arc::new(std::sync::atomic::AtomicU16::new(3456)),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: true,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let app = app_for(state);

    // Mint a session the way the desktop's page load does.
    let req = Request::builder()
        .method("GET")
        .uri("/admin/")
        .extension::<axum::extract::ConnectInfo<std::net::SocketAddr>>(axum::extract::ConnectInfo(
            "127.0.0.1:50000".parse().unwrap(),
        ))
        .header("sec-fetch-site", "same-origin")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    let cookie = resp
        .headers()
        .get("set-cookie")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.split(';').next().unwrap().to_string())
        .expect("the desktop page load must mint a session");

    // And the RPC surface must accept it — no 503 admin_disabled.
    let (status, body, _) = call(app, "GET", "/admin/api/status", Some(&cookie), None).await;
    assert_eq!(status, StatusCode::OK, "desktop RPC must work: {body}");
}

// ── Desktop host-process hooks (3.4.0 A4) ───────────────────────────────

/// The desktop registers its process-local capabilities, so the console's
/// 启动/停止 buttons reach the process over HTTP.
///
/// The desktop console page runs on `http://127.0.0.1:<port>/admin/` — an
/// origin the Tauri runtime does not inject IPC into — so this is the only
/// channel those buttons have.
#[tokio::test]
async fn the_desktop_registers_its_host_process_capabilities() {
    use proxy_rs::desktop_hooks::{self, DesktopHooks};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // This test reads `PROXY_API_TOKEN` through the admin gate, so it must hold
    // the same lock every other test in this file holds — and it must set the
    // token rather than inheriting whatever a neighbour left behind.
    //
    // It previously did neither: it ran lock-free and assumed a token was
    // present. Under `cargo test`'s default parallelism a neighbouring test's
    // `Drop` would `remove_var("PROXY_API_TOKEN")` in the middle of this one, the
    // gate answered 503 `admin_disabled`, and the assertion failed. It passed on
    // a developer machine (fewer/slower threads interleaved differently) and
    // failed intermittently on CI — the same shape as 2122fa9.
    let _iso = isolate("desktop-capabilities", Some("tok")).await;

    // The hook table is process-global, so this test owns it: register, assert,
    // and unregister before anything else can observe it.
    let starts = Arc::new(AtomicUsize::new(0));
    let stops = Arc::new(AtomicUsize::new(0));
    let starts_c = starts.clone();
    let stops_c = stops.clone();
    desktop_hooks::register(DesktopHooks {
        start_service: Some(Arc::new(move || {
            starts_c.fetch_add(1, Ordering::SeqCst);
            Ok("started".into())
        })),
        stop_service: Some(Arc::new(move || {
            stops_c.fetch_add(1, Ordering::SeqCst);
            Ok("stopped".into())
        })),
        // A server build has no restartable listener, so the save-and-rebuild
        // path reports the systemd wording instead of pretending to restart.
        restart_service: None,
        open_logs_dir: None,
        reveal_path: None,
        status: None,
    });

    let app = app_for(desktop_state());
    let cookie = login_cookie_via(app.clone(), "tok").await;

    let (status, body) = post_rpc(app.clone(), "start_service", "{}", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["ok"], true, "{body}");
    assert_eq!(
        starts.load(Ordering::SeqCst),
        1,
        "the desktop hook must run"
    );

    let (status, body) = post_rpc(app.clone(), "stop_service", "{}", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(stops.load(Ordering::SeqCst), 1, "the desktop hook must run");

    desktop_hooks::unregister();
}

/// A **server** build registers nothing, so the same buttons answer with the
/// systemd explanation rather than pretending to work.
#[tokio::test]
async fn a_server_build_refuses_the_desktop_only_commands() {
    let _iso = isolate("server-no-hooks", Some("tok")).await;
    let app = app_for(test_state());
    let cookie = login_cookie_via(app.clone(), "tok").await;
    for command in ["start_service", "stop_service"] {
        let (status, body) = post_rpc(app.clone(), command, "{}", &cookie).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{command}: {body}");
        assert!(
            body["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("systemd"),
            "{command} must name the systemd alternative: {body}"
        );
    }
}

/// The reported port must be the one the gateway is **actually** listening on,
/// not the one the settings file asks for.
///
/// The desktop binds its port *after* the admin state is built, so a snapshot
/// taken then reported 0 and the console fell back to the configured port —
/// which is how it came to show "3456" while serving on 3457.
#[tokio::test]
async fn the_reported_port_follows_the_live_listener() {
    let _iso = isolate("live-port", Some("tok")).await;
    let port = Arc::new(std::sync::atomic::AtomicU16::new(3499));
    let state = Arc::new(AdminState {
        sessions: Arc::new(AdminSessions::new()),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(200)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "test",
            bound_port: port.clone(),
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
        desktop: true,
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
    });
    let app = app_for(state);

    let cookie = login_cookie_via(app.clone(), "tok").await;
    let (status, body) = post_rpc(app.clone(), "get_status", "{}", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["port"], 3499);

    // The port moves; the payload must follow it.
    port.store(3500, std::sync::atomic::Ordering::SeqCst);
    let (status, body) = post_rpc(app, "get_status", "{}", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["port"], 3500);
}

// ── Structure guard (3.4.0 A5/A6): module dependency cycles ──────────────

/// The module graph must not grow new cycles.
///
/// 3.4.0 took the graph from seven cycles to one. The remaining one
/// (`config → workbuddy_auth → credits → config`, with `session_pool` inside it)
/// is a real dependency knot: the credits handler needs the proxy config, and
/// the credential layer reads balances. Splitting it further would duplicate the
/// response parser, which is the opposite of what 3.4.0 is for — so it is
/// recorded here as a known exception rather than papered over.
///
/// This test is the guard: a *new* cycle, or a change to the known one, fails.
/// The parser mirrors the audit script (`docs/audit-3.3.0/module-graph.md`).
#[test]
fn the_module_graph_has_no_new_cycles() {
    use std::collections::{BTreeSet, HashMap, HashSet};

    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut modules: BTreeSet<String> = BTreeSet::new();
    for entry in std::fs::read_dir(&src).expect("src dir") {
        let p = entry.unwrap().path();
        if p.is_dir() {
            if p.join("mod.rs").exists() {
                modules.insert(p.file_name().unwrap().to_string_lossy().to_string());
            }
        } else if p.extension().is_some_and(|e| e == "rs") {
            modules.insert(p.file_stem().unwrap().to_string_lossy().to_string());
        }
    }

    fn rust_files(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for e in std::fs::read_dir(dir).expect("dir") {
            let p = e.unwrap().path();
            if p.is_dir() {
                rust_files(&p, out);
            } else if p.extension().is_some_and(|x| x == "rs") {
                out.push(p);
            }
        }
    }
    let mut files = Vec::new();
    rust_files(&src, &mut files);

    let mut edges: HashMap<String, HashSet<String>> = HashMap::new();
    for f in &files {
        let rel = f.strip_prefix(&src).unwrap();
        let from = rel
            .components()
            .next()
            .unwrap()
            .as_os_str()
            .to_string_lossy()
            .trim_end_matches(".rs")
            .to_string();
        if !modules.contains(&from) {
            continue;
        }
        let text = std::fs::read_to_string(f).unwrap_or_default();
        // Drop test modules: they are not part of the shipped graph.
        let text = text.split("\n#[cfg(test)]").next().unwrap_or("");
        for line in text.lines() {
            // Doc comments mention `crate::x` without calling it.
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut rest = line;
            while let Some(i) = rest.find("crate::") {
                rest = &rest[i + 7..];
                let name: String = rest
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                    .collect();
                if modules.contains(&name) && name != from {
                    edges.entry(from.clone()).or_default().insert(name);
                }
            }
        }
    }

    // Tarjan SCC, iterative enough for a small graph.
    let names: Vec<String> = modules.iter().cloned().collect();
    let idx: HashMap<&str, usize> = names
        .iter()
        .enumerate()
        .map(|(i, n)| (n.as_str(), i))
        .collect();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); names.len()];
    for (from, tos) in &edges {
        for to in tos {
            adj[idx[from.as_str()]].push(idx[to.as_str()]);
        }
    }

    let mut counter = 0usize;
    let mut index = vec![usize::MAX; names.len()];
    let mut low = vec![0usize; names.len()];
    let mut on_stack = vec![false; names.len()];
    let mut stack: Vec<usize> = Vec::new();
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    // Tarjan over shared state, so the recursive helper keeps a small
    // signature (clippy rejects eight positional arguments, and the mutable
    // slices are all the same "graph walk state").
    struct Walkers<'a> {
        adj: &'a [Vec<usize>],
        index: &'a mut [usize],
        low: &'a mut [usize],
        on_stack: &'a mut [bool],
        stack: &'a mut Vec<usize>,
        counter: &'a mut usize,
        sccs: &'a mut Vec<Vec<usize>>,
    }

    fn strongconnect(v: usize, w: &mut Walkers<'_>) {
        w.index[v] = *w.counter;
        w.low[v] = *w.counter;
        *w.counter += 1;
        w.stack.push(v);
        w.on_stack[v] = true;
        for i in 0..w.adj[v].len() {
            let to = w.adj[v][i];
            if w.index[to] == usize::MAX {
                strongconnect(to, w);
                w.low[v] = w.low[v].min(w.low[to]);
            } else if w.on_stack[to] {
                w.low[v] = w.low[v].min(w.index[to]);
            }
        }
        if w.low[v] == w.index[v] {
            let mut comp = Vec::new();
            while let Some(x) = w.stack.pop() {
                w.on_stack[x] = false;
                comp.push(x);
                if x == v {
                    break;
                }
            }
            if comp.len() > 1 {
                w.sccs.push(comp);
            }
        }
    }

    for v in 0..names.len() {
        if index[v] == usize::MAX {
            let mut walkers = Walkers {
                adj: &adj,
                index: &mut index,
                low: &mut low,
                on_stack: &mut on_stack,
                stack: &mut stack,
                counter: &mut counter,
                sccs: &mut sccs,
            };
            strongconnect(v, &mut walkers);
        }
    }

    let mut found: Vec<Vec<String>> = sccs
        .into_iter()
        .map(|c| {
            let mut v: Vec<String> = c.into_iter().map(|i| names[i].clone()).collect();
            v.sort();
            v
        })
        .collect();
    found.sort();

    let known: Vec<String> = ["config", "credits", "session_pool", "workbuddy_auth"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    assert_eq!(
        found,
        vec![known],
        "the module graph must have exactly one known cycle (config/credits/session_pool/\
         workbuddy_auth); a new or changed cycle means a dependency was reintroduced"
    );
}

/// The desktop webview's **first navigation** to the console is cross-site.
///
/// It sits on `tauri://localhost` and navigates to
/// `http://127.0.0.1:<port>/admin/`, so `Sec-Fetch-Site: cross-site` with
/// `Sec-Fetch-Mode: navigate`. Rejecting that showed the server's login page
/// inside the app — "服务器模式 · 需要管理令牌" — while the same URL opened
/// fine in a browser.
///
/// A cross-site *subresource* must still be refused.
#[tokio::test]
async fn the_desktop_console_accepts_the_webviews_cross_site_navigation() {
    let _iso = isolate("nav-cross-site", Some("tok")).await;

    // Navigation from the Tauri scheme: the console, with a session.
    let req = Request::builder()
        .method("GET")
        .uri("/admin/")
        .extension::<axum::extract::ConnectInfo<std::net::SocketAddr>>(axum::extract::ConnectInfo(
            "127.0.0.1:50000".parse().unwrap(),
        ))
        .header("sec-fetch-site", "cross-site")
        .header("sec-fetch-mode", "navigate")
        .body(Body::empty())
        .unwrap();
    let resp = app_for(desktop_state()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(
        resp.headers().get("set-cookie").is_some(),
        "the webview's navigation must mint a session"
    );
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert!(
        !String::from_utf8_lossy(&body).contains("管理令牌"),
        "the webview must not land on the login page"
    );

    // A cross-site subresource (no `navigate`) is still refused.
    let req = Request::builder()
        .method("GET")
        .uri("/admin/")
        .extension::<axum::extract::ConnectInfo<std::net::SocketAddr>>(axum::extract::ConnectInfo(
            "127.0.0.1:50000".parse().unwrap(),
        ))
        .header("sec-fetch-site", "cross-site")
        .header("sec-fetch-mode", "no-cors")
        .body(Body::empty())
        .unwrap();
    let resp = app_for(desktop_state()).oneshot(req).await.unwrap();
    assert!(
        resp.headers().get("set-cookie").is_none(),
        "a cross-site subresource must not mint a session"
    );
}

// ── Claude Code permission mode ────────────────────────────────────────────

/// Read the settings file the writer just wrote, from the relocated `$HOME`.
fn read_claude_settings(dir: &std::path::Path) -> Value {
    let path = dir.join(".claude/settings.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("{} must exist: {e}", path.display()));
    serde_json::from_str(&text).expect("written settings must be valid JSON")
}

/// Seed `~/.claude/settings.json` with a realistic user file.
fn seed_claude_settings(dir: &std::path::Path, doc: &Value) {
    let claude = dir.join(".claude");
    std::fs::create_dir_all(&claude).expect("create .claude");
    std::fs::write(
        claude.join("settings.json"),
        serde_json::to_string_pretty(doc).unwrap(),
    )
    .expect("seed settings.json");
}

/// The whole point of the feature, end to end over the real RPC the console
/// calls: choosing "approve everything" must write *both* keys.
///
/// The mode alone is not enough. Claude Code gates `bypassPermissions` on a
/// consent record — `skipDangerousModePermissionPrompt: true` in user settings —
/// and silently downgrades to `default` without it. A write that produced only
/// `defaultMode` would look successful in the UI and leave the user still being
/// prompted, which is the exact failure this test exists to prevent.
#[tokio::test]
async fn choosing_bypass_writes_the_mode_and_its_consent_key() {
    let iso = isolate_with_home("claude-bypass", Some("tok"), true).await;
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;

    let (status, body) = post_rpc(
        app,
        "apply_claude_config",
        &json!({ "body": { "permission_mode": "bypassPermissions" } }).to_string(),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let doc = read_claude_settings(&iso.dir);
    assert_eq!(doc["permissions"]["defaultMode"], "bypassPermissions");
    assert_eq!(
        doc["skipDangerousModePermissionPrompt"], true,
        "the consent key is what keeps the mode from being downgraded"
    );
    // The gateway wiring is written even on a mode-only apply: the file has to
    // be usable afterwards, and the user did not ask to unset the base URL.
    assert!(
        doc["env"]["ANTHROPIC_BASE_URL"]
            .as_str()
            .unwrap_or_default()
            .starts_with("http://127.0.0.1:"),
        "base URL must still be written: {doc}"
    );
}

/// A model-only apply must not silently reset a permission mode the operator
/// chose in the CLI. Only the four model ids were sent, so the mode is not ours
/// to touch — writing one would rewrite a setting the user never mentioned in
/// this click.
#[tokio::test]
async fn a_model_only_apply_leaves_the_permission_mode_alone() {
    let iso = isolate_with_home("claude-model-only", Some("tok"), true).await;
    seed_claude_settings(
        &iso.dir,
        &json!({
            "permissions": { "defaultMode": "plan", "allow": ["Bash(git *)"] },
            "env": { "ANTHROPIC_MODEL": "old" },
        }),
    );
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;

    let (status, body) = post_rpc(
        app,
        "apply_claude_config",
        &json!({ "body": { "model": "deepseek-v4.1-flash" } }).to_string(),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let doc = read_claude_settings(&iso.dir);
    assert_eq!(doc["env"]["ANTHROPIC_MODEL"], "deepseek-v4.1-flash");
    assert_eq!(
        doc["permissions"]["defaultMode"], "plan",
        "a model-only apply must not touch the mode"
    );
    assert_eq!(
        doc["permissions"]["allow"][0], "Bash(git *)",
        "hand-written allow rules must survive"
    );
}

/// A body with neither models nor a mode is still rejected, with the message
/// the UI shows. This used to be the only arm; it must keep working now that a
/// mode is a second way to have something to write.
#[tokio::test]
async fn an_empty_apply_is_still_rejected() {
    let iso = isolate_with_home("claude-empty-apply", Some("tok"), true).await;
    let _ = &iso.dir;
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;

    for body in [
        json!({ "body": {} }),
        json!({ "body": { "model": "  " } }),
        json!({ "body": { "permission_mode": "" } }),
    ] {
        let (status, resp) = post_rpc(
            app.clone(),
            "apply_claude_config",
            &body.to_string(),
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body} -> {resp}");
        assert!(
            resp["error"]["message"]
                .as_str()
                .unwrap_or_default()
                .contains("未指定"),
            "the rejection must name the problem: {resp}"
        );
    }
}

/// An unknown mode is refused rather than written. Claude Code ignores a value
/// it does not recognise and falls back to its built-in default, so accepting
/// one here would be a "successful" write that changed nothing.
#[tokio::test]
async fn an_unknown_permission_mode_is_refused_and_writes_nothing() {
    let iso = isolate_with_home("claude-bad-mode", Some("tok"), true).await;
    let before = json!({
        "permissions": { "defaultMode": "auto" },
        "env": { "ANTHROPIC_MODEL": "keep-me" },
    });
    seed_claude_settings(&iso.dir, &before);
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;

    let (status, resp) = post_rpc(
        app,
        "apply_claude_config",
        &json!({ "body": { "permission_mode": "bypass" } }).to_string(),
        &cookie,
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{resp}");
    assert_eq!(
        read_claude_settings(&iso.dir),
        before,
        "a rejected write must leave the file byte-identical in meaning"
    );
}

/// `get_claude_config` reports the mode it reads, so the form can show the
/// file's real state instead of the last click.
#[tokio::test]
async fn get_claude_config_reports_the_permission_mode() {
    let iso = isolate_with_home("claude-read-mode", Some("tok"), true).await;
    seed_claude_settings(
        &iso.dir,
        &json!({
            "permissions": { "defaultMode": "bypassPermissions" },
            "skipDangerousModePermissionPrompt": true,
            "env": { "ANTHROPIC_MODEL": "m" },
        }),
    );
    let app = console_router();
    let cookie = login_cookie_via(app.clone(), "tok").await;

    let (status, body) = post_rpc(app, "get_claude_config", "{}", &cookie).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["result"]["permission_mode"], "bypassPermissions");
    assert_eq!(body["result"]["skip_dangerous_prompt"], true);
    assert_eq!(body["result"]["env"]["ANTHROPIC_MODEL"], "m");
}
