//! Request-side plumbing: extraction, rejection, session/stats, credential refresh.

use super::*;

/// Extract the typed request from a peeked body, recording any rejection.
///
/// One entry point for both early failure modes, so neither can regress into an
/// unlogged `?`:
///
///   * the body never buffered (`peeked.error`) — a 413 over the cap, or a 400
///     for a read that failed. Answered directly, because handing the emptied
///     request to the extractor would replace the real cause with a misleading
///     "invalid JSON";
///   * the body buffered but `Json` rejected it — the rejection's own status is
///     preserved (400 unparseable, 415 wrong content-type, 422 shape mismatch)
///     instead of being flattened to a generic 400.
///
/// `route` is the one that handler serves, so a client talking to three APIs on
/// one port can tell which one refused it.
pub(crate) async fn extract_or_reject<T>(
    peeked: util::PeekedBody,
    route: &str,
    gui_logs: &Arc<crate::settings::LogBuffer>,
    stats: &Arc<StatsDb>,
    session: &SessionInfo,
    tag: &str,
    start: Instant,
) -> ProxyResult<Json<T>>
where
    T: serde::de::DeserializeOwned,
{
    if let Some(body_error) = &peeked.error {
        let (status, message) = body_error.parts();
        return Err(reject_request(
            ProxyError::Rejected { status, message },
            gui_logs,
            stats,
            route,
            session,
            tag,
            start,
        )
        .await);
    }

    match Json::from_request(peeked.request, &()).await {
        Ok(json) => Ok(json),
        Err(rejection) => {
            let error = util::rejection_error(route, &rejection);
            Err(reject_request(error, gui_logs, stats, route, session, tag, start).await)
        }
    }
}

/// Record a request rejected before translation, and return the error to send.
///
/// Both early exits call this — a body that could not be buffered, and a body
/// that buffered but failed typed extraction. They run before the service-state
/// check and before the header log line, so recording here is the only thing
/// that makes them visible: previously they returned through `?` with no entry
/// in `proxy.log`, the in-memory console or the stats DB. That is exactly how a
/// 400 "Failed to buffer the request body" reached a user with nothing on the
/// proxy side to explain it.
///
/// The gauge is deliberately untouched: `InFlightGuard` is created later in the
/// handler, so there is nothing here to balance.
///
/// `model` is unknown at this point — the body never yielded one — so the row
/// carries an empty model rather than a guess.
pub(crate) async fn reject_request(
    error: ProxyError,
    gui_logs: &Arc<crate::settings::LogBuffer>,
    stats: &Arc<StatsDb>,
    route: &str,
    session: &SessionInfo,
    tag: &str,
    start: Instant,
) -> ProxyError {
    let status = error.status().as_u16();
    let message = error.to_string();
    let duration_ms = start.elapsed().as_millis() as i64;

    record_stats_row(
        stats,
        RequestOutcome {
            model: "",
            client_model: "",
            route,
            tokens: &TokenRecord::default(),
            duration_ms,
            // Rejected before any upstream attempt: nothing was ever sent.
            timing: UpstreamTiming::default(),
            streamed: false,
            status,
            error: Some(&message),
            session_id: &session.session_id,
            parent_session_id: &session.parent_session_id,
            client: session.client.tag(),
            override_key: "",
            override_model: "",
            override_reason: "",
        },
    );
    gui_logs
        .push(
            "ERROR",
            format!(
                "POST {} rejected {}ms {} | {}",
                route, duration_ms, tag, message
            ),
        )
        .await;
    tracing::warn!("POST {} rejected: {} ({})", route, message, tag);

    error
}

/// Record one request row, reporting a failure instead of dropping it.
///
/// Every call site used to be `let _ = stats.record_request_log(..)`, which
/// makes a dead stats writer indistinguishable from a healthy one: requests
/// keep being served, so the user sees a perfectly working proxy whose 请求日志
/// and 用量统计 panels simply stop moving, with nothing anywhere to explain
/// why. (`record_request_log` fails when the writer thread is gone.)
///
/// The warning is emitted at most once per process — a dead writer fails every
/// subsequent request, and one line per request would bury the log it is meant
/// to explain.
pub(crate) fn record_stats_row(stats: &Arc<StatsDb>, outcome: RequestOutcome<'_>) {
    if let Err(err) = stats.record_request_log(outcome) {
        static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !WARNED.swap(true, std::sync::atomic::Ordering::Relaxed) {
            tracing::warn!(
                "request statistics are no longer being recorded: {} (the 请求日志/用量统计 views are frozen until the app is restarted)",
                err
            );
        }
    }
}

/// Resolve the session identity of an incoming request.
///
/// Header sources are read first (they are the only ones available on
/// body-less routes such as `GET /v1/models`), then the raw body fills the gap
/// for the two dialects that carry the id in the payload — Claude Code's
/// `metadata.user_id` and Codex's `prompt_cache_key`.
///
/// The client dialect is detected from the headers *before* the body is
/// consulted, so a body field can only ever confirm an identity, never relabel
/// a request that a different client clearly sent.
/// The ` client_account=…` suffix for a handler's INFO log line.
///
/// Returns an empty string when no account authenticated the request (gate off,
/// or an exempt path), so the line keeps one shape regardless of configuration —
/// a field that only sometimes appears is a field every log query has to special
/// case.
pub(crate) fn account_log_field(account: &str) -> String {
    if account.is_empty() {
        String::new()
    } else {
        format!(" client_account={account}")
    }
}

pub(crate) fn resolve_session(
    headers: &HeaderMap,
    body: Option<&serde_json::Value>,
) -> SessionInfo {
    let client = session::detect_client(headers);
    let body_info = body.and_then(|b| session::detect_from_body(client, b));
    session::merge(session::detect(headers), body_info)
}

/// HTTP status to report for a completed request.
///
/// Delegates to [`ProxyError::status`] so the log row and the response cannot
/// disagree; keeping a second table here is what let a 413 be recorded as a 400.
pub(crate) fn outcome_status(result: &ProxyResult<Response>) -> u16 {
    match result {
        Ok(resp) => resp.status().as_u16(),
        Err(err) => err.status().as_u16(),
    }
}

/// The error message to log for a completed request, if it failed.
pub(crate) fn outcome_error(result: &ProxyResult<Response>) -> Option<String> {
    result.as_ref().err().map(|e| e.to_string())
}

/// Record the outcome of a completed request in metrics, the GUI log buffer and
/// the daily stats database. Shared by all three API handlers.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn finalize_request(
    status: u16,
    error: Option<String>,
    gui_logs: &Arc<crate::settings::LogBuffer>,
    stats: &Arc<StatsDb>,
    route: &str,
    client_model: &str,
    is_streaming: bool,
    start: Instant,
    session: &SessionInfo,
    tag: &str,
    in_flight: metrics::InFlightGuard,
    overrides: &Arc<std::sync::Mutex<OverrideTrace>>,
    estimated_input_tokens: i64,
) {
    in_flight.finish(start, status);
    let overrides = overrides.lock().unwrap_or_else(|p| p.into_inner()).clone();

    // Success rows are written by the response writers themselves — the
    // StreamLedger on the streaming path and `non_streaming_response` on the
    // JSON path — because only they see the final token usage. Recording here
    // as well would double-count every successful request.
    match error {
        None => {
            gui_logs
                .push(
                    "INFO",
                    format!(
                        "POST {} ok model={} stream={} {}ms {}{}",
                        route,
                        client_model,
                        is_streaming,
                        start.elapsed().as_millis(),
                        tag,
                        overrides.log_suffix()
                    ),
                )
                .await;
        }
        Some(message) => {
            let duration_ms = start.elapsed().as_millis() as i64;
            // A `free` request that failed before upstream mapping still gets
            // billed under the node it would have used, so failures group with
            // successes instead of under the virtual name.
            let served_model = if client_model == crate::translate::pipeline::FREE_MODEL_NAME {
                crate::translate::pipeline::resolve_free_model()
            } else {
                client_model.to_string()
            };
            // Record the failed request in the stats DB with estimated input tokens.
            let failed_tokens = TokenRecord {
                input: estimated_input_tokens,
                cache_read: 0,
                cache_write: 0,
                output: 0,
            };
            record_stats_row(
                stats,
                RequestOutcome {
                    model: &served_model,
                    client_model,
                    route,
                    tokens: &failed_tokens,
                    duration_ms,
                    // A failure may have come from any attempt; the successful
                    // writers own the real timings, so this stays unknown.
                    timing: UpstreamTiming::default(),
                    streamed: is_streaming,
                    status,
                    error: Some(&message),
                    session_id: &session.session_id,
                    parent_session_id: &session.parent_session_id,
                    client: session.client.tag(),
                    override_key: &overrides.key,
                    override_model: &overrides.model,
                    override_reason: &overrides.reason(),
                },
            );
            gui_logs
                .push(
                    "ERROR",
                    format!(
                        "POST {} failed model={} stream={} {}ms {}{} | {}",
                        route,
                        client_model,
                        is_streaming,
                        duration_ms,
                        tag,
                        overrides.log_suffix(),
                        message
                    ),
                )
                .await;
        }
    }
}

/// What an exception override (failover session switch, degraded retry, token
/// refresh, stream upgrade) changed about one request.
///
/// The retry chain folds its events into these fields. `key` and `model` are the
/// *served* values, kept independent of each other and each with its own reason,
/// so a model override can never erase the credential reason (or vice versa) —
/// sharing one `reason` slot made the log drop half of a two-part override.
/// Empty fields mean the request ran exactly as configured.
#[derive(Debug, Clone, Default)]
pub(crate) struct OverrideTrace {
    /// Credential short id actually served (the `override_key` column). Set
    /// whenever it differs from the configured default, not merely once.
    pub key: String,
    /// Why the credential differs, e.g. `429 from wb-a1` / `sticky failover`.
    pub key_reason: String,
    /// Upstream model actually served when it was changed (the
    /// `override_model` column).
    pub model: String,
    /// Why the model was changed, e.g. `content_blocked 400`.
    pub model_reason: String,
}

impl OverrideTrace {
    /// Record that the request was served by a credential other than the
    /// configured default. Always overwrites, so the column keeps reporting the
    /// override on every subsequent request rather than only the first one.
    pub(crate) fn set_key(&mut self, key: impl Into<String>, reason: impl fmt::Display) {
        self.key = key.into();
        self.key_reason = reason.to_string();
    }

    /// Record a model-level override (degraded retry / stream upgrade).
    pub(crate) fn set_model(&mut self, model: impl Into<String>, reason: impl fmt::Display) {
        self.model = model.into();
        self.model_reason = reason.to_string();
    }

    /// One-line combined reason for the log suffix and the `override_reason`
    /// column. Both halves are reported when both fired.
    pub(crate) fn reason(&self) -> String {
        match (self.key_reason.is_empty(), self.model_reason.is_empty()) {
            (false, false) => format!("{}; {}", self.key_reason, self.model_reason),
            (false, true) => self.key_reason.clone(),
            (true, false) => self.model_reason.clone(),
            (true, true) => String::new(),
        }
    }

    /// ` override=wb-b1@glm-5.3(429 from wb-a1; content_blocked 400)` for log
    /// lines; empty when the request ran on its configured credential/model.
    pub(crate) fn log_suffix(&self) -> String {
        if self.key.is_empty() && self.model.is_empty() {
            return String::new();
        }
        let what = match (self.key.is_empty(), self.model.is_empty()) {
            (false, false) => format!("{}@{}", self.key, self.model),
            (false, true) => self.key.clone(),
            (true, false) => self.model.clone(),
            (true, true) => String::new(),
        };
        let reason = self.reason();
        if reason.is_empty() {
            format!(" override={}", what)
        } else {
            format!(" override={}({})", what, reason)
        }
    }
}

/// Record that the picked credential differs from the configured default.
///
/// Shared by all three API handlers. The comparison is against the pool's
/// *configured* default (pool order), not the first healthy entry, so the label
/// persists for every request the session is relocated — see
/// [`crate::session_pool::CredentialPool::configured_default_id`].
pub(crate) async fn record_credential_override(
    pool: &crate::session_pool::SharedCredentialPool,
    credential: &Option<crate::workbuddy_auth::WorkBuddyCredential>,
    overrides: &Arc<std::sync::Mutex<OverrideTrace>>,
) {
    let Some(cred) = credential else {
        return;
    };
    let default_id = pool.configured_default_id().await;
    if default_id.as_deref() != Some(cred.id.as_str()) {
        if let Ok(mut t) = overrides.lock() {
            t.set_key(cred.id.clone(), "非默认身份(sticky)");
        }
    }
}

/// Renew one WorkBuddy access token and push the fresh copy into the pool.
///
/// The login-state credential is useless once its access token expires; the
/// gateway answers `401`. The refresh endpoint (and its `refreshToken`) is the
/// only way back, so a 401 must renew before it fails over — otherwise a single
/// account just surfaces the 401 to the client. On success the pool entry is
/// replaced in place so the next attempt (and every later request) uses the new
/// token without a reload; on failure the credential is parked briefly so a
/// burst of requests does not hammer the refresh endpoint.
pub(crate) async fn refresh_workbuddy_credential(
    client: &Client,
    cred: &crate::workbuddy_auth::WorkBuddyCredential,
    pool: &crate::session_pool::SharedCredentialPool,
    gui_logs: &Arc<crate::settings::LogBuffer>,
    tag: &str,
) -> Result<crate::workbuddy_auth::WorkBuddyCredential, String> {
    let endpoint = crate::workbuddy_auth::auth_endpoint_for(cred);
    let mut fresh = cred.clone();
    match crate::workbuddy_auth::refresh_credential(client, &endpoint, &mut fresh).await {
        Ok(()) => {
            pool.update_credential(fresh.clone()).await;
            gui_logs
                .push(
                    "INFO",
                    format!(
                        "已刷新 WorkBuddy 访问令牌 id={} endpoint={} {}",
                        fresh.id, endpoint, tag
                    ),
                )
                .await;
            Ok(fresh)
        }
        Err(e) => {
            let msg = e.to_string();
            pool.mark_refresh_failed(&cred.id, 60_000, &msg).await;
            // Persist the verdict as well as parking the pool entry. The
            // in-memory cooldown expires in 60s and dies with the process, but
            // "this account needs a new login" is a lasting fact the GUI badge
            // must keep showing — otherwise the user only learns about a revoked
            // credential when a request fails.
            let needs_relogin = crate::workbuddy_auth::refresh_failure_needs_relogin(cred, &msg);
            crate::workbuddy_auth::mark_credential_refresh_failure(cred, &msg, needs_relogin);
            gui_logs
                .push(
                    "WARN",
                    format!(
                        "刷新 WorkBuddy 访问令牌失败 id={} endpoint={} {} | {}{}",
                        cred.id,
                        endpoint,
                        tag,
                        msg,
                        if needs_relogin {
                            "（需要重新登录该账号）"
                        } else {
                            ""
                        }
                    ),
                )
                .await;
            Err(msg)
        }
    }
}
