//! End-to-end checks for the scheduler observation and trigger routes.
//!
//! These drive the real [`build_app_router`] rather than the shaping helpers,
//! because the failures worth catching here are wiring and policy failures:
//!
//! * the routes landing in the wrong namespace, or under `/v1/*`;
//! * the read route getting gated on `ServiceController::is_running` the way the
//!   forwarding routes are (it must not be — see `tests/api_endpoints.rs` for the
//!   same decision on the sibling routes);
//! * the **mutating** route being reachable without a token. `/api/*` has no
//!   authentication and `allow_origin(Any)`, which is an accepted trade-off for
//!   read-only diagnostics and must not silently extend to a route that can make
//!   the gateway act on upstream accounts.
//!
//! Every test that could write files points the data directory and the log file
//! at a temp dir first, so a test run never touches the developer's real
//! `~/.proxy-rs`.
//!
//! # Serialization
//!
//! The data-directory override and the log-file override are process-global, so
//! these tests hold a shared mutex. They are cheap; correctness of the isolation
//! matters more than parallel wall-clock time.

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
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::Mutex;
use tower::ServiceExt;

/// Serialises the tests, which all repoint process-global paths.
static GLOBAL_LOCK: Mutex<()> = Mutex::const_new(());

/// Point the data dir and the log file at a temp directory for this test.
///
/// Returns the guard, which must outlive the test: dropping it restores nothing,
/// so the *next* test re-sets both. That is why every test takes the same lock.
struct Isolated {
    _guard: tokio::sync::MutexGuard<'static, ()>,
    dir: std::path::PathBuf,
}

impl Drop for Isolated {
    fn drop(&mut self) {
        // Leave the temp dir behind on failure, where it is useful; remove it on
        // success so repeated runs do not accumulate directories.
        if std::thread::panicking() {
            return;
        }
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn isolate(name: &str) -> Isolated {
    let guard = GLOBAL_LOCK.lock().await;
    let dir = std::env::temp_dir().join(format!(
        "proxy-rs-scheduler-api-{}-{}",
        std::process::id(),
        name
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    // `PROXY_DATA_DIR` is read on every `data_dir()` call, so setting it here
    // redirects the scheduler state file too — including through the handler.
    std::env::set_var("PROXY_DATA_DIR", &dir);
    settings::set_log_file_override_for_tests(dir.join("proxy.log"));
    Isolated { _guard: guard, dir }
}

/// A router whose service is stopped on purpose.
///
/// The stopped controller is part of the fixture: it pins the decision that
/// observation routes are not gated on the running flag. A future change that
/// adds the gate fails these tests instead of blanking the endpoint whenever the
/// proxy is down.
fn test_router() -> axum::Router {
    let stats = StatsDb::in_memory().expect("in-memory sqlite");
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

/// Send one request and return `(status, parsed JSON body)`.
async fn send(app: axum::Router, request: Request<Body>) -> (StatusCode, Value) {
    let response = app.oneshot(request).await.expect("router responds");
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .expect("body collects");
    let json = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    (status, json)
}

async fn get(app: axum::Router, uri: &str) -> (StatusCode, Value) {
    send(
        app,
        Request::builder()
            .uri(uri)
            .body(Body::empty())
            .expect("request builds"),
    )
    .await
}

/// POST with explicit headers, so a test can shape the auth surface exactly.
async fn post(
    app: axum::Router,
    uri: &str,
    origin: Option<&str>,
    authorization: Option<&str>,
) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("POST").uri(uri);
    if let Some(origin) = origin {
        builder = builder.header("origin", origin);
    }
    if let Some(authorization) = authorization {
        builder = builder.header("authorization", authorization);
    }
    send(app, builder.body(Body::empty()).expect("request builds")).await
}

/// Clear the token env var so a test starts from "mutation disabled".
///
/// `PROXY_API_TOKEN` may be reaching this process through `~/.proxy-rs/.env`,
/// which `env_value` consults, so both the variable and the data directory (which
/// relocates the `.env` lookup) are neutralised by [`isolate`].
fn clear_token() {
    std::env::remove_var("PROXY_API_TOKEN");
}

#[tokio::test]
async fn scheduler_snapshot_answers_over_http_while_the_service_is_stopped() {
    let _iso = isolate("snapshot").await;
    clear_token();

    let (status, body) = get(test_router(), "/api/scheduler").await;

    assert_eq!(status, StatusCode::OK, "body: {body}");
    // Shape contract: every consumer reads these keys.
    for key in [
        "now",
        "now_ms",
        "utc_offset_secs",
        "process",
        "note",
        "loops",
    ] {
        assert!(!body[key].is_null(), "missing {key}: {body}");
    }
    let loops = body["loops"].as_array().expect("loops is an array");
    assert_eq!(loops.len(), 3, "three loops: {body}");
    let ids: Vec<&str> = loops.iter().filter_map(|l| l["id"].as_str()).collect();
    assert_eq!(ids, ["checkin", "login_scan", "growth"]);

    // Each loop carries both record slots as keys, even when null. A consumer
    // that branches on `last_skip` must not have to handle "key absent".
    for loop_json in loops {
        for key in ["last_run", "last_skip", "next_due", "cadence", "detail"] {
            assert!(
                loop_json.get(key).is_some(),
                "{} is missing {key}: {loop_json}",
                loop_json["id"]
            );
        }
    }
}

#[tokio::test]
async fn scheduler_snapshot_reports_no_last_run_on_a_fresh_install() {
    let _iso = isolate("fresh").await;
    clear_token();

    let (status, body) = get(test_router(), "/api/scheduler").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    // A fresh data directory has no state file. The honest answer is `null`,
    // never a fabricated timestamp — that is the whole reason the state file is
    // separate from the preference file that records "ran today".
    for loop_json in body["loops"].as_array().expect("loops") {
        assert!(
            loop_json["last_run"].is_null(),
            "{} invented a last run: {loop_json}",
            loop_json["id"]
        );
        assert!(
            loop_json["last_skip"].is_null(),
            "{} invented a skip: {loop_json}",
            loop_json["id"]
        );
    }
}

/// A disabled loop must not claim a countdown.
#[tokio::test]
async fn scheduler_snapshot_reports_disabled_loops_without_a_next_due() {
    let _iso = isolate("disabled").await;
    clear_token();

    let (status, body) = get(test_router(), "/api/scheduler").await;
    assert_eq!(status, StatusCode::OK, "body: {body}");

    for loop_json in body["loops"].as_array().expect("loops") {
        if loop_json["enabled"] == false {
            assert_eq!(
                loop_json["next_due"]["kind"], "disabled",
                "{} is off but claims a next due: {loop_json}",
                loop_json["id"]
            );
            assert!(
                loop_json["next_due"]["in_secs"].is_null(),
                "{} must not report a countdown while off: {loop_json}",
                loop_json["id"]
            );
        }
    }
}

/// The default posture, and the one that matters most: with no token
/// configured, the mutating route refuses everything.
#[tokio::test]
async fn run_route_is_refused_when_no_token_is_configured() {
    let _iso = isolate("no-token").await;
    clear_token();

    // Even a well-formed request with a plausible token is refused, because the
    // route is switched off entirely rather than comparing against nothing.
    let (status, body) = post(
        test_router(),
        "/api/scheduler/run/checkin",
        None,
        Some("Bearer anything"),
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["type"], "mutation_disabled", "{body}");
}

/// A configured token does not make the route browser-callable.
///
/// CORS is `allow_origin(Any)`, so a page can *send* the POST; the `Origin`
/// header is what distinguishes it from a legitimate local process, and it must
/// be refused even when the token is correct — otherwise every "open a link"
/// moment is a remote trigger for the gateway's account actions.
#[tokio::test]
async fn run_route_refuses_a_browser_origin_even_with_the_right_token() {
    let _iso = isolate("origin").await;
    std::env::set_var("PROXY_API_TOKEN", "it-is-a-test-token");

    let (status, body) = post(
        test_router(),
        "/api/scheduler/run/checkin",
        Some("http://evil.example"),
        Some("Bearer it-is-a-test-token"),
    )
    .await;

    // If this ever returns 202, a web page can drain the account pool.
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["type"], "browser_origin", "{body}");

    clear_token();
}

/// A wrong or missing bearer token is a 401, not a 403 or a 500.
#[tokio::test]
async fn run_route_rejects_a_bad_token() {
    let _iso = isolate("bad-token").await;
    std::env::set_var("PROXY_API_TOKEN", "the-real-token");

    for authorization in [
        None,
        Some("Bearer wrong"),
        Some("Bearer the-real-toke"),
        Some("Bearer the-real-token-extended"),
        Some("the-real-token"),
        Some("Basic the-real-token"),
        Some(""),
    ] {
        let (status, body) = post(
            test_router(),
            "/api/scheduler/run/checkin",
            None,
            authorization,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "authorization={authorization:?} body: {body}"
        );
        assert_eq!(body["error"]["type"], "unauthorized", "{body}");
    }

    clear_token();
}

/// Auth is checked before the loop id, so an unauthenticated caller cannot
/// enumerate which ids exist — and an authenticated one gets a real 404.
#[tokio::test]
async fn run_route_distinguishes_unknown_loop_from_unauthorized() {
    let _iso = isolate("unknown").await;
    clear_token();

    // Unauthenticated + unknown id: the auth answer wins.
    let (status, body) = post(test_router(), "/api/scheduler/run/nope", None, None).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "body: {body}");
    assert_eq!(body["error"]["type"], "mutation_disabled", "{body}");

    // Authenticated + unknown id: 404 with the accepted vocabulary in the
    // message, because the caller's fix is a different URL.
    std::env::set_var("PROXY_API_TOKEN", "the-real-token");
    let (status, body) = post(
        test_router(),
        "/api/scheduler/run/nope",
        None,
        Some("Bearer the-real-token"),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body: {body}");
    assert_eq!(body["error"]["type"], "unknown_loop", "{body}");
    let message = body["error"]["message"].as_str().unwrap_or_default();
    for id in ["checkin", "login_scan", "growth"] {
        assert!(
            message.contains(id),
            "the 404 should name the accepted ids, got: {message}"
        );
    }

    clear_token();
}

/// The two namespaces stay separate: the scheduler routes must not shadow, or be
/// shadowed by, the protocol routes they sit beside.
#[tokio::test]
async fn scheduler_routes_leave_the_protocol_namespace_alone() {
    let _iso = isolate("namespace").await;
    clear_token();

    // GET on a POST-only protocol route still answers 405, so nothing
    // re-routed `/v1/*`.
    let (status, _) = get(test_router(), "/v1/messages").await;
    assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);

    // And the scheduler routes are not reachable under `/v1`.
    let (status, _) = get(test_router(), "/v1/scheduler").await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    // A nested path under the run route is a 404 rather than matching loosely.
    let (status, _) = get(test_router(), "/api/scheduler/run").await;
    assert_ne!(status, StatusCode::OK);
}
