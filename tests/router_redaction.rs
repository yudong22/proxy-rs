//! End-to-end checks driven through the real `build_app_router`, so a fix is
//! verified at the HTTP layer rather than only at the helper level.
//!
//! These tests exist because the `/v1/credits` handler used to log inbound
//! headers through a hand-rolled loop instead of the shared redactor: the
//! caller's own `authorization` value reached `proxy.log` verbatim. Asserting on
//! `format_headers` alone would not have caught that — the handler simply did
//! not call it.
//!
//! The log destination is process-global, and `LogBuffer` both seeds from and
//! appends to it. Each test therefore redirects it to its own temp file and
//! holds an async-aware lock for its whole body. Without the redirect these
//! tests would write their synthetic credentials into the developer's real,
//! long-lived `~/.proxy-rs/logs/proxy.log`.

use axum::{
    body::Body,
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

/// Serialises the tests in this file, which all repoint one process-global path.
///
/// A `tokio::sync::Mutex` rather than a `std` one: the guard is held across
/// `await` points, which is exactly what `std::sync::Mutex` must not do.
static LOG_LOCK: Mutex<()> = Mutex::const_new(());

/// Redirect log output to a temp file, returning the path to assert on.
///
/// The caller must hold the returned guard while it drives requests, so no other
/// test can move the destination underneath it.
async fn isolate_log(name: &str) -> (std::path::PathBuf, tokio::sync::MutexGuard<'static, ()>) {
    let guard = LOG_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!("proxy-rs-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    settings::set_log_file_override_for_tests(path.clone());
    (path, guard)
}

fn test_router(logs: Arc<LogBuffer>) -> axum::Router {
    let config = Arc::new(Config::default());
    let handle = metrics::install();
    build_app_router(
        ServiceController::new(false),
        logs,
        config,
        reqwest::Client::new(),
        StatsDb::in_memory().expect("in-memory sqlite"),
        handle,
    )
}

#[tokio::test]
async fn credits_does_not_log_the_callers_credentials() {
    const SECRET: &str = "ck_supersecret_value_that_must_never_be_logged";

    let (log_path, _guard) = isolate_log("credits-redaction.log").await;
    let logs = Arc::new(LogBuffer::new(500));
    let app = test_router(logs.clone());

    let response = app
        .oneshot(
            Request::builder()
                .uri("/v1/credits")
                .header("authorization", format!("Bearer {SECRET}"))
                .header("x-api-key", SECRET)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    // The service is stopped in this router, so a 503 is the expected outcome —
    // the header-logging side effect has already run by then.
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

    let buffered: String = logs
        .snapshot()
        .await
        .iter()
        .map(|e| e.message.clone())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !buffered.contains(SECRET),
        "credential leaked into the in-memory log: {buffered}"
    );
    assert!(
        buffered.contains("/v1/credits headers"),
        "the header line should still be logged for debugging: {buffered}"
    );

    // The mirror is the long-lived artifact users paste into bug reports, so
    // assert on it directly rather than trusting the in-memory buffer.
    let mirrored = std::fs::read_to_string(&log_path).unwrap_or_default();
    assert!(
        !mirrored.contains(SECRET),
        "credential leaked into the on-disk log: {mirrored}"
    );
}

#[tokio::test]
async fn health_route_is_reachable() {
    let (_log_path, _guard) = isolate_log("health.log").await;
    let logs = Arc::new(LogBuffer::new(100));
    let app = test_router(logs);

    let response = app
        .oneshot(
            Request::builder()
                .uri("/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .expect("router responds");

    assert_eq!(response.status(), StatusCode::OK);
}
