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
//! **No authentication on the read routes, and the router's CORS layer allows
//! every origin.** A page the user visits can therefore read their request log —
//! models, session ids, override trails, error text — from
//! `http://127.0.0.1:<port>/api/...`, and `PROXY_BIND=0.0.0.0` widens that to
//! the local network. That is an accepted, temporary trade-off for a
//! loopback-only developer tool.
//!
//! **The mutating route is the exception, and it is gated.** Reads are safe to
//! expose because reading is all an attacker gets; `POST /api/scheduler/run/*`
//! makes the gateway act on upstream accounts, so it requires a bearer token and
//! refuses any request carrying a browser `Origin` (see [`crate::api_token`]).
//! Any *future* mutating route under `/api` must go through the same guard — the
//! read trade-off does not extend to it.

use crate::error::ProxyResult;
use crate::settings::LogBuffer;
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
    typed_error_response(status, "stats_query_failed", message)
}

/// [`error_response`] with an explicit machine-readable `type`.
///
/// The scheduler routes need this: a client branches on `unknown_loop` versus
/// `already_running` to decide whether to fix its argument or wait, and folding
/// both into `stats_query_failed` would make that impossible.
fn typed_error_response(
    status: StatusCode,
    error_type: &'static str,
    message: impl Into<String>,
) -> Response {
    let body = json!({
        "error": {
            "type": error_type,
            "message": message.into(),
        }
    });
    (status, Json(body)).into_response()
}

/// `GET /api/scheduler`
///
/// The three background loops' switches, cadence, last result, and next
/// occurrence. Deliberately **not** gated on `ServiceController::is_running`,
/// for the same reason as the other `/api` routes: this is how an operator asks
/// what the process is doing, and a stopped listener answers nothing at all.
///
/// The blocking part is three small file reads plus a preference load, so it
/// runs on `spawn_blocking` like its siblings — the same runtime serves proxied
/// requests, and a slow disk must not stall forwarding.
pub async fn scheduler_handler() -> Response {
    match tokio::task::spawn_blocking(crate::scheduler::snapshot).await {
        Ok(payload) => json_response(payload),
        Err(e) => typed_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "scheduler_snapshot_failed",
            format!("定时任务快照任务异常终止: {e}"),
        ),
    }
}

/// Path segment accepted by [`scheduler_run_handler`].
#[derive(serde::Deserialize)]
pub struct SchedulerRunPath {
    /// One of [`crate::scheduler::LOOP_IDS`].
    pub loop_id: String,
}

/// `POST /api/scheduler/run/{loop_id}`
///
/// Starts one loop's work immediately and answers `202` without waiting for it.
/// The run is detached on purpose: a full-pool growth sweep takes tens of
/// seconds and the caller's next step is to read `GET /api/scheduler` for the
/// result, which is the same contract the JSON body documents.
///
/// Three gates, in this order, and the order is the decision:
///
/// 1. **Auth** — before the loop id is even examined, so an unauthenticated
///    caller learns nothing about which ids exist.
/// 2. **Known loop** — `404`, not `400`: the path names a resource that does not
///    exist, and the client's fix is a different URL.
/// 3. **Already running** — `409`, because the request is well-formed and the
///    conflict is transient.
pub async fn scheduler_run_handler(
    axum::extract::Path(path): axum::extract::Path<SchedulerRunPath>,
    headers: axum::http::HeaderMap,
    Extension(client): Extension<reqwest::Client>,
    Extension(logs): Extension<Arc<LogBuffer>>,
) -> Response {
    // Header values are checked as raw bytes-decoded strings; `to_str` fails on
    // non-ASCII, which is itself a reason to refuse rather than to guess.
    let origin = headers
        .get(header::ORIGIN)
        .map(|v| v.to_str().unwrap_or(""));
    let authorization = headers
        .get(header::AUTHORIZATION)
        .map(|v| v.to_str().unwrap_or(""));

    if let Err(failure) = crate::api_token::authorize(origin, authorization) {
        let (status, detail) = match failure {
            crate::api_token::AuthFailure::NotConfigured => (
                StatusCode::FORBIDDEN,
                format!(
                    "手动调用未启用：请在 ~/.proxy-rs/.env 或进程环境中设置 {} 后重启网关",
                    crate::api_token::TOKEN_ENV
                ),
            ),
            crate::api_token::AuthFailure::BrowserOrigin => (
                StatusCode::FORBIDDEN,
                "拒绝来自浏览器的调用：该接口只接受本机进程（Node / curl / launchd）的请求"
                    .to_string(),
            ),
            crate::api_token::AuthFailure::BadToken => (
                StatusCode::UNAUTHORIZED,
                format!(
                    "缺少或错误的 Authorization: Bearer 令牌（见 {}）",
                    crate::api_token::TOKEN_ENV
                ),
            ),
        };
        return typed_error_response(status, failure.slug(), detail);
    }

    let loop_id = path.loop_id.clone();
    if !crate::scheduler::is_known_loop(&loop_id) {
        return typed_error_response(
            StatusCode::NOT_FOUND,
            "unknown_loop",
            format!(
                "未知的定时任务 {}；可选：{}",
                loop_id,
                crate::scheduler::LOOP_IDS.join(" / ")
            ),
        );
    }

    let run_id = format!("{}-{}", crate::util::unix_millis(), loop_id);
    match crate::scheduler::run_once(&loop_id, client, logs).await {
        Ok(()) => (
            StatusCode::ACCEPTED,
            Json(json!({
                "accepted": true,
                "loop": loop_id,
                "run_id": run_id,
                "note": "已在后台开始；结果会出现在 GET /api/scheduler 的 last_run。",
            })),
        )
            .into_response(),
        Err(refusal) => {
            let status = match refusal {
                crate::scheduler::RunRefusal::UnknownLoop => StatusCode::NOT_FOUND,
                crate::scheduler::RunRefusal::AlreadyRunning => StatusCode::CONFLICT,
            };
            typed_error_response(
                status,
                refusal.slug(),
                format!("{loop_id} 上一次手动运行还没结束，请等它完成后再触发"),
            )
        }
    }
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
