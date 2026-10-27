//! End-to-end checks for the read-only observation endpoints (`/api/*`).
//!
//! These drive the real [`build_app_router`], not the shaping helpers, because
//! the failure this suite is meant to catch is a wiring one: a route that is
//! registered in the wrong namespace, a `Query` extraction that drops a filter,
//! or a handler that answers 503 because it was gated on the service flag the way
//! the forwarding routes are. None of those are visible from a unit test of
//! `api::stats_today_json`.
//!
//! The endpoints are deliberately *not* gated on `ServiceController::is_running`
//! — `test_router` below builds the router with a stopped controller on purpose,
//! so every assertion here also pins that decision down. A future change that
//! adds the gate would fail these tests rather than silently blanking the
//! dashboard whenever the proxy is down.
//!
//! Log output is redirected per test for the same reason as in
//! `router_redaction.rs`: `LogBuffer` seeds from and appends to a
//! process-global path, so without the redirect these tests would write into the
//! developer's real `~/.proxy-rs/logs/proxy.log`.

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
    stats::{RequestOutcome, StatsDb, TokenRecord, UpstreamTiming},
};
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;

/// Serialises the tests, which all repoint one process-global log path.
static LOG_LOCK: Mutex<()> = Mutex::const_new(());

async fn isolate_log(name: &str) -> tokio::sync::MutexGuard<'static, ()> {
    let guard = LOG_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!("proxy-rs-api-it-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join(name);
    let _ = std::fs::remove_file(&path);
    settings::set_log_file_override_for_tests(path);
    guard
}

/// A router whose `StatsDb` is seeded with `rows`, and whose service is stopped.
fn test_router(rows: Vec<RequestOutcome<'_>>) -> axum::Router {
    let stats = StatsDb::in_memory().expect("in-memory sqlite");
    for row in rows {
        stats.record_request_log(row).expect("record request log");
    }
    build_app_router(
        ServiceController::new(false),
        Arc::new(LogBuffer::new(100)),
        Arc::new(Config::default()),
        reqwest::Client::new(),
        stats,
        metrics::install(),
        Arc::new(proxy_rs::session_pool::CredentialPool::empty()),
    )
}

/// GET `uri` and return `(status, parsed JSON body)`.
///
/// Takes the router by value: `oneshot` consumes it, and `Router` is cheap to
/// clone, so callers that make several requests clone at the call site.
async fn get_json(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    let response = app
        .oneshot(
            Request::builder()
                .uri(uri)
                .body(Body::empty())
                .expect("request builds"),
        )
        .await
        .expect("router responds");

    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

/// One servable row: `model`, requested model, route, status, tokens.
fn row<'a>(
    model: &'a str,
    route: &'a str,
    tokens: &'a TokenRecord,
    status: u16,
) -> RequestOutcome<'a> {
    RequestOutcome {
        model,
        client_model: model,
        route,
        tokens,
        duration_ms: 1250,
        timing: UpstreamTiming {
            ttft_ms: 200,
            model_ms: 800,
            ended_with_tool_call: false,
        },
        streamed: true,
        status,
        error: None,
        session_id: "claude:abc",
        // Claude Code reports no lineage; the gateway stores an empty parent.
        parent_session_id: "",
        client: "claude",
        override_key: "",
        override_model: "",
        override_reason: "",
    }
}

#[tokio::test]
async fn stats_today_answers_over_http_while_the_service_is_stopped() {
    let _guard = isolate_log("api-stats.log").await;

    let tokens = TokenRecord {
        input: 100,
        cache_read: 40,
        cache_write: 10,
        output: 50,
    };
    let tokens2 = TokenRecord {
        input: 20,
        cache_read: 0,
        cache_write: 0,
        output: 5,
    };
    let app = test_router(vec![
        row("deepseek-chat", "/v1/messages", &tokens, 200),
        row("deepseek-chat", "/v1/messages", &tokens2, 502),
    ]);

    let (status, body) = get_json(app, "/api/stats/today").await;

    // The stopped controller must NOT produce a 503 here: this endpoint is the
    // dashboard's own read path, not the forwarding path.
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert!(body["date"].is_string(), "date is reported: {body}");
    assert_eq!(body["requests_total"], 2);
    assert_eq!(body["requests_success"], 1);
    assert_eq!(body["requests_failed"], 1);
    // Totals are the four categories summed: 100+40+10+50 + 20+0+0+5 = 225.
    assert_eq!(body["tokens_total"], 225);
    // cache_read / (input + cache_read) over both rows = 40 / (120 + 40) = 25%.
    assert_eq!(body["cache_hit_pct"], 25);
}

#[tokio::test]
async fn stats_today_is_empty_but_valid_before_any_request() {
    let _guard = isolate_log("api-stats-empty.log").await;

    let (status, body) = get_json(test_router(vec![]), "/api/stats/today").await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["requests_total"], 0);
    assert_eq!(body["tokens_total"], 0);
    // No input traffic means no cache rate at all — 0, never a division panic.
    assert_eq!(body["cache_hit_pct"], 0);
}

#[tokio::test]
async fn request_logs_returns_rows_and_facets() {
    let _guard = isolate_log("api-logs.log").await;

    let tokens = TokenRecord::default();
    let app = test_router(vec![
        row("deepseek-chat", "/v1/messages", &tokens, 200),
        row("hy3", "/v1/responses", &tokens, 200),
    ]);

    let (status, body) = get_json(app, "/api/request-logs").await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["total"], 2);
    assert_eq!(body["items"].as_array().map(Vec::len), Some(2));

    // The facet lists ride along with the page, exactly as the Tauri command
    // returned them — the dropdowns are built from these, not from a second
    // request.
    let models: Vec<&str> = body["models"]
        .as_array()
        .expect("models facet")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(models, ["deepseek-chat", "hy3"]);
    assert!(body["clients"].is_array(), "clients facet present");
    assert!(body["sessions"].is_array(), "sessions facet present");

    // A row carries the fields the table renders.
    let first = &body["items"][0];
    for key in [
        "id",
        "created_at",
        "model",
        "route",
        "input_tokens",
        "output_tokens",
        "duration_ms",
        "streamed",
        "status",
        "session_id",
        "client",
    ] {
        assert!(!first[key].is_null(), "row is missing {key}: {first}");
    }
}

#[tokio::test]
async fn request_logs_honours_query_string_filters() {
    let _guard = isolate_log("api-logs-filter.log").await;

    let tokens = TokenRecord::default();
    let app = test_router(vec![
        row("deepseek-chat", "/v1/messages", &tokens, 200),
        row("hy3", "/v1/responses", &tokens, 200),
        row("hy3", "/v1/responses", &tokens, 502),
    ]);

    // `model` narrows the page but the facet lists stay database-wide: the
    // dropdown must not collapse to the value already selected.
    let (status, body) = get_json(app.clone(), "/api/request-logs?model=hy3").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["total"], 2);
    let models: Vec<&str> = body["models"]
        .as_array()
        .expect("models facet")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert_eq!(models, ["deepseek-chat", "hy3"]);

    // Status grouping and limit/offset are the pager's contract.
    let (status, body) = get_json(app.clone(), "/api/request-logs?status_group=5xx").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 1);
    assert_eq!(body["items"][0]["status"], 502);

    let (status, body) = get_json(app.clone(), "/api/request-logs?limit=1&offset=1").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["total"], 3, "total counts the filter, not the page");
    assert_eq!(body["items"].as_array().map(Vec::len), Some(1));

    // A boolean filter arrives as a query-string word. The window's table sends
    // `streamed=true` (see `core/api.js`), so the deserializer must accept the
    // URL form rather than only a JSON `true` — a mismatch here would silently
    // return the unfiltered page, and the only symptom would be a filter that
    // does nothing.
    let (status, body) = get_json(app.clone(), "/api/request-logs?streamed=true").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["total"], 3, "every fixture row is streamed");
    let (status, body) = get_json(app.clone(), "/api/request-logs?streamed=false").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");
    assert_eq!(body["total"], 0, "no fixture row is non-streaming");
}

#[tokio::test]
async fn unknown_observation_route_is_a_404_not_a_proxied_request() {
    let _guard = isolate_log("api-404.log").await;

    let (status, _) = get_json(test_router(vec![]), "/api/stats/yesterday").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // The namespace itself must not shadow the protocol routes it sits beside.
    let (status, _) = get_json(test_router(vec![]), "/v1/messages").await;
    assert_eq!(
        status,
        StatusCode::METHOD_NOT_ALLOWED,
        "/v1/messages still exists as POST, so the /api routes did not displace it"
    );
}
