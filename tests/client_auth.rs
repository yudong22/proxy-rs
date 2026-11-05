//! End-to-end checks for the inbound access-key gate (`x-proxy-rs-key`).
//!
//! Driven through the real [`build_app_router`] rather than the middleware
//! function directly, because the failures worth catching here are wiring
//! failures: a route that forgot the gate, a gate that leaked into `/health`,
//! and — most importantly — the gate interfering with the **upstream**
//! passthrough path it must never touch.
//!
//! # The two-key rule
//!
//! `x-api-key` / `authorization` stay owned by the upstream credential path;
//! the gateway key travels on `x-proxy-rs-key`. The decisive test here is
//! [`both_keys_travel_their_own_directions`]: it proves a request can carry both
//! headers, that the upstream still receives the client's `x-api-key`
//! untouched, and that the gateway key never appears in the outbound request.
//!
//! # Serialization
//!
//! The data-directory and log-file overrides are process-global, so these tests
//! hold a shared mutex for their whole body.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use proxy_rs::{
    client_accounts::{self, ClientAuthState, KEY_HEADER},
    config::{Config, ModelsFlavor},
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

/// Serialises every test, which repoints process-global paths.
static GLOBAL_LOCK: Mutex<()> = Mutex::const_new(());

/// Point the data dir and the log file at a temp directory for this test.
struct Isolated {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
}

impl Drop for Isolated {
    fn drop(&mut self) {
        std::env::remove_var("PROXY_DATA_DIR");
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

async fn isolate(name: &str) -> Isolated {
    let guard = GLOBAL_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!(
        "proxy-rs-it-clientauth-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::env::set_var("PROXY_DATA_DIR", &dir);
    settings::set_log_file_override_for_tests(dir.join("proxy.log"));
    Isolated { _guard: guard, dir }
}

/// Build a router with the gate in the given state.
fn router_with(state: Arc<ClientAuthState>, config: Config) -> axum::Router {
    let ctrl = ServiceController::new(false);
    // Marked running so a request that passes the gate reaches the forwarding
    // code (and the mock upstream) instead of the 503 path.
    ctrl.mark_running();
    build_app_router(
        ctrl,
        Arc::new(LogBuffer::new(200)),
        Arc::new(config),
        reqwest::Client::new(),
        StatsDb::in_memory().expect("in-memory sqlite"),
        metrics::install(),
        Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
        state,
    )
}

/// A minimal Anthropic body; the upstream is a mock, so only the shape matters.
fn messages_body() -> Body {
    Body::from(
        serde_json::json!({
            "model": "test-model",
            "max_tokens": 16,
            "messages": [{"role": "user", "content": "hi"}]
        })
        .to_string(),
    )
}

/// POST `/v1/messages` with the given extra headers.
async fn post_messages(app: axum::Router, headers: &[(&str, &str)]) -> (StatusCode, Value) {
    let mut builder = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json");
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    let response = app
        .oneshot(builder.body(messages_body()).expect("request builds"))
        .await
        .expect("router responds");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    (
        status,
        serde_json::from_slice(&bytes).unwrap_or(Value::Null),
    )
}

/// A router whose upstream points at a closed port, so a request that passes the
/// gate fails as a *gateway* error (502) rather than as an auth error. That is
/// enough to distinguish "gate let it through" from "gate refused it".
fn unreachable_upstream_config() -> Config {
    Config {
        upstream_urls: vec!["http://127.0.0.1:1".to_string()],
        models_flavor: ModelsFlavor::OpenAI,
        ..Default::default()
    }
}

// ── The gate is off ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_disabled_gate_lets_every_request_through() {
    let _iso = isolate("disabled").await;
    let app = router_with(
        Arc::new(ClientAuthState::disabled()),
        unreachable_upstream_config(),
    );

    // No key at all. This is the upgrade-safety case: an existing CLI config
    // that knows nothing about `x-proxy-rs-key` must keep working.
    let (status, _) = post_messages(app, &[]).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "the gate is off, so a missing key must not be refused"
    );
}

// ── The gate is on ─────────────────────────────────────────────────────────

#[tokio::test]
async fn a_missing_key_is_refused_with_an_authentication_error() {
    let _iso = isolate("missing").await;
    let state = Arc::new(ClientAuthState::new(true));
    let app = router_with(state, unreachable_upstream_config());

    let (status, body) = post_messages(app, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // The *shape* matters as much as the status: `proxy_error` would be read by
    // Claude Code / Codex as a gateway fault and retried forever.
    assert_eq!(body["type"], "error");
    assert_eq!(body["error"]["type"], "authentication_error");
    assert_eq!(body["error"]["code"], "missing_key");
}

#[tokio::test]
async fn a_wrong_key_is_refused_but_a_right_one_passes() {
    let _iso = isolate("wrong-right").await;
    let (_account, key) = client_accounts::create_account("ci", "").expect("create");
    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;

    let app = router_with(state.clone(), unreachable_upstream_config());
    let (status, body) = post_messages(app, &[(KEY_HEADER, "prs_wrong")]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(body["error"]["code"], "invalid_key");

    // The correct key gets past the gate; the mock upstream is unreachable, so a
    // 502 proves forwarding was attempted. A 401 here would mean the gate.
    let app = router_with(state, unreachable_upstream_config());
    let (status, _) = post_messages(app, &[(KEY_HEADER, &key)]).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "a valid key must pass the gate"
    );
}

#[tokio::test]
async fn a_disabled_account_is_refused_with_its_own_code() {
    let _iso = isolate("account-disabled").await;
    let (account, key) = client_accounts::create_account("old", "").expect("create");
    client_accounts::set_enabled(&account.id, false).expect("disable");

    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;
    let app = router_with(state, unreachable_upstream_config());

    let (status, body) = post_messages(app, &[(KEY_HEADER, &key)]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Correct key, switched-off account: the code must say so rather than
    // sending the user hunting for a typo.
    assert_eq!(body["error"]["code"], "account_disabled");
}

#[tokio::test]
async fn an_enabled_gate_with_no_accounts_fails_closed() {
    let _iso = isolate("empty-store").await;
    let state = Arc::new(ClientAuthState::new(true));
    // Store is empty on purpose: "on but configured with nothing" must never
    // degrade into "open to everyone".
    let app = router_with(state, unreachable_upstream_config());

    let (status, _) = post_messages(app, &[]).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn health_is_open_even_when_the_gate_is_on() {
    let _iso = isolate("health").await;
    let state = Arc::new(ClientAuthState::new(true));
    let app = router_with(state, unreachable_upstream_config());

    // A reverse proxy and uptime probe must reach this without a key.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);
}

#[tokio::test]
async fn the_observation_api_is_not_gated() {
    let _iso = isolate("api-open").await;
    let state = Arc::new(ClientAuthState::new(true));
    let app = router_with(state, unreachable_upstream_config());

    // The desktop console reads these over loopback with `fetch`; gating them
    // would break the GUI on every machine. The public edge hides them instead.
    let response = app
        .oneshot(
            Request::builder()
                .uri("/api/stats/today")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router responds");
    assert_ne!(
        response.status(),
        StatusCode::UNAUTHORIZED,
        "/api/* must not be behind the inbound gate"
    );
}

#[tokio::test]
async fn a_disabled_account_takes_effect_without_a_restart() {
    let _iso = isolate("hot-reload").await;
    let (account, key) = client_accounts::create_account("live", "").expect("create");
    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;

    let app = router_with(state.clone(), unreachable_upstream_config());
    let (status, _) = post_messages(app, &[(KEY_HEADER, &key)]).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "key works before disabling"
    );

    // Disable through the same path the GUI/HTTP commands use, then reload —
    // this is the whole point of holding the accounts in memory.
    client_accounts::set_enabled(&account.id, false).expect("disable");
    state.reload_from_store().await;

    let app = router_with(state, unreachable_upstream_config());
    let (status, _) = post_messages(app, &[(KEY_HEADER, &key)]).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "disabling must apply to the next request, not the next restart"
    );
}

// ── The two keys must not interfere (the decisive test) ────────────────────

/// A mock upstream that records the request headers it received.
async fn mock_upstream() -> (String, Arc<std::sync::Mutex<Vec<axum::http::HeaderMap>>>) {
    use axum::{extract::State, routing::post, Router};

    let captured: Arc<std::sync::Mutex<Vec<axum::http::HeaderMap>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = captured.clone();

    let app = Router::new()
        .route(
            "/v1/chat/completions",
            post(
                |State(sink): State<Arc<std::sync::Mutex<Vec<axum::http::HeaderMap>>>>,
                 headers: axum::http::HeaderMap| async move {
                    sink.lock().expect("capture lock").push(headers);
                    axum::Json(serde_json::json!({
                        "id": "chatcmpl-test",
                        "object": "chat.completion",
                        "model": "test-model",
                        "choices": [{
                            "index": 0,
                            "message": {"role": "assistant", "content": "ok"},
                            "finish_reason": "stop"
                        }],
                        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
                    }))
                },
            ),
        )
        .with_state(sink);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind mock");
    let addr = listener.local_addr().expect("addr");
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{addr}"), captured)
}

/// The rule this release is built around, in one test.
///
/// `UPSTREAM_API_KEY_PASSTHROUGH` + the inbound gate switched on together:
///
///   * the client's `x-api-key` must reach the upstream **unchanged**;
///   * the gateway key must **not** appear in the outbound request at all;
///   * and the request must still authenticate at the door.
///
/// If this ever fails, the two credential systems have started interfering and
/// either an upstream key is being replaced by a gateway key (breaking the
/// upstream) or a gateway key is being leaked upstream (a credential escape).
#[tokio::test]
async fn both_keys_travel_their_own_directions() {
    let _iso = isolate("two-keys").await;
    let (base_url, captured) = mock_upstream().await;

    let (_account, gateway_key) = client_accounts::create_account("laptop", "").expect("create");
    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;

    let config = Config {
        upstream_urls: vec![base_url],
        // The upstream credential comes from the request, exactly as before.
        passthrough_api_key: true,
        // WorkBuddy is the flavour where `x-api-key` is *mirrored* onto the
        // outbound request alongside `Authorization`, so this exercises the
        // strongest form of the rule: two auth headers on the wire, both of
        // which must still carry the client's upstream key and neither of which
        // may carry the gateway key.
        models_flavor: ModelsFlavor::WorkBuddyConfig,
        // Both features on at once. No mutual exclusion anywhere.
        client_auth_enabled: true,
        ..Default::default()
    };

    let app = router_with(state, config);
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .header("x-api-key", "upstream-key-abc")
                .header(KEY_HEADER, &gateway_key)
                .body(Body::from(
                    serde_json::json!({
                        "model": "test-model",
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .expect("request builds"),
        )
        .await
        .expect("router responds");

    assert_eq!(
        response.status(),
        StatusCode::OK,
        "both features on at once must still serve the request"
    );

    let requests = captured.lock().expect("capture lock").clone();
    assert_eq!(requests.len(), 1, "exactly one upstream call");
    let outbound = &requests[0];

    // 1. The upstream key is the client's, unchanged — on both headers it is
    //    allowed to occupy.
    let outbound_api_key = outbound
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        outbound_api_key, "upstream-key-abc",
        "the client's upstream key must reach the upstream untouched on x-api-key"
    );
    let outbound_auth = outbound
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        outbound_auth, "Bearer upstream-key-abc",
        "the client's upstream key must reach the upstream untouched on authorization"
    );

    // 2. The gateway key is nowhere in the outbound headers — neither on its own
    //    header nor mirrored into an auth header.
    assert!(
        outbound.get(KEY_HEADER).is_none(),
        "the gateway access key must never be forwarded upstream"
    );
    for value in outbound.values() {
        if let Ok(text) = value.to_str() {
            assert!(
                !text.contains(&gateway_key),
                "the gateway key leaked into an outbound header: {text}"
            );
        }
    }
}
