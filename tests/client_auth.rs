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

// ── Standard client headers work with only a key + base URL ────────────────

/// The usability requirement: an off-the-shelf client configured with just a
/// base URL and an API key must work, without knowing about `x-proxy-rs-key`.
#[tokio::test]
async fn the_key_works_in_authorization_bearer() {
    let _iso = isolate("std-auth").await;
    let (_account, key) = client_accounts::create_account("client", "").expect("create");
    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;
    let app = router_with(state, unreachable_upstream_config());

    // This is what `ANTHROPIC_AUTH_TOKEN` / OpenAI SDK `api_key` produce.
    let (status, _) = post_messages(app, &[("authorization", &format!("Bearer {key}"))]).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "a gateway key in `Authorization: Bearer` must authenticate"
    );
}

#[tokio::test]
async fn the_key_works_in_x_api_key() {
    let _iso = isolate("std-xapikey").await;
    let (_account, key) = client_accounts::create_account("client", "").expect("create");
    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;
    let app = router_with(state, unreachable_upstream_config());

    // Claude Code's `ANTHROPIC_API_KEY` lands here.
    let (status, _) = post_messages(app, &[("x-api-key", &key)]).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "a gateway key in `x-api-key` must authenticate"
    );
}

#[tokio::test]
async fn the_explicit_header_still_wins() {
    let _iso = isolate("explicit-wins").await;
    let (_account, key) = client_accounts::create_account("client", "").expect("create");
    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;
    let app = router_with(state, unreachable_upstream_config());

    // Both present: the dedicated header is authoritative and the standard one
    // is simply left for the upstream path (here it is junk, which is fine —
    // the gate is satisfied by the explicit header).
    let (status, _) = post_messages(
        app,
        &[
            (KEY_HEADER, &key),
            ("authorization", "Bearer some-upstream-key"),
        ],
    )
    .await;
    assert_ne!(status, StatusCode::UNAUTHORIZED);
}

/// The safety direction: an upstream key in a standard header must be **left
/// alone** by the gate, not swallowed.
///
/// This is the case that would silently break passthrough deployments if the
/// header check were loose: the gate would consume the upstream key, decide it
/// is not a gateway key, and either refuse the request or forward it with no
/// credential at all.
#[tokio::test]
async fn an_upstream_key_in_a_standard_header_is_left_for_passthrough() {
    let _iso = isolate("passthrough-intact").await;
    let (base_url, captured) = mock_upstream().await;

    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;

    let config = Config {
        upstream_urls: vec![base_url],
        passthrough_api_key: true,
        models_flavor: ModelsFlavor::WorkBuddyConfig,
        client_auth_enabled: true,
        ..Default::default()
    };
    let app = router_with(state.clone(), config);

    // Only an upstream key, no gateway key. With no gateway accounts at all,
    // nothing can match, so this must be refused — and crucially the upstream
    // key must NOT be the reason it is stored or consumed.
    let (status, _) = post_messages(app, &[("x-api-key", "sk-upstream-only")]).await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "an upstream key alone is not a gateway credential"
    );
    assert!(
        captured.lock().expect("lock").is_empty(),
        "nothing may reach the upstream on an unauthenticated request"
    );
}

/// A gateway key presented in a standard header is consumed, so it must not be
/// forwarded upstream — the same rule as the dedicated header.
///
/// This is the leak that would matter if the standard-header support were added
/// carelessly: a user pasting their gateway key into `x-api-key` (the natural
/// thing to do) would otherwise have that key forwarded to the upstream.
#[tokio::test]
async fn a_gateway_key_in_a_standard_header_is_not_forwarded_upstream() {
    let _iso = isolate("std-no-leak").await;
    let (base_url, captured) = mock_upstream().await;
    let (_account, gateway_key) = client_accounts::create_account("client", "").expect("create");
    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;

    // A static upstream key is configured, so the upstream has a credential of
    // its own and does not depend on the client's header.
    let config = Config {
        upstream_urls: vec![base_url],
        api_key: Some("sk-static-upstream".to_string()),
        models_flavor: ModelsFlavor::WorkBuddyConfig,
        client_auth_enabled: true,
        ..Default::default()
    };
    let app = router_with(state, config);

    let (status, _) = post_messages(app, &[("x-api-key", &gateway_key)]).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "the gateway key authenticates"
    );

    let requests = captured.lock().expect("capture lock").clone();
    assert_eq!(requests.len(), 1);
    for value in requests[0].values() {
        if let Ok(text) = value.to_str() {
            assert!(
                !text.contains(&gateway_key),
                "the gateway key leaked upstream via a standard header: {text}"
            );
        }
    }
    // The upstream still got its own configured static key.
    assert_eq!(
        requests[0]
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .unwrap_or(""),
        "Bearer sk-static-upstream"
    );
}

/// **The deployment shape this release is designed for.**
///
/// A client is configured with nothing but a base URL and an API key. That key
/// is a *gateway* key: it proves the caller may use this gateway, and nothing
/// else. The upstream is authenticated by the **credential pool**, which is
/// entirely independent of what the client sent.
///
/// Asserts three things at once:
///   1. a standard header alone is enough to get in;
///   2. the upstream credential is the pool's account token, not the client's;
///   3. the gateway key appears nowhere in the outbound request.
#[tokio::test]
async fn a_gateway_key_authenticates_here_while_the_pool_authenticates_upstream() {
    let _iso = isolate("pool-upstream").await;
    let (base_url, captured) = mock_upstream().await;

    let (_account, gateway_key) = client_accounts::create_account("我们家", "").expect("create");
    let state = Arc::new(ClientAuthState::new(true));
    state.reload_from_store().await;

    // The upstream credential lives in the pool — never in the client request.
    let pool = proxy_rs::session_pool::CredentialPool::new(vec![pool_credential()]);
    let config = Config {
        upstream_urls: vec![base_url],
        models_flavor: ModelsFlavor::WorkBuddyConfig,
        credential_pool_enabled: true,
        client_auth_enabled: true,
        // Passthrough OFF: the upstream key comes from the pool, which is the
        // normal shape for this deployment.
        passthrough_api_key: false,
        ..Default::default()
    };

    let app = build_app_router(
        {
            let ctrl = ServiceController::new(false);
            ctrl.mark_running();
            ctrl
        },
        Arc::new(LogBuffer::new(200)),
        Arc::new(config),
        reqwest::Client::new(),
        StatsDb::in_memory().expect("in-memory sqlite"),
        metrics::install(),
        Arc::new(pool),
        state.clone(),
    );

    // Only the gateway key, in a standard header — the "paste key + host" setup.
    let (status, _) = post_messages(app, &[("x-api-key", &gateway_key)]).await;
    assert_ne!(
        status,
        StatusCode::UNAUTHORIZED,
        "a standard header carrying a gateway key must authenticate"
    );

    let requests = captured.lock().expect("capture lock").clone();
    assert_eq!(requests.len(), 1, "the request reached the upstream");
    let outbound = &requests[0];

    // 1. Upstream credential is the pool's account token.
    let outbound_auth = outbound
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    assert_eq!(
        outbound_auth, "Bearer account-access-token",
        "the upstream must be authenticated by the credential pool"
    );

    // 2. The gateway key is nowhere outbound — not on its own header, not in any
    //    header value, not as the upstream credential.
    assert!(outbound.get(KEY_HEADER).is_none());
    for value in outbound.values() {
        if let Ok(text) = value.to_str() {
            assert!(
                !text.contains(&gateway_key),
                "the gateway key must never be forwarded upstream: {text}"
            );
        }
    }
}

/// A pool credential, taking it from the existing helper's shape.
///
/// `needless_update` would fire if every field were listed; they are not — the
/// credential struct carries cooldown/points/error bookkeeping this test has no
/// opinion about — so the update is meaningful and the lint is wrong here.
#[allow(clippy::needless_update)]
fn pool_credential() -> proxy_rs::workbuddy_auth::WorkBuddyCredential {
    proxy_rs::workbuddy_auth::WorkBuddyCredential {
        id: "wb-abc123".to_string(),
        label: "test account".to_string(),
        access_token: "account-access-token".to_string(),
        refresh_token: None,
        expires_at_ms: None,
        domain: "www.codebuddy.cn".to_string(),
        account: proxy_rs::workbuddy_auth::WorkBuddyAccount {
            uid: "uid-1".to_string(),
            enterprise_id: "ent-1".to_string(),
            nickname: "test".to_string(),
            ..Default::default()
        },
        machine_id: "machine-abc".to_string(),
        enabled: true,
        // Several credential fields (cooldown, last_error, points, …) are not
        // listed above; this keeps the test resilient to the struct growing.
        ..Default::default()
    }
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
