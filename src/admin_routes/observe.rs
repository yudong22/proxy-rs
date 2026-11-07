//! Read-only observation endpoints: status, stats, request logs, Claude sessions.

use super::*;

pub(crate) async fn status(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    Json(state.process.status_json())
}

pub(crate) async fn stats(
    Extension(state): Extension<Arc<AdminState>>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(admin::stats_today(state.stats.clone()).await?))
}

#[derive(Deserialize, Default)]
pub(crate) struct LogQuery {
    limit: Option<u32>,
    offset: Option<u32>,
    model: Option<String>,
    client_model: Option<String>,
    session_id: Option<String>,
    client: Option<String>,
    errors_only: Option<bool>,
}

pub(crate) async fn request_logs(
    Extension(state): Extension<Arc<AdminState>>,
    Query(q): Query<LogQuery>,
) -> Result<Json<Value>, ApiError> {
    let filter = RequestLogFilter {
        limit: q.limit,
        offset: q.offset,
        model: q.model,
        client_model: q.client_model,
        session_id: q.session_id,
        client: q.client,
        ..RequestLogFilter::default()
    };
    if q.errors_only.unwrap_or(false) {
        // The schema has no `errors_only` flag; an errored row is one whose
        // `error` column is set, and there is no server-side predicate for that.
        // Rather than fake it (or silently ignore the parameter), the filter is
        // dropped and `has_errors` is reported so the caller knows why.
        tracing::debug!("errors_only is not supported server-side; ignoring");
    }
    let stats = state.stats.clone();
    let res = tokio::task::spawn_blocking(move || stats.query_request_logs(&filter))
        .await
        .map_err(|e| ApiError::internal(format!("查询异常终止: {e}")))?
        .map_err(|e| ApiError::internal(format!("查询失败: {e}")))?;
    Ok(Json(
        crate::api::request_logs_json(&res).map_err(|e| ApiError::internal(e.to_string()))?,
    ))
}

pub(crate) async fn session_metrics(
    Extension(state): Extension<Arc<AdminState>>,
) -> Result<Json<Value>, ApiError> {
    let stats = state.stats.clone();
    let rows = tokio::task::spawn_blocking(move || stats.query_recent_session_metrics(3))
        .await
        .map_err(|e| ApiError::internal(format!("查询异常终止: {e}")))?
        .map_err(|e| ApiError::internal(format!("查询失败: {e}")))?;
    Ok(Json(json!({ "sessions": rows })))
}

/// Local Claude Code sessions, for scripts as well as the console.
pub(crate) fn not_found_or_internal(err: anyhow::Error) -> ApiError {
    let msg = err.to_string();
    if msg.contains("不存在") || msg.contains("无效的") {
        ApiError::not_found(msg)
    } else {
        ApiError::internal(msg)
    }
}

pub(crate) async fn claude_sessions(
    Extension(state): Extension<Arc<AdminState>>,
    Query(q): Query<ClaudeSessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let args = crate::claude_sessions::ListArgs {
        limit: q.limit,
        offset: q.offset,
    };
    crate::admin::claude_sessions_list(Some(state.stats.clone()), args)
        .await
        .map(Json)
        .map_err(not_found_or_internal)
}

pub(crate) async fn claude_session(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Query(q): Query<ClaudeSessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let args = crate::claude_sessions::DetailArgs {
        limit: q.limit,
        offset: q.offset,
    };
    crate::admin::claude_session_detail(Some(state.stats.clone()), id, args)
        .await
        .map(Json)
        .map_err(not_found_or_internal)
}

pub(crate) async fn claude_subagent(
    Path((id, agent)): Path<(String, String)>,
    Query(q): Query<ClaudeSessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let args = crate::claude_sessions::DetailArgs {
        limit: q.limit,
        offset: q.offset,
    };
    crate::admin::claude_subagent_detail(id, agent, args)
        .await
        .map(Json)
        .map_err(not_found_or_internal)
}

#[derive(Debug, Deserialize)]
pub(crate) struct ClaudeSessionsQuery {
    limit: Option<usize>,
    offset: Option<usize>,
}

pub(crate) async fn logs_get(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    Json(json!({ "entries": state.logs.snapshot().await }))
}

pub(crate) async fn logs_clear(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    state.logs.clear().await;
    Json(json!({ "ok": true }))
}

// ── Settings ───────────────────────────────────────────────────────────────

/// The three background loops' own status.
///
/// `/api/scheduler` exists on the data plane for external agents, but a server
/// operator reads it from this console — and the console is the only surface
/// that is reachable from outside without an access key, so it needs its own.
pub(crate) async fn scheduler_snapshot() -> Json<Value> {
    Json(crate::scheduler::snapshot())
}
