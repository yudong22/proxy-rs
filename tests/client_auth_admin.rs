//! End-to-end checks for the client-account **admin** routes.
//!
//! These are the only `/api` routes that can mint a credential unlocking the
//! whole `/v1/*` data plane, so the interesting failures are policy failures:
//! a route reachable without `PROXY_API_TOKEN`, a browser `Origin` being
//! accepted, and a create response that leaks more than it should.
//!
//! The read routes deliberately stay open (the desktop console fetches them over
//! loopback); these four do not, and the tests below are what keeps that
//! distinction from silently eroding.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use proxy_rs::{
    client_accounts::ClientAuthState,
    config::Config,
    metrics,
    router::build_app_router,
    service::ServiceController,
    settings::{self, LogBuffer},
    stats::StatsDb,
};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;

static GLOBAL_LOCK: Mutex<()> = Mutex::const_new(());

struct Isolated {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
}

impl Drop for Isolated {
    fn drop(&mut self) {
        std::env::remove_var("PROXY_DATA_DIR");
        std::env::remove_var("PROXY_API_TOKEN");
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

/// Isolate the data dir and set (or clear) the admin token.
async fn isolate(name: &str, token: Option<&str>) -> Isolated {
    let guard = GLOBAL_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!("proxy-rs-it-admin-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::env::set_var("PROXY_DATA_DIR", &dir);
    settings::set_log_file_override_for_tests(dir.join("proxy.log"));
    // `PROXY_API_TOKEN` is looked up in `.env` *before* the process environment,
    // so clearing the variable below would still leave a developer's `.env`
    // token in force and turn the "mutation disabled" cases into 401s.
    proxy_rs::env::set_dotenv_override_for_tests(None);
    match token {
        Some(value) => std::env::set_var("PROXY_API_TOKEN", value),
        None => std::env::remove_var("PROXY_API_TOKEN"),
    }
    Isolated { _guard: guard, dir }
}

fn test_router(state: Arc<ClientAuthState>) -> axum::Router {
    build_app_router(
        ServiceController::new(false),
        Arc::new(LogBuffer::new(100)),
        Arc::new(Config::default()),
        reqwest::Client::new(),
        StatsDb::in_memory().expect("in-memory sqlite"),
        metrics::install(),
        Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        state,
    )
}

/// Send one request and return `(status, json body)`.
async fn send(
    app: axum::Router,
    method: &str,
    uri: &str,
    headers: &[(&str, &str)],
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method(method).uri(uri);
    let mut has_content_type = false;
    for (name, value) in headers {
        if *name == "content-type" {
            has_content_type = true;
        }
        builder = builder.header(*name, *value);
    }
    let request = match body {
        Some(value) => {
            if !has_content_type {
                builder = builder.header("content-type", "application/json");
            }
            builder
                .body(Body::from(value.to_string()))
                .expect("request builds")
        }
        None => builder.body(Body::empty()).expect("request builds"),
    };
    let response = app.oneshot(request).await.expect("router responds");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

fn admin(token: &str) -> Vec<(&str, &str)> {
    // A `Vec` of borrowed pairs would outlive the token; leak a static string
    // instead so the helper can stay simple.
    let auth: &'static str = Box::leak(format!("Bearer {token}").into_boxed_str());
    vec![("authorization", auth)]
}

// ── The gate ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn without_a_configured_token_every_admin_route_is_disabled() {
    let _iso = isolate("no-token", None).await;
    let app = test_router(Arc::new(ClientAuthState::new(true)));

    // `403 mutation_disabled`, not 401: the route is switched off entirely, and
    // an unauthenticated caller should learn nothing about what it would accept.
    let (status, body) = send(app, "GET", "/api/admin/client-accounts", &[], None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "mutation_disabled");
}

#[tokio::test]
async fn a_wrong_token_is_unauthorized() {
    let _iso = isolate("bad-token", Some("the-real-token")).await;
    let app = test_router(Arc::new(ClientAuthState::new(true)));

    let (status, body) = send(
        app,
        "GET",
        "/api/admin/client-accounts",
        &admin("a-different-token"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["type"], "unauthorized");
}

#[tokio::test]
async fn a_browser_origin_is_refused_even_with_the_right_token() {
    let _iso = isolate("origin", Some("tok")).await;
    let app = test_router(Arc::new(ClientAuthState::new(true)));

    // CORS is `allow_origin(Any)`, so a browser can *send* this. The `Origin`
    // header is therefore the check that matters: a page the user visits must
    // not be able to mint itself a gateway key.
    let mut headers = admin("tok");
    headers.push(("origin", "https://evil.example"));
    let (status, body) = send(app, "GET", "/api/admin/client-accounts", &headers, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["type"], "browser_origin");
}

// ── CRUD ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn create_returns_the_key_once_and_never_again() {
    let _iso = isolate("create", Some("tok")).await;
    let state = Arc::new(ClientAuthState::new(true));
    let app = test_router(state.clone());

    let (status, body) = send(
        app.clone(),
        "POST",
        "/api/admin/client-accounts",
        &admin("tok"),
        Some(serde_json::json!({"label": "笔记本"})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let api_key = body["api_key"]
        .as_str()
        .expect("api_key present")
        .to_string();
    assert!(api_key.starts_with("prs_"));
    assert_eq!(body["header"], "x-proxy-rs-key");
    assert_eq!(body["account"]["label"], "笔记本");

    // The list must never carry the key or the hash.
    let (status, list) = send(
        app.clone(),
        "GET",
        "/api/admin/client-accounts",
        &admin("tok"),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(list["total"], 1);
    let text = list.to_string();
    assert!(!text.contains(&api_key), "the key must not be re-readable");
    assert!(
        !text.contains("key_hash"),
        "the hash is credential material and must not be served"
    );

    // And the created key actually unlocks the data plane.
    assert!(state.verify(&api_key).await.is_ok());
}

#[tokio::test]
async fn an_account_created_over_http_works_immediately() {
    let _iso = isolate("create-hot", Some("tok")).await;
    let state = Arc::new(ClientAuthState::new(true));
    let app = test_router(state.clone());

    let (_, body) = send(
        app,
        "POST",
        "/api/admin/client-accounts",
        &admin("tok"),
        Some(serde_json::json!({"label": "ci"})),
    )
    .await;
    let api_key = body["api_key"].as_str().unwrap().to_string();

    // No restart, no explicit reload call from the test: the handler reloads.
    assert!(
        state.verify(&api_key).await.is_ok(),
        "a created key must be usable on the next request"
    );
}

#[tokio::test]
async fn update_and_delete_take_effect_without_a_restart() {
    let _iso = isolate("update-delete", Some("tok")).await;
    let state = Arc::new(ClientAuthState::new(true));
    let app = test_router(state.clone());

    let (_, created) = send(
        app.clone(),
        "POST",
        "/api/admin/client-accounts",
        &admin("tok"),
        Some(serde_json::json!({"label": "old"})),
    )
    .await;
    let id = created["account"]["id"].as_str().unwrap().to_string();
    let api_key = created["api_key"].as_str().unwrap().to_string();

    // Disable.
    let uri = format!("/api/admin/client-accounts/{id}");
    let (status, _) = send(
        app.clone(),
        "PATCH",
        &uri,
        &admin("tok"),
        Some(serde_json::json!({"enabled": false})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state.verify(&api_key).await,
        Err(proxy_rs::client_accounts::VerifyFailure::Disabled),
        "disabling must apply immediately"
    );

    // Re-enable and delete.
    let (status, _) = send(
        app.clone(),
        "PATCH",
        &uri,
        &admin("tok"),
        Some(serde_json::json!({"enabled": true})),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(state.verify(&api_key).await.is_ok());

    let (status, _) = send(app.clone(), "DELETE", &uri, &admin("tok"), None).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        state.verify(&api_key).await,
        Err(proxy_rs::client_accounts::VerifyFailure::Unknown)
    );

    // Deleting again is a 404, not a silent success.
    let (status, body) = send(app, "DELETE", &uri, &admin("tok"), None).await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "unknown_account");
}

#[tokio::test]
async fn updating_an_unknown_account_is_a_404() {
    let _iso = isolate("unknown", Some("tok")).await;
    let app = test_router(Arc::new(ClientAuthState::new(true)));

    let (status, body) = send(
        app,
        "PATCH",
        "/api/admin/client-accounts/ca-000000",
        &admin("tok"),
        Some(serde_json::json!({"label": "x"})),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    assert_eq!(body["error"]["type"], "unknown_account");
}
