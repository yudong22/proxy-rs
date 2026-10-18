//! End-to-end check that a terminal upstream rejection reaches the client with
//! the upstream's own status, driven through the real `build_app_router`.
//!
//! This is a regression test with a specific history behind it. A Codex session
//! sent a 1.18M-token prompt against a model whose upstream enforces a
//! 1,048,576-token cap. The gateway answered 400 (`code 11115`, "prompt is too
//! long"), the proxy re-wrapped it as `ProxyError::Upstream`, which renders 502
//! — and Codex, reading 502 as "the gateway is temporarily down", resent the
//! identical prompt 37 times instead of compacting its context.
//!
//! Asserting `ProxyError::status()` alone would not catch a regression: the
//! flattening happened in `forward_request`, at the point the error was
//! *constructed*. So this test drives a real HTTP request through the router
//! against a local upstream that always answers 400, and asserts on the status
//! and body the client sees.

use axum::{
    body::{to_bytes, Body},
    http::{Request, StatusCode},
};
use proxy_rs::{
    config::Config,
    metrics,
    router::build_app_router,
    service::ServiceController,
    settings::{self, LogBuffer},
    stats::StatsDb,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;

/// Serialises these tests: they repoint the process-global log path and bind
/// local ports.
static TEST_LOCK: Mutex<()> = Mutex::const_new(());

/// The upstream body the proxy must relay verbatim.
///
/// This is the shape a client's own SDK parses: `code: "context_length_exceeded"`
/// is what tells Codex to compact rather than retry, and re-wrapping the body
/// in the proxy's `{"error":{"type":"proxy_error",…}}` envelope would erase it.
const OVERFLOW_BODY: &str = concat!(
    r#"{"error":{"message":"prompt is too long: 1187714 tokens > 1048576 maximum","#,
    r#""type":"invalid_request_error","param":"","code":"context_length_exceeded"}}"#
);

/// Serve `status` + `body` on a local port for the duration of one test.
///
/// A tiny hand-rolled server rather than a mock crate: the test needs a real
/// socket so the request travels the same `reqwest` path production does.
async fn spawn_upstream(status: u16, body: &'static str) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind a loopback port");
    let addr = listener.local_addr().expect("local addr");

    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            // Drain the request so the client's write completes before we reply.
            let mut buf = vec![0u8; 64 * 1024];
            let _ = stream.read(&mut buf).await;
            let response = format!(
                "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n\
                 Content-Length: {len}\r\nConnection: close\r\n\r\n{body}",
                len = body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.flush().await;
        }
    });

    format!("http://{addr}")
}

/// A router whose upstream is the given base URL, with logging redirected to a
/// temp file so synthetic traffic never touches the developer's real log.
async fn router_against(upstream_base: &str) -> axum::Router {
    let dir = std::env::temp_dir().join(format!("proxy-rs-relay-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    settings::set_log_file_override_for_tests(dir.join("relay.log"));

    let config = Config {
        upstream_urls: vec![upstream_base.to_string()],
        api_key: Some("test-key".to_string()),
        force_stream_upstream: false,
        ..Config::default()
    };

    // The router refuses traffic unless the service is marked running, and a
    // 503 would mask the status this test is about.
    let service = ServiceController::new(false);
    service.mark_running();

    build_app_router(
        service,
        Arc::new(LogBuffer::new(200)),
        Arc::new(config),
        reqwest::Client::new(),
        StatsDb::in_memory().expect("in-memory sqlite"),
        metrics::install(),
        Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
    )
}

fn chat_body() -> Body {
    Body::from(
        serde_json::json!({
            "model": "deepseek-v4.1-flash",
            "max_tokens": 16,
            "messages": [{ "role": "user", "content": "hi" }],
        })
        .to_string(),
    )
}

/// The regression: a 400 from upstream must come back as 400, not 502.
#[tokio::test]
async fn upstream_400_reaches_the_client_as_400() {
    let _guard = TEST_LOCK.lock().await;

    let base = spawn_upstream(400, OVERFLOW_BODY).await;
    let app = router_against(&base).await;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(chat_body())
                .unwrap(),
        )
        .await
        .expect("router responds");

    assert_eq!(
        response.status(),
        StatusCode::BAD_REQUEST,
        "a terminal upstream rejection must not be reported as a gateway fault"
    );

    let bytes = to_bytes(response.into_body(), 64 * 1024)
        .await
        .expect("body readable");
    let text = String::from_utf8_lossy(&bytes);
    // The client's error taxonomy is the actionable part: it is what makes a
    // caller compact instead of retrying.
    assert!(
        text.contains("context_length_exceeded"),
        "the upstream error body must be relayed verbatim, got: {text}"
    );
    assert!(
        !text.contains("proxy_error"),
        "the body must not be re-wrapped in the proxy envelope, got: {text}"
    );
}

/// A 413 is the other terminal rejection that must survive.
#[tokio::test]
async fn upstream_413_reaches_the_client_as_413() {
    let _guard = TEST_LOCK.lock().await;

    let base = spawn_upstream(413, r#"{"error":{"message":"body too large"}}"#).await;
    let app = router_against(&base).await;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(chat_body())
                .unwrap(),
        )
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::PAYLOAD_TOO_LARGE);
}

/// A 5xx stays a 502: that *is* the gateway failing, and clients already treat
/// it as retriable. Widening the relay to every status would break that.
#[tokio::test]
async fn upstream_500_stays_a_gateway_error() {
    let _guard = TEST_LOCK.lock().await;

    let base = spawn_upstream(500, r#"{"error":{"message":"boom"}}"#).await;
    let app = router_against(&base).await;

    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/chat/completions")
                .header("content-type", "application/json")
                .body(chat_body())
                .unwrap(),
        )
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
}
