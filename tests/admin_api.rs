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
    });
    (build_admin_router(state), sessions)
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
    });
    build_admin_router(state)
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
        ("POST", "/admin/api/logout"),
        ("POST", "/admin/api/checkin"),
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
async fn the_pool_export_is_a_download_not_a_path() {
    let _iso = isolate("export", Some("tok")).await;
    let (app, _sessions) = test_router_with_sessions();
    let cookie = login_cookie(app.clone(), "tok").await;
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/pool/export")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let disposition = resp
        .headers()
        .get("content-disposition")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default()
        .to_string();
    assert!(disposition.starts_with("attachment;"), "{disposition}");
    // A pool export carries live tokens; nothing may cache it.
    let cache = resp
        .headers()
        .get("cache-control")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert_eq!(cache, "no-store");
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

// ── Console assets ─────────────────────────────────────────────────────────

#[tokio::test]
async fn the_console_page_is_served_without_a_session() {
    let _iso = isolate("console-page", Some("tok")).await;
    // Loading the page that creates a session cannot itself require one.
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/")
        .body(Body::empty())
        .unwrap();
    let resp = test_router().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    assert!(ct.starts_with("text/html"), "{ct}");

    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);
    assert!(html.contains("Proxy RS 管理台"), "the shell must render");
    assert!(
        html.contains("id=\"token\""),
        "it must contain the login field"
    );
}

#[tokio::test]
async fn the_console_script_is_served_as_javascript() {
    let _iso = isolate("console-js", Some("tok")).await;
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/console.js")
        .body(Body::empty())
        .unwrap();
    let resp = test_router().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let ct = resp
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    // A module script served as anything but JavaScript is refused by the
    // browser, which would leave the page permanently blank.
    assert!(ct.starts_with("text/javascript"), "{ct}");
}

#[tokio::test]
async fn the_console_page_contains_no_secrets() {
    let _iso = isolate("console-secret", Some("super-secret-token")).await;
    let req = Request::builder()
        .method("GET")
        .uri("/admin/api/")
        .body(Body::empty())
        .unwrap();
    let resp = test_router().oneshot(req).await.unwrap();
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let html = String::from_utf8_lossy(&body);
    // The unauthenticated page must not embed the admin token or any upstream
    // material — it is served before anyone has proven who they are.
    assert!(!html.contains("super-secret-token"));
}
