//! End-to-end regression: an account-pool request must authenticate with the
//! login token in `Authorization` **only**.
//!
//! Why this test exists
//! -------------------
//! The WorkBuddy gateway treats `x-api-key` as authoritative for chat
//! completions. A login-state credential is a Keycloak bearer, not an API key,
//! so sending its token in `x-api-key` alongside the correct
//! `Authorization: Bearer <token>` makes the gateway answer
//! `401 {"message":"not_found"}` even though the bearer is perfectly valid.
//!
//! `upstream_auth_headers` used to mirror the credential token into
//! `x-api-key` ("the token is the bearer *and* the api key"). Every
//! account-pool request therefore 401'd on the first attempt, the failover
//! logic relocated it to the API-key pool, and the request log recorded
//! `override=k-…` — which is why the account path looked like it had *never*
//! once succeeded. Verified against the live upstream (2026-09-28):
//!
//!   * `Authorization: Bearer <token>`                       → 200
//!   * `Authorization: Bearer <token>` + `x-api-key: <token>` → 401 not_found
//!
//! The helper-level unit tests could not catch this: they asserted the mirrored
//! `x-api-key` was present, encoding the bug as the contract. So this test drives
//! the real `build_app_router` against a real local upstream and asserts on the
//! bytes that actually reach the wire.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use proxy_rs::{
    config::{Config, ModelsFlavor},
    metrics,
    router::build_app_router,
    service::ServiceController,
    session_pool::CredentialPool,
    settings::{self, LogBuffer},
    stats::StatsDb,
    workbuddy_auth::{WorkBuddyAccount, WorkBuddyCredential},
};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tower::ServiceExt;

/// Serialises the tests in this file: the log destination is process-global and
/// each test redirects it.
static LOG_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// One captured upstream request: the header block exactly as received.
#[derive(Debug, Clone, Default)]
struct CapturedRequest {
    headers: Vec<(String, String)>,
}

impl CapturedRequest {
    /// Case-insensitive single-header lookup.
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    /// How many times a header name appears (reqwest's `.header()` appends).
    fn count(&self, name: &str) -> usize {
        self.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name))
            .count()
    }
}

/// A one-shot local upstream that records the request headers it receives and
/// answers with a valid non-streaming completion.
///
/// Returns the bound base URL and the shared capture slot. The accept loop runs
/// in the background and serves `expect` requests before returning.
async fn mock_upstream(expect: usize) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
    mock_upstream_with_body(expect, completion_body()).await
}

/// The OpenAI completion body the chat-path tests expect back.
fn completion_body() -> serde_json::Value {
    serde_json::json!({
        "id": "chatcmpl-test",
        "object": "chat.completion",
        "created": 1,
        "model": "glm-5.3-flash",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": "hi"},
            "finish_reason": "stop"
        }],
        "usage": {"prompt_tokens": 1, "completion_tokens": 1, "total_tokens": 2}
    })
}

/// As [`mock_upstream`], but replies with a caller-supplied body — used to serve
/// the billing-shape response the credits route parses.
async fn mock_upstream_with_body(
    expect: usize,
    response_body: serde_json::Value,
) -> (String, Arc<Mutex<Vec<CapturedRequest>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind mock");
    let addr = listener.local_addr().expect("mock addr");
    let captured = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&captured);

    tokio::spawn(async move {
        for _ in 0..expect {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };

            // Read until the end of the header block, then honour Content-Length
            // so the client's body write completes before we reply.
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let header_end = loop {
                let n = match socket.read(&mut chunk).await {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(_) => return,
                };
                buf.extend_from_slice(&chunk[..n]);
                if let Some(pos) = find_header_end(&buf) {
                    break pos;
                }
            };

            let head = String::from_utf8_lossy(&buf[..header_end]).to_string();
            let mut headers = Vec::new();
            for line in head.lines().skip(1) {
                if let Some((name, value)) = line.split_once(':') {
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                }
            }
            sink.lock()
                .expect("capture lock")
                .push(CapturedRequest { headers });

            let content_length: usize = head
                .lines()
                .find_map(|l| {
                    let (n, v) = l.split_once(':')?;
                    n.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse().ok())?
                })
                .unwrap_or(0);
            let already = buf.len().saturating_sub(header_end);
            let mut remaining = content_length.saturating_sub(already);
            while remaining > 0 {
                let n = match socket.read(&mut chunk).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(_) => break,
                };
                remaining = remaining.saturating_sub(n);
            }

            let body = response_body.to_string();

            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = socket.write_all(response.as_bytes()).await;
            let _ = socket.flush().await;
        }
    });

    (format!("http://{addr}"), captured)
}

/// Byte offset of the `\r\n\r\n` that terminates the header block.
fn find_header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Redirect log output to a temp file, returning the path and the held guard.
///
/// Async, not blocking: the guard is held across `await` points, so the lock
/// must be a `tokio::sync::Mutex` and acquired with `.await` — `blocking_lock`
/// panics inside the test's runtime.
async fn isolate_log(name: &str) -> (std::path::PathBuf, tokio::sync::MutexGuard<'static, ()>) {
    let guard = LOG_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!("proxy-rs-auth-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    settings::set_log_file_override_for_tests(path.clone());
    (path, guard)
}

fn test_credential() -> WorkBuddyCredential {
    WorkBuddyCredential {
        id: "wb-abc123".to_string(),
        label: "test account".to_string(),
        access_token: "account-access-token".to_string(),
        refresh_token: None,
        // No known expiry: usable, and no proactive refresh round-trip.
        expires_at_ms: None,
        domain: "www.codebuddy.cn".to_string(),
        account: WorkBuddyAccount {
            uid: "uid-1".to_string(),
            enterprise_id: "ent-1".to_string(),
            nickname: "test".to_string(),
        },
        machine_id: "machine-1".to_string(),
        enabled: true,
        ..Default::default()
    }
}

/// The account path, through the real router and a real upstream.
///
/// Asserts on the bytes that actually reach the wire, which the helper-level
/// unit tests could not do: they asserted the mirrored `x-api-key` was present,
/// encoding the bug as the contract.
#[tokio::test]
async fn account_request_authenticates_with_bearer_only() {
    let (_path, _guard) = isolate_log("account-bearer.log").await;

    let (base_url, captured) = mock_upstream(1).await;

    let credential = test_credential();
    let pool = Arc::new(CredentialPool::new(vec![credential.clone()]));

    let service = ServiceController::new(false);
    service.mark_running();
    let app = build_app_router(
        service,
        Arc::new(LogBuffer::new(200)),
        Arc::new(Config {
            upstream_urls: vec![base_url],
            api_key: Some("static-key".to_string()),
            models_flavor: ModelsFlavor::WorkBuddyConfig,
            credential_pool_enabled: true,
            session_switch_enabled: true,
            ..Default::default()
        }),
        reqwest::Client::new(),
        StatsDb::in_memory().expect("in-memory sqlite"),
        metrics::install(),
        Arc::clone(&pool),
    );

    let response = app.oneshot(chat_request()).await.expect("router responds");
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "an account-served request must succeed on the first attempt"
    );

    let requests = captured.lock().expect("capture lock").clone();
    assert_eq!(
        requests.len(),
        1,
        "the account request reached the upstream"
    );

    let account = &requests[0];
    assert_eq!(
        account.header("authorization"),
        Some("Bearer account-access-token"),
        "the account token must travel as the bearer; got {:?}",
        account.headers
    );
    assert_eq!(
        account.count("authorization"),
        1,
        "exactly one Authorization value on the wire"
    );
    assert_eq!(
        account.header("x-api-key"),
        None,
        "a login-state token must NOT be mirrored into x-api-key (this is the \
         401 not_found regression); got {:?}",
        account.header("x-api-key")
    );
    assert_eq!(account.count("x-api-key"), 0);
    // The account fingerprint still rides along, and the static key is absent.
    assert_eq!(account.header("x-user-id"), Some("uid-1"));
    assert_eq!(account.header("x-machine-id"), Some("machine-1"));
    assert_eq!(account.header("x-domain"), Some("www.codebuddy.cn"));
    assert_eq!(account.header("x-enterprise-id"), Some("ent-1"));
    assert!(
        !account
            .headers
            .iter()
            .any(|(_, v)| v.contains("static-key")),
        "the static key must not leak into an account request"
    );
}

/// A static-key request keeps the 1.7.x `Authorization` + `x-api-key` pair.
#[tokio::test]
async fn static_key_request_keeps_both_auth_headers() {
    let (_path, _guard) = isolate_log("static-key.log").await;
    let (base_url, captured) = mock_upstream(1).await;

    let service = ServiceController::new(false);
    service.mark_running();
    let app = build_app_router(
        service,
        Arc::new(LogBuffer::new(200)),
        Arc::new(Config {
            upstream_urls: vec![base_url],
            api_key: Some("static-key".to_string()),
            models_flavor: ModelsFlavor::WorkBuddyConfig,
            credential_pool_enabled: false,
            ..Default::default()
        }),
        reqwest::Client::new(),
        StatsDb::in_memory().expect("in-memory sqlite"),
        metrics::install(),
        Arc::new(CredentialPool::empty()),
    );

    let response = app.oneshot(chat_request()).await.expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);

    let requests = captured.lock().expect("capture lock").clone();
    assert_eq!(requests.len(), 1);
    let req = &requests[0];
    assert_eq!(req.header("authorization"), Some("Bearer static-key"));
    assert_eq!(req.header("x-api-key"), Some("static-key"));
    assert_eq!(req.count("authorization"), 1);
    assert_eq!(req.count("x-api-key"), 1);
}

fn chat_request() -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/chat/completions")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::json!({
                "model": "glm-5.3-flash",
                "messages": [{"role": "user", "content": "hi"}],
                "stream": false
            })
            .to_string(),
        ))
        .expect("request builds")
}

/// `GET /v1/credits` has the same login-token-is-not-an-api-key rule.
///
/// The handler picks the account pool's token when the pool is active. That
/// token used to be sent as both `Authorization` and `X-API-Key`, and the
/// billing gateway answers `401 {"message":"not_found"}` for that pairing — so
/// the 剩余积分 card showed "—" whenever the account pool was on. The static-key
/// path must keep the pair.
#[tokio::test]
async fn credits_request_sends_a_login_token_as_bearer_only() {
    let (_path, _guard) = isolate_log("credits-identity.log").await;

    // A billing-shaped body so the route parses a real balance from the mock.
    let billing = serde_json::json!({
        "code": 0,
        "msg": "OK",
        "data": {"Response": {"Data": {"Accounts": [{
            "PackageName": "Pro 5000",
            "SlicePeriodCapacityRemainPrecise": 1234.5,
            "SlicePeriodCapacitySizePrecise": 5000.0,
            "PackageEndTime": "2099-12-31 23:59:59"
        }]}}}
    });

    // ── Account-pool request ───────────────────────────────────────────────
    let (base_url, captured) = mock_upstream_with_body(1, billing.clone()).await;
    let service = ServiceController::new(false);
    service.mark_running();
    let app = build_app_router(
        service,
        Arc::new(LogBuffer::new(200)),
        Arc::new(Config {
            upstream_urls: vec![base_url.clone()],
            api_key: Some("static-key".to_string()),
            models_flavor: ModelsFlavor::WorkBuddyConfig,
            credential_pool_enabled: true,
            credits_endpoint: Some(format!("{base_url}/v2/billing/meter/get-user-resource")),
            ..Default::default()
        }),
        reqwest::Client::new(),
        StatsDb::in_memory().expect("in-memory sqlite"),
        metrics::install(),
        Arc::new(CredentialPool::new(vec![test_credential()])),
    );

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/credits")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);

    let requests = captured.lock().expect("capture lock").clone();
    assert_eq!(requests.len(), 1);
    let account = &requests[0];
    assert_eq!(
        account.header("authorization"),
        Some("Bearer account-access-token")
    );
    assert_eq!(
        account.header("x-api-key"),
        None,
        "the credits route must not send a login token as X-API-Key; got {:?}",
        account.header("x-api-key")
    );

    // ── Static-key request keeps the pair ──────────────────────────────────
    let (base_url, captured) = mock_upstream_with_body(1, billing).await;
    let service = ServiceController::new(false);
    service.mark_running();
    let app = build_app_router(
        service,
        Arc::new(LogBuffer::new(200)),
        Arc::new(Config {
            upstream_urls: vec![base_url.clone()],
            api_key: Some("static-key".to_string()),
            models_flavor: ModelsFlavor::WorkBuddyConfig,
            credential_pool_enabled: false,
            credits_endpoint: Some(format!("{base_url}/v2/billing/meter/get-user-resource")),
            ..Default::default()
        }),
        reqwest::Client::new(),
        StatsDb::in_memory().expect("in-memory sqlite"),
        metrics::install(),
        Arc::new(CredentialPool::empty()),
    );
    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/credits")
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router responds");
    assert_eq!(response.status(), StatusCode::OK);

    let requests = captured.lock().expect("capture lock").clone();
    assert_eq!(requests.len(), 1);
    let static_key = &requests[0];
    assert_eq!(
        static_key.header("authorization"),
        Some("Bearer static-key")
    );
    assert_eq!(
        static_key.header("x-api-key"),
        Some("static-key"),
        "a static gateway key keeps the Bearer + X-API-Key pair"
    );
}
