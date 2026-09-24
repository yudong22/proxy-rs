//! Why `/v1/messages` could answer with a bare `400 Failed to buffer the request
//! body` that nothing in the proxy logged — and what it does now.
//!
//! Both handlers extract the body twice: `peek_json_body` buffers it once (for
//! session identification) and hands a rebuilt request to `Json`, which buffers
//! it again. axum applies its 2 MB default limit on the second pass, and every
//! rejection was funnelled into `ProxyError::Transform` *before* any of the
//! handler's logging ran — so a body over the limit produced a 400 the proxy
//! never recorded. Claude Code's own bodies already reach ~2 MB on a long
//! session, which is how this reached a real user.
//!
//! The fix is asserted at the HTTP layer, through the real `build_app_router`:
//!
//!   * a body over the old 2 MB default is no longer rejected,
//!   * a body over the configured cap is a 413 that says so, and
//!   * both the 413 and a failed read leave a trace in the log buffer and the
//!     stats DB, so the failure is visible instead of silent.

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use bytes::Bytes;
use futures::stream;
use proxy_rs::{
    config::Config,
    metrics,
    router::build_app_router,
    service::ServiceController,
    settings::LogBuffer,
    stats::{RequestLogFilter, StatsDb},
    util::MAX_BODY_ENV,
};
use std::sync::Arc;
use tower::ServiceExt;

/// Serialises every test in this file.
///
/// The body cap is read from the environment, which is process-global, and one
/// test pins it small. A router built concurrently by another test would
/// inherit that cap and fail for a reason that has nothing to do with what it
/// asserts, so the whole file runs one test at a time.
static SERIAL: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The router with the service deliberately stopped.
///
/// Parsing happens before the `is_running` check, so a request that *parses*
/// reaches 503 while a request rejected during body handling never gets that
/// far. That difference is what these tests read.
fn router() -> (axum::Router, Arc<LogBuffer>, Arc<StatsDb>) {
    let logs = Arc::new(LogBuffer::new(100));
    let stats = StatsDb::in_memory().expect("in-memory sqlite");
    let app = build_app_router(
        ServiceController::new(false),
        logs.clone(),
        Arc::new(Config::default()),
        reqwest::Client::new(),
        stats.clone(),
        metrics::install(),
    );
    (app, logs, stats)
}

async fn body_text(response: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("response body");
    String::from_utf8_lossy(&bytes).to_string()
}

fn post_messages(body: Body) -> Request<Body> {
    Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-type", "application/json")
        .body(body)
        .expect("request builds")
}

/// A valid Anthropic body whose single user message is `filler_len` bytes.
///
/// Valid JSON on purpose: only the size may be what rejects it, so a 413 cannot
/// be confused with a parse error.
fn body_of_size(filler_len: usize) -> String {
    let filler = "x".repeat(filler_len);
    serde_json::json!({
        "model": "deepseek-chat",
        "max_tokens": 16,
        "messages": [{"role": "user", "content": filler}],
    })
    .to_string()
}

/// Every log line currently buffered.
async fn logged(logs: &Arc<LogBuffer>) -> String {
    logs.snapshot()
        .await
        .iter()
        .map(|e| format!("[{}] {}", e.level, e.message))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Total request-log rows, whatever the filter.
fn stored_rows(stats: &Arc<StatsDb>) -> i64 {
    stats
        .query_request_logs(&RequestLogFilter::default())
        .expect("query request logs")
        .total
}

/// A body under the limit parses, so it reaches the stopped-service answer.
/// This is the control: the route is otherwise healthy.
#[tokio::test]
async fn small_body_parses_and_reaches_the_service_check() {
    let _guard = SERIAL.lock().await;
    let (app, _logs, _stats) = router();
    let response = app
        .oneshot(post_messages(Body::from(body_of_size(16))))
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
}

/// The regression itself: ~3 MB is past axum's 2 MB default but under the
/// configured cap, so it must parse rather than be rejected.
#[tokio::test]
async fn body_over_the_old_two_megabyte_default_is_accepted() {
    let _guard = SERIAL.lock().await;
    let (app, _logs, _stats) = router();
    let response = app
        .oneshot(post_messages(Body::from(body_of_size(3 * 1024 * 1024))))
        .await
        .expect("router responds");

    let status = response.status();
    let text = body_text(response).await;
    assert_eq!(
        status,
        StatusCode::SERVICE_UNAVAILABLE,
        "a 3 MB body must parse and reach the service check, got {status}: {text}"
    );
}

/// A body past the configured cap is refused with 413, not 400, and the message
/// names the limit and the knob that raises it.
#[tokio::test]
async fn body_over_the_configured_cap_is_a_413_that_explains_itself() {
    // The cap is read from the environment when the router is built, so the test
    // pins a small one instead of moving 32 MiB through memory. Env vars are
    // process-wide, so the override is serialised and always removed again —
    // even if an assertion below fails.
    let _guard = SERIAL.lock().await;
    std::env::set_var(MAX_BODY_ENV, "4096");
    let (app, logs, stats) = router();

    let response = app
        .oneshot(post_messages(Body::from(body_of_size(8192))))
        .await
        .expect("router responds");
    let status = response.status();
    let text = body_text(response).await;
    std::env::remove_var(MAX_BODY_ENV);

    assert_eq!(
        status,
        StatusCode::PAYLOAD_TOO_LARGE,
        "got {status}: {text}"
    );
    assert!(
        text.contains("exceeds"),
        "message should name the limit: {text}"
    );
    assert!(
        text.contains(MAX_BODY_ENV),
        "message should name the override: {text}"
    );

    // …and the rejection is now visible on both surfaces.
    let logged = logged(&logs).await;
    assert!(
        logged.contains("exceeds"),
        "the 413 must reach the log buffer: {logged}"
    );
    assert_eq!(
        stored_rows(&stats),
        1,
        "the 413 must be recorded as a request row"
    );

    let row = stats
        .query_request_logs(&RequestLogFilter::default())
        .expect("query")
        .items
        .remove(0);
    assert_eq!(row.status, 413, "the recorded status must be the one sent");
}

/// A body that errors mid-read is a 400 — but a *read* failure, not a parse
/// failure, and it leaves a trace. Previously it surfaced as the opaque
/// "Failed to buffer the request body" with nothing recorded.
#[tokio::test]
async fn aborted_body_read_is_visible_and_distinct_from_a_parse_error() {
    let _guard = SERIAL.lock().await;
    let (app, logs, stats) = router();
    let broken = stream::once(async {
        Err::<Bytes, std::io::Error>(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "client went away mid-body",
        ))
    });

    let response = app
        .oneshot(post_messages(Body::from_stream(broken)))
        .await
        .expect("router responds");

    let status = response.status();
    let text = body_text(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got {status}: {text}");
    assert!(
        text.contains("could not read the request body"),
        "a read failure must not be reported as a parse failure: {text}"
    );

    let logged = logged(&logs).await;
    assert!(
        logged.contains("rejected"),
        "the failed read must reach the log buffer: {logged}"
    );
    assert_eq!(stored_rows(&stats), 1, "the failed read must be recorded");
}

/// Malformed JSON is still the extractor's to report, and now says which route
/// and which field set it failed to satisfy.
#[tokio::test]
async fn malformed_json_is_a_bad_request_that_names_the_route() {
    let _guard = SERIAL.lock().await;
    let (app, logs, stats) = router();
    let response = app
        .oneshot(post_messages(Body::from("{ not json")))
        .await
        .expect("router responds");

    let status = response.status();
    let text = body_text(response).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "got {status}: {text}");
    assert!(
        text.contains("/v1/messages"),
        "the message should name the route: {text}"
    );

    let logged = logged(&logs).await;
    assert!(
        logged.contains("/v1/messages"),
        "the parse failure must reach the log buffer: {logged}"
    );
    // The same invisibility applied here: a body that failed extraction also
    // returned before any logging, so it too left no row behind.
    assert_eq!(
        stored_rows(&stats),
        1,
        "the parse failure must be recorded as a request row"
    );
}

/// A structurally valid body that cannot deserialize into the request type is
/// axum's 422, and that must stay distinct from a syntax error's 400.
#[tokio::test]
async fn wrong_shape_is_a_422_not_a_400() {
    let _guard = SERIAL.lock().await;
    let (app, _logs, stats) = router();
    // Valid JSON, wrong types: `messages` is not an array of messages.
    let response = app
        .oneshot(post_messages(Body::from(
            r#"{"model": "m", "max_tokens": 1, "messages": "not-an-array"}"#,
        )))
        .await
        .expect("router responds");

    let status = response.status();
    let text = body_text(response).await;
    assert_eq!(
        status,
        StatusCode::UNPROCESSABLE_ENTITY,
        "a shape mismatch must stay 422, got {status}: {text}"
    );
    assert_eq!(stored_rows(&stats), 1, "and it must be recorded");
}

/// A wrong `content-type` is axum's 415, and it must survive as 415 rather than
/// being flattened to 400 by the `Transform` catch-all.
#[tokio::test]
async fn missing_json_content_type_keeps_its_415() {
    let (app, _logs, _stats) = router();
    let response = app
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/v1/messages")
                .body(Body::from(body_of_size(16)))
                .expect("request builds"),
        )
        .await
        .expect("router responds");

    let status = response.status();
    let text = body_text(response).await;
    assert_eq!(
        status,
        StatusCode::UNSUPPORTED_MEDIA_TYPE,
        "a content-type rejection must keep its status, got {status}: {text}"
    );
}
