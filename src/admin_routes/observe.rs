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

/// Local sessions, for scripts as well as the console.
///
/// The `无效的` / `不存在` → 404 rule below is a **contract**, not a convenience:
/// every "the caller asked for something that does not exist" message must start
/// with one of those two substrings, or a client error turns into a 500. New
/// source errors follow it (see `sessions::by_slug`).
pub(crate) fn not_found_or_internal(err: anyhow::Error) -> ApiError {
    let msg = err.to_string();
    if msg.contains("不存在") || msg.contains("无效的") {
        ApiError::not_found(msg)
    } else {
        ApiError::internal(msg)
    }
}

pub(crate) async fn sessions_list(
    Extension(state): Extension<Arc<AdminState>>,
    Query(q): Query<SessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let args = q.list_args();
    crate::admin::sessions_list(Some(state.stats.clone()), args)
        .await
        .map(Json)
        .map_err(not_found_or_internal)
}

/// The detail for a route with **no** source segment.
///
/// Two routes land here (`/admin/api/sessions/:id` and the 3.5.5
/// `/admin/api/claude-sessions/:id`) and both mean the default source — those
/// URLs could only ever have referred to the one source that existed when they
/// were published.
pub(crate) async fn session_detail_default(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Query(q): Query<SessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let args = q.detail_args();
    crate::admin::session_detail(Some(state.stats.clone()), None, id, args)
        .await
        .map(Json)
        .map_err(not_found_or_internal)
}

/// The detail for `/admin/api/sessions/:source/:id`.
///
/// A separate handler rather than `Path<(Option<String>, String)>`: axum's path
/// extractors do not support optional segments, and the failure mode is a 500
/// with an empty body — which reads as "the backend crashed" rather than "the
/// route was declared wrong". Two handlers with plain extractors cannot fail that
/// way.
pub(crate) async fn session_detail_sourced(
    Extension(state): Extension<Arc<AdminState>>,
    Path((source, id)): Path<(String, String)>,
    Query(q): Query<SessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let args = q.detail_args();
    crate::admin::session_detail(Some(state.stats.clone()), Some(source), id, args)
        .await
        .map(Json)
        .map_err(not_found_or_internal)
}

pub(crate) async fn session_subagent_default(
    Path((id, agent)): Path<(String, String)>,
    Query(q): Query<SessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let args = q.detail_args();
    crate::admin::session_subagent_detail(None, id, agent, args)
        .await
        .map(Json)
        .map_err(not_found_or_internal)
}

pub(crate) async fn session_subagent_sourced(
    Path((source, id, agent)): Path<(String, String, String)>,
    Query(q): Query<SessionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let args = q.detail_args();
    crate::admin::session_subagent_detail(Some(source), id, agent, args)
        .await
        .map(Json)
        .map_err(not_found_or_internal)
}

/// Query string of every session endpoint.
///
/// One struct for both list and detail because the REST surface has always
/// accepted `limit`/`offset` on both, and a second near-identical type would be
/// two places to add the next paging knob to. The extra list-only fields are
/// simply ignored by the detail handlers.
#[derive(Debug, Deserialize)]
pub(crate) struct SessionsQuery {
    limit: Option<usize>,
    offset: Option<usize>,
    group_by: Option<String>,
    group: Option<String>,
    group_limit: Option<usize>,
    source: Option<String>,
}

impl SessionsQuery {
    fn list_args(&self) -> crate::claude_sessions::ListArgs {
        crate::claude_sessions::ListArgs {
            limit: self.limit,
            offset: self.offset,
            group_by: self.group_by.clone(),
            group: self.group.clone(),
            group_limit: self.group_limit,
            source: self.source.clone(),
        }
    }

    fn detail_args(&self) -> crate::claude_sessions::DetailArgs {
        crate::claude_sessions::DetailArgs {
            limit: self.limit,
            offset: self.offset,
        }
    }
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
