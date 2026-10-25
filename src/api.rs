//! Read-only observation endpoints for the dashboard (`/api/*`).
//!
//! The desktop window used to read its two data-heavy panels — the overview's
//! 统计 row and the 请求日志 table — exclusively through Tauri commands. Those
//! commands still exist, but the same payloads are now also served over HTTP so
//! that the window, a headless deployment and any external tool share one
//! interface instead of three copies of the same SQL and the same JSON shaping.
//!
//! **One shape, two transports.** [`stats_today_json`] and
//! [`request_logs_json`] are the single definition of each payload; the Tauri
//! commands in `src-tauri/src/main.rs` and the handlers here both call them. A
//! second hand-rolled mapping is how the IPC and HTTP views would silently
//! drift apart (`42%` over IPC versus `42.0` over HTTP), which is exactly the
//! failure this module exists to prevent.
//!
//! **Path namespace.** These live under `/api`, never under `/v1`. `/v1/*` is
//! the namespace upstream clients are pointed at (Anthropic Messages, OpenAI
//! Responses, Chat Completions); a `/v1/usage` would read as part of that proxy
//! protocol rather than as this process's own diagnostics, and would collide
//! with the day the upstream grows a real endpoint of that name.
//!
//! **Not gated on [`crate::service::ServiceController::is_running`].** Unlike the
//! forwarding routes, this is not the proxy data path: answering "how much did I
//! use today" is exactly what an operator wants while the listener is up, and a
//! stopped service closes the port anyway — so the check would only fire in the
//! brief window where the port exists but the running flag lags behind.
//!
//! **No authentication, and the router's CORS layer allows every origin.** A
//! page the user visits can therefore read their request log — models, session
//! ids, override trails, error text — from `http://127.0.0.1:<port>/api/...`,
//! and `PROXY_BIND=0.0.0.0` widens that to the local network. That is an
//! accepted, temporary trade-off for a loopback-only developer tool; anything
//! reachable from a shared host needs a token or an origin allowlist first.

use crate::error::ProxyResult;
use crate::stats::{DayStats, RequestLogFilter, RequestLogsResult, StatsDb};
use axum::{
    extract::Query,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    Extension, Json,
};
use serde_json::{json, Value};
use std::sync::Arc;

/// Today's counters, as the overview's statistics row reads them.
///
/// Every counter is a plain number — no `Option`, no formatted string: the
/// window decides how to abbreviate (`1.2K`) and how to render the cache rate.
/// `tokens_total` is derived ([`DayStats::tokens_total`]) rather than stored, so
/// the four token categories and their sum can never disagree.
pub fn stats_today_json(today: &DayStats) -> Value {
    json!({
        "date": today.date,
        "requests_total": today.requests_total,
        "requests_success": today.requests_success,
        "requests_failed": today.requests_failed,
        "tokens_total": today.tokens_total(),
        "tokens_input": today.tokens_input,
        "tokens_cache_read": today.tokens_cache_read,
        "tokens_cache_write": today.tokens_cache_write,
        "tokens_output": today.tokens_output,
        "cache_hit_pct": today.cache_hit_pct(),
        "overrides_total": today.overrides_total,
    })
}

/// One page of request logs, with the facet lists its filter dropdowns need.
///
/// The struct already carries the wire contract (`#[derive(Serialize)]`), so this
/// is a plain serialization: the value here is that both transports go through
/// *this* function instead of one of them serializing the struct directly and
/// the other hand-building an object beside it.
pub fn request_logs_json(res: &RequestLogsResult) -> ProxyResult<Value> {
    Ok(serde_json::to_value(res)?)
}

/// `GET /api/stats/today`
pub async fn stats_today_handler(Extension(stats): Extension<Arc<StatsDb>>) -> Response {
    // SQLite is blocking and this runs on the same runtime that serves proxied
    // requests: a slow query parked on a worker thread would stall forwarding.
    // Same reason the Tauri command uses `spawn_blocking`.
    match tokio::task::spawn_blocking(move || stats.query_today()).await {
        Ok(Ok(today)) => json_response(stats_today_json(&today)),
        Ok(Err(e)) => error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("统计查询任务异常终止: {e}"),
        ),
    }
}

/// `GET /api/request-logs?model=&client_model=&client=&session_id=&status_group=&streamed=&search=&limit=&offset=`
///
/// The query string deserializes straight into [`RequestLogFilter`], so the
/// filter vocabulary is the struct's field names — one list to keep current,
/// shared with the Tauri command that receives the same struct from the window.
pub async fn request_logs_handler(
    Extension(stats): Extension<Arc<StatsDb>>,
    Query(filter): Query<RequestLogFilter>,
) -> Response {
    match tokio::task::spawn_blocking(move || stats.query_request_logs(&filter)).await {
        Ok(Ok(res)) => match request_logs_json(&res) {
            Ok(payload) => json_response(payload),
            Err(e) => error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        },
        Ok(Err(e)) => error_response(StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        Err(e) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("请求日志查询任务异常终止: {e}"),
        ),
    }
}

/// A successful observation payload.
///
/// `no-store` because these are live counters: a browser caching "today's
/// requests" would show a number that silently stops moving.
fn json_response(payload: Value) -> Response {
    (
        StatusCode::OK,
        [(header::CACHE_CONTROL, "no-store")],
        Json(payload),
    )
        .into_response()
}

/// An error the caller can read, in the same envelope the proxy routes use
/// (`{"error": {"type", "message"}}`) rather than a bare string.
fn error_response(status: StatusCode, message: impl Into<String>) -> Response {
    let body = json!({
        "error": {
            "type": "stats_query_failed",
            "message": message.into(),
        }
    });
    (status, Json(body)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stats payload's key set is a contract: both transports and every
    /// external consumer read these names, so a rename must be a deliberate
    /// edit here rather than a field silently disappearing from one caller.
    #[test]
    fn stats_payload_exposes_the_documented_counters() {
        let today = DayStats {
            date: "2026-10-20".to_string(),
            requests_total: 7,
            requests_success: 6,
            requests_failed: 1,
            tokens_input: 100,
            tokens_cache_read: 40,
            tokens_cache_write: 10,
            tokens_output: 50,
            overrides_total: 2,
        };

        let payload = stats_today_json(&today);

        let mut keys: Vec<&str> = payload
            .as_object()
            .expect("stats payload is an object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "cache_hit_pct",
                "date",
                "overrides_total",
                "requests_failed",
                "requests_success",
                "requests_total",
                "tokens_cache_read",
                "tokens_cache_write",
                "tokens_input",
                "tokens_output",
                "tokens_total",
            ]
        );

        // Totals are derived, so the sum and its parts cannot drift.
        assert_eq!(payload["tokens_total"], 100 + 40 + 10 + 50);
        // 40 / (100 + 40) = 28% — integer division, matching the card's `28%`.
        assert_eq!(payload["cache_hit_pct"], 28);
    }
}
