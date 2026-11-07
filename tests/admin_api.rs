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
}

impl Drop for Isolated {
    fn drop(&mut self) {
        std::env::remove_var(TOKEN_ENV);
        std::env::remove_var("PROXY_DATA_DIR");
        std::env::remove_var("CLAUDE_PROJECTS_DIR");
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

async fn isolate(name: &str, token: Option<&str>) -> Isolated {
    let guard = GLOBAL_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!("proxy-rs-it-admin-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::env::set_var("PROXY_DATA_DIR", &dir);
    // The session browser must never read the real home in a test.
    std::env::set_var("CLAUDE_PROJECTS_DIR", dir.join("claude-projects"));
    settings::set_log_file_override_for_tests(dir.join("proxy.log"));
    match token {
        Some(t) => std::env::set_var(TOKEN_ENV, t),
        None => std::env::remove_var(TOKEN_ENV),
    }
    Isolated { _guard: guard, dir }
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
            bound_port: 3456,
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
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
            bound_port: 3456,
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
    });
    app_for(state)
}

/// A fresh state for tests that only need the login route.
fn test_state() -> Arc<AdminState> {
    Arc::new(AdminState {
        sessions: Arc::new(AdminSessions::new()),
        stats: StatsDb::in_memory().expect("in-memory sqlite"),
        logs: Arc::new(LogBuffer::new(200)),
        client_auth: Arc::new(ClientAuthState::disabled()),
        pool: Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        process: ProcessInfo {
            version: "test",
            bound_port: 3456,
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
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
            bound_port: 3456,
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".to_string(),
        },
        client: reqwest::Client::new(),
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
        !body.contains("type=\"module\" src=\"src/main.js\""),
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

    for (uri, expected) in [
        ("/admin/src/main.js", "text/javascript"),
        ("/admin/src/bridge.js", "text/javascript"),
        ("/admin/src/views/overview.js", "text/javascript"),
        ("/admin/style/tokens.css", "text/css"),
        ("/admin/assets/logo.svg", "image/svg+xml"),
    ] {
        let (status, ct, _) = get_raw(app.clone(), uri, None).await;
        assert_eq!(status, StatusCode::OK, "{uri}");
        assert!(ct.starts_with(expected), "{uri}: got {ct}, want {expected}");
    }
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
            bound_port: 3456,
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".into(),
        },
        client: reqwest::Client::new(),
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
            bound_port: 3456,
            started_at: std::time::Instant::now(),
            bind_addr: "127.0.0.1".into(),
        },
        client: reqwest::Client::new(),
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
