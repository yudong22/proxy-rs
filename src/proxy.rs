use crate::config::{Config, ModelsFlavor};
use crate::error::{ProxyError, ProxyResult};
use crate::metrics;
use crate::models::{anthropic, openai, responses};
use crate::service;
use crate::session::{self, ClientKind, SessionInfo};
use crate::stats::{RequestOutcome, StatsDb, TokenRecord};
use crate::translate::{pipeline, responses as responses_pipeline, stream};
use crate::util::{self, format_headers, truncate};
use axum::{
    body::Body,
    extract::{FromRequest, Request},
    http::{HeaderMap, HeaderName, HeaderValue},
    response::{IntoResponse, Response},
    Extension, Json,
};
use bytes::Bytes;
use futures::stream::{Stream, StreamExt};
use reqwest::Client;
use std::collections::BTreeMap;
use std::fmt;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Which wire protocol a request was accepted on. The three supported APIs
/// share the entire upstream send/retry path and differ only in how the request
/// was translated and how the response is rendered back.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ApiFlavor {
    /// Anthropic Messages API (`/v1/messages`).
    Anthropic,
    /// OpenAI Responses API (`/v1/responses`).
    Responses,
    /// OpenAI Chat Completions passthrough (`/v1/chat/completions`).
    Chat,
}

impl ApiFlavor {
    /// Log stage tag used for upstream failures.
    fn stage(self) -> &'static str {
        match self {
            ApiFlavor::Anthropic | ApiFlavor::Chat => "chat/completions",
            ApiFlavor::Responses => "responses/chat_completions",
        }
    }

    /// Human-readable name used in `tracing` debug lines.
    fn label(self) -> &'static str {
        match self {
            ApiFlavor::Anthropic => "",
            ApiFlavor::Responses => "Responses API ",
            ApiFlavor::Chat => "Chat Completions ",
        }
    }

    /// Whether SSE responses advertise CORS. The Anthropic endpoint is consumed
    /// by CLI clients; the OpenAI-compatible ones are also called from browsers.
    fn sse_allows_cors(self) -> bool {
        !matches!(self, ApiFlavor::Anthropic)
    }
}

#[allow(clippy::too_many_arguments)]
pub async fn proxy_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(gui_logs): Extension<Arc<crate::settings::LogBuffer>>,
    Extension(service): Extension<Arc<service::ServiceController>>,
    Extension(stats): Extension<Arc<StatsDb>>,
    Extension(pool): Extension<crate::session_pool::SharedCredentialPool>,
    headers: HeaderMap,
    request: Request,
) -> ProxyResult<Response> {
    let start = Instant::now();
    // Resolve the session before the body is consumed by `Json`. The raw bytes
    // are parsed because the translation type deliberately drops
    // `metadata.user_id` — the only session id Claude Code sends.
    let limit = util::max_body_bytes();
    let peeked = util::peek_json_body(request, limit).await;
    let session = resolve_session(&headers, peeked.json.as_ref());
    let tag = session.log_tag();

    let Json(req): Json<anthropic::AnthropicRequest> = extract_or_reject(
        peeked,
        "/v1/messages",
        &gui_logs,
        &stats,
        &session,
        &tag,
        start,
    )
    .await?;

    let is_streaming = req.stream.unwrap_or(false);

    let incoming_headers = format_headers(&headers);
    gui_logs
        .push(
            "INFO",
            format!("POST /v1/messages {} headers: {}", tag, incoming_headers),
        )
        .await;
    tracing::info!("POST /v1/messages {} headers: {}", tag, incoming_headers);

    // The console shares this listener; a stopped service must not take it
    // offline, so answer with 503 instead of closing the port.
    if !service.is_running() {
        return Ok(service::service_unavailable_response());
    }

    let api_key = resolve_api_key(&config, &headers);

    tracing::debug!("Received request for model: {}", req.model);
    tracing::debug!("Streaming: {}", is_streaming);
    // Held for the whole handler: any early return still balances the gauge.
    let in_flight = metrics::InFlightGuard::start(is_streaming);

    if config.verbose {
        tracing::trace!(
            "Incoming Anthropic request: {}",
            serde_json::to_string_pretty(&req).unwrap_or_default()
        );
    }

    let policy = translation_policy(&config);
    // Capture what the log lines need before `req` is consumed by translation.
    let client_model = req.model.clone();
    let message_count = req.messages.len();
    let tool_count = req.tools.as_ref().map(|t| t.len()).unwrap_or(0);
    // A translation failure is a real 400 the client does not see otherwise:
    // record it like any other outcome rather than letting `?` skip the log row.
    let openai_req = match pipeline::translate_request(req, &policy) {
        Ok(req) => req,
        Err(err) => {
            finalize_request(
                400,
                Some(err.to_string()),
                &gui_logs,
                &stats,
                "/v1/messages",
                &client_model,
                is_streaming,
                start,
                &session,
                &tag,
                in_flight,
                &Arc::new(std::sync::Mutex::new(OverrideTrace::default())),
            )
            .await;
            return Err(err);
        }
    };

    if config.verbose {
        tracing::trace!(
            "Transformed OpenAI request: {}",
            serde_json::to_string_pretty(&openai_req).unwrap_or_default()
        );
    }

    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/messages {} model={} -> upstream={} stream={} msgs={} tools={}",
                tag, client_model, openai_req.model, is_streaming, message_count, tool_count
            ),
        )
        .await;

    // Folded by forward_request: which credential/model an exception override
    // ended up using, for the `override_key`/`override_model` DB columns.
    let overrides = Arc::new(std::sync::Mutex::new(OverrideTrace::default()));

    // Credential-pool selection: default-first for every healthy session, the
    // session's sticky replacement after a failover, or None when the pool is
    // disabled (static key / passthrough path, behaviour unchanged from 1.7.1).
    let credential = if config.credential_pool_enabled && pool.is_enabled().await {
        pool.pick(&session.session_id).await
    } else {
        None
    };
    // A credential other than the configured default is an override, and stays
    // labelled as one on every request it serves (not just the first).
    record_credential_override(&pool, &credential, &overrides).await;
    let result = forward_request(
        config,
        client,
        openai_req,
        api_key,
        gui_logs.clone(),
        client_model.clone(),
        stats.clone(),
        ApiFlavor::Anthropic,
        is_streaming,
        "/v1/messages",
        start,
        session.clone(),
        tag.clone(),
        Arc::clone(&overrides),
        Arc::clone(&pool),
        credential,
    )
    .await;

    finalize_request(
        outcome_status(&result),
        outcome_error(&result),
        &gui_logs,
        &stats,
        "/v1/messages",
        &client_model,
        is_streaming,
        start,
        &session,
        &tag,
        in_flight,
        &overrides,
    )
    .await;

    result
}

#[allow(clippy::too_many_arguments)]
pub async fn responses_proxy_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(gui_logs): Extension<Arc<crate::settings::LogBuffer>>,
    Extension(service): Extension<Arc<service::ServiceController>>,
    Extension(stats): Extension<Arc<StatsDb>>,
    Extension(pool): Extension<crate::session_pool::SharedCredentialPool>,
    headers: HeaderMap,
    request: Request,
) -> ProxyResult<Response> {
    let start = Instant::now();
    let limit = util::max_body_bytes();
    let peeked = util::peek_json_body(request, limit).await;
    let session = resolve_session(&headers, peeked.json.as_ref());
    let tag = session.log_tag();

    let Json(req): Json<responses::ResponsesRequest> = extract_or_reject(
        peeked,
        "/v1/responses",
        &gui_logs,
        &stats,
        &session,
        &tag,
        start,
    )
    .await?;

    let is_streaming = req.stream.unwrap_or(false);

    let incoming_headers = format_headers(&headers);
    gui_logs
        .push(
            "INFO",
            format!("POST /v1/responses {} headers: {}", tag, incoming_headers),
        )
        .await;
    tracing::info!("POST /v1/responses {} headers: {}", tag, incoming_headers);

    if !service.is_running() {
        return Ok(service::service_unavailable_response());
    }

    let api_key = resolve_responses_api_key(&config, &headers);

    tracing::debug!("Received Responses API request for model: {}", req.model);
    tracing::debug!("Streaming: {}", is_streaming);
    // Held for the whole handler: any early return still balances the gauge.
    let in_flight = metrics::InFlightGuard::start(is_streaming);

    if config.verbose {
        tracing::trace!(
            "Incoming Responses API request: {}",
            serde_json::to_string_pretty(&req).unwrap_or_default()
        );
    }

    let policy = translation_policy(&config);
    let client_model = req.model.clone();
    // A translation failure is a real 400 the client does not see otherwise:
    // record it like any other outcome rather than letting `?` skip the log row.
    let openai_req = match responses_pipeline::translate_responses_request(req, &policy) {
        Ok(req) => req,
        Err(err) => {
            finalize_request(
                400,
                Some(err.to_string()),
                &gui_logs,
                &stats,
                "/v1/responses",
                &client_model,
                is_streaming,
                start,
                &session,
                &tag,
                in_flight,
                &Arc::new(std::sync::Mutex::new(OverrideTrace::default())),
            )
            .await;
            return Err(err);
        }
    };

    if config.verbose {
        tracing::trace!(
            "Transformed OpenAI request from Responses API: {}",
            serde_json::to_string_pretty(&openai_req).unwrap_or_default()
        );
    }

    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/responses {} model={} -> upstream={} stream={}",
                tag, client_model, openai_req.model, is_streaming
            ),
        )
        .await;

    // Folded by forward_request: which credential/model an exception override
    // ended up using, for the `override_key`/`override_model` DB columns.
    let overrides = Arc::new(std::sync::Mutex::new(OverrideTrace::default()));

    // Credential-pool selection: default-first for every healthy session, the
    // session's sticky replacement after a failover, or None when the pool is
    // disabled (static key / passthrough path, behaviour unchanged from 1.7.1).
    let credential = if config.credential_pool_enabled && pool.is_enabled().await {
        pool.pick(&session.session_id).await
    } else {
        None
    };
    // A credential other than the configured default is an override, and stays
    // labelled as one on every request it serves (not just the first).
    record_credential_override(&pool, &credential, &overrides).await;
    let result = forward_request(
        config,
        client,
        openai_req,
        api_key,
        gui_logs.clone(),
        client_model.clone(),
        stats.clone(),
        ApiFlavor::Responses,
        is_streaming,
        "/v1/responses",
        start,
        session.clone(),
        tag.clone(),
        Arc::clone(&overrides),
        Arc::clone(&pool),
        credential,
    )
    .await;

    finalize_request(
        outcome_status(&result),
        outcome_error(&result),
        &gui_logs,
        &stats,
        "/v1/responses",
        &client_model,
        is_streaming,
        start,
        &session,
        &tag,
        in_flight,
        &overrides,
    )
    .await;

    result
}

#[allow(clippy::too_many_arguments)]
pub async fn chat_completions_proxy_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(gui_logs): Extension<Arc<crate::settings::LogBuffer>>,
    Extension(service): Extension<Arc<service::ServiceController>>,
    Extension(stats): Extension<Arc<StatsDb>>,
    Extension(pool): Extension<crate::session_pool::SharedCredentialPool>,
    headers: HeaderMap,
    request: Request,
) -> ProxyResult<Response> {
    let start = Instant::now();
    let limit = util::max_body_bytes();
    let peeked = util::peek_json_body(request, limit).await;
    let session = resolve_session(&headers, peeked.json.as_ref());
    let tag = session.log_tag();

    let Json(mut req): Json<openai::OpenAIRequest> = extract_or_reject(
        peeked,
        "/v1/chat/completions",
        &gui_logs,
        &stats,
        &session,
        &tag,
        start,
    )
    .await?;

    let is_streaming = req.stream.unwrap_or(false);

    let incoming_headers = format_headers(&headers);
    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/chat/completions {} headers: {}",
                tag, incoming_headers
            ),
        )
        .await;
    tracing::info!(
        "POST /v1/chat/completions {} headers: {}",
        tag,
        incoming_headers
    );

    if !service.is_running() {
        return Ok(service::service_unavailable_response());
    }

    let api_key = resolve_chat_api_key(&config, &headers);

    tracing::debug!("Received Chat Completions request for model: {}", req.model);
    tracing::debug!("Streaming: {}", is_streaming);
    // Held for the whole handler: any early return still balances the gauge.
    let in_flight = metrics::InFlightGuard::start(is_streaming);

    if config.verbose {
        tracing::trace!(
            "Incoming Chat Completions request: {}",
            serde_json::to_string_pretty(&req).unwrap_or_default()
        );
    }

    let policy = translation_policy(&config);
    let client_model = req.model.clone();

    // Same resolution as the other two routes, so a request cannot reach a
    // different upstream model depending on which endpoint it arrives at. This
    // path has no `thinking` flag — OpenAI-format clients select reasoning via
    // the model name itself — so it resolves against `completion_model`.
    req.model =
        pipeline::resolve_upstream_model(&req.model, policy.completion_model.as_ref(), &policy);

    // 2. Sanitize system prompt if ignore terms are configured
    if !policy.ignore_terms.is_empty() {
        for msg in &mut req.messages {
            if msg.role == "system" {
                if let Some(openai::MessageContent::Text(ref mut text)) = msg.content {
                    *text = pipeline::sanitize_prompt(text.clone(), &policy.ignore_terms);
                }
            }
        }
    }

    // 3. `stream` / `stream_options` are set by `forward_request`, which owns
    //    the decision (it also upgrades non-streaming requests for providers
    //    that only accept streams).

    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/chat/completions {} model={} -> upstream={} stream={}",
                tag, client_model, req.model, is_streaming
            ),
        )
        .await;

    // Folded by forward_request: which credential/model an exception override
    // ended up using, for the `override_key`/`override_model` DB columns.
    let overrides = Arc::new(std::sync::Mutex::new(OverrideTrace::default()));

    // Credential-pool selection: default-first for every healthy session, the
    // session's sticky replacement after a failover, or None when the pool is
    // disabled (static key / passthrough path, behaviour unchanged from 1.7.1).
    let credential = if config.credential_pool_enabled && pool.is_enabled().await {
        pool.pick(&session.session_id).await
    } else {
        None
    };
    // A credential other than the configured default is an override, and stays
    // labelled as one on every request it serves (not just the first).
    record_credential_override(&pool, &credential, &overrides).await;
    let result = forward_request(
        config,
        client,
        req,
        api_key,
        gui_logs.clone(),
        client_model.clone(),
        stats.clone(),
        ApiFlavor::Chat,
        is_streaming,
        "/v1/chat/completions",
        start,
        session.clone(),
        tag.clone(),
        Arc::clone(&overrides),
        Arc::clone(&pool),
        credential,
    )
    .await;

    finalize_request(
        outcome_status(&result),
        outcome_error(&result),
        &gui_logs,
        &stats,
        "/v1/chat/completions",
        &client_model,
        is_streaming,
        start,
        &session,
        &tag,
        in_flight,
        &overrides,
    )
    .await;

    result
}

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
async fn extract_or_reject<T>(
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
async fn reject_request(
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

    let _ = stats.record_request_log(RequestOutcome {
        model: "",
        route,
        tokens: &TokenRecord::default(),
        duration_ms,
        streamed: false,
        status,
        error: Some(&message),
        session_id: &session.session_id,
        client: session.client.tag(),
        override_key: "",
        override_model: "",
        override_reason: "",
    });
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
fn resolve_session(headers: &HeaderMap, body: Option<&serde_json::Value>) -> SessionInfo {
    let client = session::detect_client(headers);
    let body_info = body.and_then(|b| session::detect_from_body(client, b));
    session::merge(session::detect(headers), body_info)
}

/// HTTP status to report for a completed request.
///
/// Delegates to [`ProxyError::status`] so the log row and the response cannot
/// disagree; keeping a second table here is what let a 413 be recorded as a 400.
fn outcome_status(result: &ProxyResult<Response>) -> u16 {
    match result {
        Ok(resp) => resp.status().as_u16(),
        Err(err) => err.status().as_u16(),
    }
}

/// The error message to log for a completed request, if it failed.
fn outcome_error(result: &ProxyResult<Response>) -> Option<String> {
    result.as_ref().err().map(|e| e.to_string())
}

/// Record the outcome of a completed request in metrics, the GUI log buffer and
/// the daily stats database. Shared by all three API handlers.
#[allow(clippy::too_many_arguments)]
async fn finalize_request(
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
            // Record the failed request in the stats DB (no tokens) and request_logs.
            let _ = stats.record_request_log(RequestOutcome {
                model: client_model,
                route,
                tokens: &TokenRecord::default(),
                duration_ms,
                streamed: is_streaming,
                status,
                error: Some(&message),
                session_id: &session.session_id,
                client: session.client.tag(),
                override_key: &overrides.key,
                override_model: &overrides.model,
                override_reason: &overrides.reason(),
            });
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
    fn set_key(&mut self, key: impl Into<String>, reason: impl fmt::Display) {
        self.key = key.into();
        self.key_reason = reason.to_string();
    }

    /// Record a model-level override (degraded retry / stream upgrade).
    fn set_model(&mut self, model: impl Into<String>, reason: impl fmt::Display) {
        self.model = model.into();
        self.model_reason = reason.to_string();
    }

    /// One-line combined reason for the log suffix and the `override_reason`
    /// column. Both halves are reported when both fired.
    fn reason(&self) -> String {
        match (self.key_reason.is_empty(), self.model_reason.is_empty()) {
            (false, false) => format!("{}; {}", self.key_reason, self.model_reason),
            (false, true) => self.key_reason.clone(),
            (true, false) => self.model_reason.clone(),
            (true, true) => String::new(),
        }
    }

    /// ` override=wb-b1@glm-5.3(429 from wb-a1; content_blocked 400)` for log
    /// lines; empty when the request ran on its configured credential/model.
    fn log_suffix(&self) -> String {
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
async fn record_credential_override(
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
async fn refresh_workbuddy_credential(
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
            gui_logs
                .push(
                    "WARN",
                    format!(
                        "刷新 WorkBuddy 访问令牌失败 id={} endpoint={} {} | {}",
                        cred.id, endpoint, tag, msg
                    ),
                )
                .await;
            Err(msg)
        }
    }
}

/// Forward a translated request to the configured upstreams, with failover.
///
/// This is the single upstream send path for the Anthropic, Responses and Chat
/// Completions APIs, in both streaming and non-streaming mode. It walks
/// `config.chat_completions_urls()` in order, retrying the next upstream on a
/// connection error or a retriable status (429/5xx) and failing fast otherwise.
#[allow(clippy::too_many_arguments)]
async fn forward_request(
    config: Arc<Config>,
    client: Client,
    mut openai_req: openai::OpenAIRequest,
    api_key: Option<String>,
    gui_logs: Arc<crate::settings::LogBuffer>,
    client_model: String,
    stats: Arc<StatsDb>,
    flavor: ApiFlavor,
    streaming: bool,
    route: &'static str,
    start: Instant,
    session: SessionInfo,
    tag: String,
    overrides: std::sync::Arc<std::sync::Mutex<OverrideTrace>>,
    pool: crate::session_pool::SharedCredentialPool,
    credential: Option<crate::workbuddy_auth::WorkBuddyCredential>,
) -> ProxyResult<Response> {
    let urls = config.chat_completions_urls();
    let mut last_err = None;

    // The credential chosen for THIS attempt. Starts as the pool's selection
    // (default-first, or the session's sticky replacement); the 4xx handler
    // below may fail over to a different credential and re-enter the loop.
    let mut active_credential = credential;

    // Some providers (WorkBuddy) only accept streaming bodies and answer a
    // non-streaming one with `11101 Non-stream chat request is currently not
    // supported`. For those, request a stream and aggregate it back into the
    // single response the client asked for, so the caller's `stream` flag is
    // honoured at the API boundary regardless.
    //
    // Providers are not all configured alike, so this is also discovered at
    // runtime: a non-streaming request that comes back 11101 is retried once
    // as a stream. That keeps `/v1/messages` without `stream:true` working
    // against a provider we do not know rejects plain bodies.
    let upstream_streaming = streaming || config.force_stream_upstream;
    if upstream_streaming {
        openai_req.stream = Some(true);
        openai_req.stream_options = Some(openai::StreamOptions {
            include_usage: true,
        });
    } else {
        openai_req.stream = Some(false);
        openai_req.stream_options = None;
    }

    // Neutralize content-filter fingerprints across every message field once,
    // before sending. This is the Rust port of the Go sanitize pass and covers
    // the user/assistant/tool/reasoning channels that a system-only scrub misses.
    // It runs for all three API flavors through this shared path (Anthropic,
    // Responses, Chat), so both Claude Code and Codex benefit.
    if config.sanitize_fingerprints {
        pipeline::sanitize_openai_request(&mut openai_req);
    }

    // Stall guard: a content-policy block (WorkBuddy business code 11128) is
    // *not* a transient 5xx, and retrying with the identical fingerprinted body
    // only re-hits the same `unapproved channel` rejection. We make exactly one
    // "degraded" attempt per upstream URL: re-send with a neutral system prompt
    // so the user's real request still gets answered. This mirrors the Go
    // upstream's ErrContentBlocked → Degraded retry and stops Claude Code/Codex
    // from spinning on a bare 400.
    let mut degraded_attempt = false;

    // Credential attempts: the pool's pick first, then at most
    // `min(pool-1, 2)` failovers when a limit/failure exception hits. An empty
    // pool (or a static-key/passthrough configuration) yields exactly one
    // iteration with `active_credential = None` — the 1.7.1 behaviour.
    let pool_size = if pool.is_enabled().await && config.credential_pool_enabled {
        pool.snapshot().await.iter().filter(|c| c.enabled).count()
    } else {
        0
    };
    let max_credential_attempts = if config.session_switch_enabled {
        pool_size.clamp(1, 3)
    } else {
        1
    };

    'credential: for credential_attempt in 0..max_credential_attempts {
        let mut current_credential = active_credential.clone();

        // Proactive renewal: a token already past (or within the margin of) its
        // expiry is renewed before spending a round-trip on it. This is what
        // keeps a long-idle app working on the first request after the login
        // state's access token expired.
        if let Some(cred) = current_credential.clone() {
            if cred.needs_refresh(crate::util::unix_millis()) {
                if let Ok(fresh) =
                    refresh_workbuddy_credential(&client, &cred, &pool, &gui_logs, &tag).await
                {
                    active_credential = Some(fresh.clone());
                    current_credential = Some(fresh);
                }
                // On failure fall through: the request will 401 and the
                // reactive handler below makes one more (bounded) attempt,
                // then fails over.
            }
        }

        // One token refresh per credential attempt: a 401 is retried once on
        // the same credential with a fresh token before any failover.
        let mut token_refresh_attempted = false;

        'url: for url in &urls {
            let mode = if streaming {
                "streaming"
            } else {
                "non-streaming"
            };

            // Up to three attempts against this URL: the original request, plus at
            // most one recovery retry of either kind — a content-policy degraded
            // retry, or an upgrade to a stream when the provider refuses plain
            // bodies (`11101`). A connect error or retriable 5xx instead moves on
            // to the next URL.
            for attempt in 0..=2 {
                tracing::debug!(
                    "Sending {} {}request to {} (model: {})",
                    mode,
                    flavor.label(),
                    url,
                    openai_req.model
                );

                let mut req_builder = client.post(url).json(&openai_req);
                // Credential auth is authoritative when present: the token must
                // not be accompanied by a static-key Authorization/x-api-key.
                req_builder = apply_upstream_auth(
                    req_builder,
                    &config,
                    &api_key,
                    current_credential.as_ref(),
                );
                if !streaming {
                    // Streaming requests get no per-request timeout on purpose: a
                    // total timeout hard-kills any stream that outlives it — exactly
                    // the "Codex occasionally dies mid-run" symptom when a long
                    // generation exceeds the limit. Stalls are instead caught by the
                    // shared client's idle `read_timeout` (see the GUI client
                    // builder), which only fires when the upstream stops sending.
                    req_builder = req_builder.timeout(Duration::from_secs(300));
                }

                let upstream_start = Instant::now();
                let response = match req_builder.send().await {
                    Ok(resp) => {
                        metrics::upstream_latency(
                            upstream_start.elapsed().as_secs_f64(),
                            "chat_completions",
                        );
                        resp
                    }
                    Err(err) => {
                        tracing::warn!("Failed to reach {}: {:?}", url, err);
                        metrics::upstream_error("chat_completions");
                        gui_logs
                            .push(
                                "ERROR",
                                format!(
                                    "UPSTREAM ERROR [connect] url={} model={} {}ms {} | {}",
                                    url,
                                    openai_req.model,
                                    upstream_start.elapsed().as_millis(),
                                    tag,
                                    err
                                ),
                            )
                            .await;
                        last_err = Some(ProxyError::Http(err));
                        continue 'url; // try next upstream URL
                    }
                };

                let status = response.status();
                if !status.is_success() {
                    let body = response
                        .text()
                        .await
                        .unwrap_or_else(|_| "Unknown error".to_string());
                    let elapsed = upstream_start.elapsed().as_millis();
                    metrics::upstream_error("chat_completions");
                    log_upstream_failure(
                        &gui_logs,
                        flavor.stage(),
                        url,
                        &openai_req.model,
                        status.as_u16(),
                        &body,
                        elapsed,
                        &tag,
                    )
                    .await;

                    // Limit/failure exception on an active credential → fail over.
                    // Only the session that hit the exception is relocated (the
                    // plan's default-first contract), the replacement sticks for
                    // that session so its prompt cache survives, and the current
                    // request is retried on the replacement immediately.
                    if let Some(cred) = current_credential.as_ref() {
                        // ── 401 special-case: distinguish token expiry from routing failure ──
                        //
                        // Tencent / WorkBuddy upstreams return HTTP 401 for two very
                        // different root causes that call for opposite treatments:
                        //
                        //  a) {"message":"not_found"} — the gateway could not route to
                        //     the session / pod. The access token is fine; refreshing it
                        //     wastes ~200 ms and won't fix a routing hiccup.  A brief
                        //     wait lets the load-balancer resolve the bad path; if the
                        //     retry still fails we fall through to the account switch.
                        //
                        //  b) Any other 401 — the access token is stale or revoked.
                        //     Renew it and retry the *same* credential once before
                        //     switching; with a single account, failing over has nowhere
                        //     to go and would surface the 401 to the client directly.
                        if status.as_u16() == 401 && body.contains("not_found") && !token_refresh_attempted {
                            if attempt == 0 {
                                // First attempt: brief pause, then retry without refresh.
                                gui_logs
                                    .push(
                                        "WARN",
                                        format!(
                                            "UPSTREAM 401 not_found (疑似瞬时路由故障), 1.5s 后重试同一凭据 model={} {}",
                                            openai_req.model, tag
                                        ),
                                    )
                                    .await;
                                tokio::time::sleep(Duration::from_millis(1500)).await;
                                continue; // attempt 0 → 1, no token refresh
                            }
                            // attempt ≥ 1: two tries already failed; fall through to
                            // is_switchable_error → account switch (or 5 s cooldown).
                        } else if status.as_u16() == 401 && !token_refresh_attempted {
                            // Normal 401: the access token is stale/revoked. Renew it
                            // and retry the *same* credential once before failing
                            // over: with a single account, failing over has nowhere
                            // to go and would just surface the 401 to the client.
                            token_refresh_attempted = true;
                            let updated = cred.clone();
                            if let Ok(fresh) = refresh_workbuddy_credential(
                                &client, &updated, &pool, &gui_logs, &tag,
                            )
                            .await
                            {
                                // A transparent renewal of the *default* account is
                                // not an identity override: only label it when the
                                // credential itself differs from the default.
                                if pool.configured_default_id().await.as_deref()
                                    != Some(fresh.id.as_str())
                                {
                                    if let Ok(mut t) = overrides.lock() {
                                        t.set_key(fresh.id.clone(), "401 刷新令牌后重试");
                                        t.set_model(openai_req.model.clone(), "401 refresh 后重试");
                                    }
                                }
                                active_credential = Some(fresh.clone());
                                current_credential = Some(fresh);
                                continue; // retry this URL with the renewed token
                            }
                        }


                        if is_switchable_error(status.as_u16(), &body) {
                            let from = cred.id.clone();
                            // Attempt the switch BEFORE marking the failing
                            // credential as limited, so we can use the
                            // availability of a healthy replacement to decide
                            // how long the cooldown should be:
                            //
                            //  • Switch succeeds → full 60 s: the error is
                            //    likely account-specific (rate-limited, banned).
                            //  • Switch fails (pool exhausted) → short 5 s: the
                            //    error may be transient/global (e.g. upstream
                            //    hiccup hitting all accounts simultaneously).
                            //    A 60 s cooldown would make pool.is_enabled()
                            //    return false and take every subsequent request
                            //    offline for the full minute.
                            let replacement = if config.session_switch_enabled {
                                pool.switch(&session.session_id, &from).await
                            } else {
                                None
                            };
                            match replacement {
                                Some(next) => {
                                    // Park the failing credential for the full
                                    // cooldown — we have somewhere else to go.
                                    pool.mark_limited(&from, 60_000).await;
                                    gui_logs
                                        .push(
                                            "WARN",
                                            format!(
                                            "session 切换 from={} to={} reason={} {} session={}",
                                            from,
                                            next.id,
                                            status,
                                            tag,
                                            session.session_id
                                        ),
                                        )
                                        .await;
                                    if let Ok(mut t) = overrides.lock() {
                                        t.set_key(next.id.clone(), format!("{status} from {from}"));
                                        t.set_model(openai_req.model.clone(), format!("failover {status} from {from}"));
                                    }
                                    active_credential = Some(next);
                                    continue 'credential; // retry the request on the replacement
                                }
                                None => {
                                    // No healthy replacement available. Apply only
                                    // a short cooldown so the pool stays online
                                    // for the next client retry (transient errors
                                    // resolve; a 60 s full park would silence the
                                    // pool and make every following request fail
                                    // immediately without trying the upstream).
                                    pool.mark_limited(&from, 5_000).await;
                                    gui_logs
                                        .push(
                                            "WARN",
                                            format!(
                                                "凭据池无可用替换凭据 from={} reason={} {} (短暂冷却5s后恢复)",
                                                from, status, tag
                                            ),
                                        )
                                        .await;
                                }
                            }
                        }
                    }

                    // Shape-dependent rejections (e.g. 11148 "tool calls and tool
                    // results do not match") can only be diagnosed from the message
                    // skeleton, so attach it whenever the upstream complains about
                    // tool-call pairing.
                    if body.contains("11148") || body.contains("11152") {
                        gui_logs
                            .push(
                                "ERROR",
                                format!(
                                    "REQUEST SHAPE model={} {} | {}",
                                    openai_req.model,
                                    tag,
                                    describe_message_shape(&openai_req.messages)
                                ),
                            )
                            .await;
                    }

                    // Content-policy block → at most one degraded retry against the
                    // same URL, then surface a clear content_blocked error instead of
                    // a raw 400/502.
                    if is_content_blocked(status.as_u16(), &body)
                        && !degraded_attempt
                        && attempt == 0
                    {
                        degraded_attempt = true;
                        tracing::warn!(
                            "Upstream content block ({}); attempting one degraded retry",
                            status
                        );
                        gui_logs
                            .push(
                                "WARN",
                                format!(
                                "UPSTREAM content_blocked ({}) → 1 degraded retry (model={}) {}",
                                status, openai_req.model, tag
                            ),
                            )
                            .await;
                        apply_degraded_prompt(&mut openai_req);
                        if let Ok(mut t) = overrides.lock() {
                            t.set_model(&openai_req.model, format!("content_blocked {status}"));
                        }
                        continue; // attempt == 1: retry the SAME URL with the neutral prompt
                    }

                    // Safety net: a provider that only serves streaming bodies may
                    // not be known up front (custom URL rather than a known
                    // preset). If it rejects a plain body with `11101`, re-send as
                    // a stream and aggregate it back into the single body the
                    // client asked for; the client's own `stream` flag is
                    // unaffected.
                    if !upstream_streaming && is_non_stream_unsupported(&body) {
                        tracing::warn!(
                            "Upstream rejected non-streaming body ({}); retrying as a stream",
                            status
                        );
                        gui_logs
                            .push(
                                "WARN",
                                format!(
                                    "非流式被上游拒绝(11101) → 改用流式请求重试 (model={}) {}",
                                    openai_req.model, tag
                                ),
                            )
                            .await;
                        if let Ok(mut t) = overrides.lock() {
                            t.set_model(&openai_req.model, "11101 stream upgrade");
                        }
                        return retry_as_stream(
                            &config,
                            &client,
                            &openai_req,
                            &api_key,
                            current_credential.as_ref(),
                            &gui_logs,
                            &client_model,
                            &stats,
                            flavor,
                            route,
                            start,
                            &session,
                            &tag,
                            url,
                            &overrides,
                        )
                        .await;
                    }

                    let err =
                        ProxyError::Upstream(format!("Upstream returned {}: {}", status, body));
                    if is_retriable_status(status.as_u16()) {
                        last_err = Some(err);
                        continue 'url; // try next upstream URL
                    }
                    // After the degraded retry still fails, report a content_blocked
                    // error the client can understand rather than a generic upstream
                    // one.
                    if is_content_blocked(status.as_u16(), &body) {
                        return Err(ProxyError::Upstream(
                        "Upstream rejected the request as content policy violation (code 11128). \
                         The degraded retry also failed."
                            .to_string(),
                    ));
                    }
                    return Err(err);
                }

                return if streaming {
                    streaming_response(
                        response,
                        flavor,
                        client_model,
                        route,
                        start,
                        gui_logs,
                        stats,
                        session,
                        tag,
                        Arc::clone(&overrides),
                    )
                } else {
                    // The client wants one JSON body. If the request was upgraded
                    // to a stream upstream, aggregate it first; otherwise parse the
                    // response directly.
                    let resp = if upstream_streaming {
                        collect_stream_into_response(response).await?
                    } else {
                        response.json::<openai::OpenAIResponse>().await?
                    };
                    non_streaming_response(
                        resp,
                        flavor,
                        &openai_req.model,
                        client_model,
                        route,
                        start,
                        &config,
                        stats,
                        &session,
                        &tag,
                        &overrides,
                    )
                    .await
                };
            }
        }

        // The credential's attempts were exhausted without an exception that
        // warrants a switch (e.g. a plain upstream error): stop, so a 5xx
        // storm does not burn the whole pool.
        if credential_attempt + 1 < max_credential_attempts {
            // A 4xx limit exception already failed over inside the loop (see
            // the 429/402 handling); reaching here means non-switch failures.
            break 'credential;
        }
    }

    Err(last_err.unwrap_or_else(|| ProxyError::Upstream("All upstreams failed".to_string())))
}

/// Accumulates one streaming upstream response into a single
/// `OpenAIResponse`, for clients that asked for a non-streaming reply.
///
/// Needed because some providers (WorkBuddy) only accept `stream:true` and
/// reject a plain body with `11101`. Merging is done on the same chunk shape
/// `create_flavor_sse_stream` consumes, so both paths agree on how content,
/// tool calls and usage are read.
async fn collect_stream_into_response(
    response: reqwest::Response,
) -> ProxyResult<openai::OpenAIResponse> {
    use futures::StreamExt;

    let mut accumulated = Aggregate::default();
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(ProxyError::Http)?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        while let Some(pos) = buffer.find("\n\n") {
            let frame = buffer[..pos].to_string();
            buffer = buffer[pos + 2..].to_string();
            for line in frame.lines() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if data.trim() == "[DONE]" {
                    continue;
                }
                if let Ok(chunk_obj) = serde_json::from_str::<openai::StreamChunk>(data) {
                    accumulated.absorb(&chunk_obj);
                }
            }
        }
    }

    Ok(accumulated.into_response())
}

/// Running merge of stream chunks into one response.
#[derive(Default)]
struct Aggregate {
    id: Option<String>,
    model: Option<String>,
    created: Option<u64>,
    content: String,
    reasoning: String,
    finish_reason: Option<String>,
    /// Tool calls keyed by their stream index, since arguments arrive in pieces.
    tool_calls: BTreeMap<usize, openai::ToolCall>,
    usage: Option<openai::Usage>,
}

impl Aggregate {
    fn absorb(&mut self, chunk: &openai::StreamChunk) {
        if self.id.is_none() {
            self.id = chunk.id.clone();
        }
        if self.model.is_none() {
            self.model = chunk.model.clone();
        }
        if self.created.is_none() {
            self.created = chunk.created;
        }
        if let Some(usage) = &chunk.usage {
            self.usage = Some(usage.clone());
        }

        let Some(choice) = chunk.choices.first() else {
            return;
        };

        if let Some(content) = &choice.delta.content {
            self.content.push_str(content);
        }
        if let Some(reasoning) = choice
            .delta
            .reasoning_content
            .as_ref()
            .or(choice.delta.reasoning.as_ref())
        {
            self.reasoning.push_str(reasoning);
        }
        // An empty-string finish_reason means "not done yet" on some upstreams.
        if let Some(reason) = &choice.finish_reason {
            if !reason.is_empty() {
                self.finish_reason = Some(reason.clone());
            }
        }

        for call in choice.delta.tool_calls.iter().flatten() {
            let entry = self
                .tool_calls
                .entry(call.index)
                .or_insert_with(|| openai::ToolCall {
                    id: String::new(),
                    call_type: "function".to_string(),
                    function: openai::FunctionCall {
                        name: String::new(),
                        arguments: String::new(),
                    },
                });
            if let Some(id) = &call.id {
                if !id.is_empty() {
                    entry.id = id.clone();
                }
            }
            if let Some(t) = &call.call_type {
                entry.call_type = t.clone();
            }
            if let Some(f) = &call.function {
                if let Some(name) = &f.name {
                    entry.function.name.push_str(name);
                }
                if let Some(args) = &f.arguments {
                    entry.function.arguments.push_str(args);
                }
            }
        }
    }

    fn into_response(self) -> openai::OpenAIResponse {
        let mut tool_calls: Vec<openai::ToolCall> = self.tool_calls.into_values().collect();
        // Drop half-open calls: an id is what lets the client answer them.
        tool_calls.retain(|c| !c.id.is_empty());
        let tool_calls = if tool_calls.is_empty() {
            None
        } else {
            Some(tool_calls)
        };

        let content = if self.content.is_empty() {
            None
        } else {
            Some(self.content)
        };

        openai::OpenAIResponse {
            id: self.id,
            object: Some("chat.completion".to_string()),
            created: self.created,
            model: self.model,
            choices: vec![openai::Choice {
                index: 0,
                message: openai::ChoiceMessage {
                    role: "assistant".to_string(),
                    content,
                    tool_calls,
                },
                finish_reason: self.finish_reason.or(Some("stop".to_string())),
            }],
            usage: self.usage.unwrap_or_default(),
            system_fingerprint: None,
        }
    }
}

/// Translate a successful non-streaming upstream response back to the caller's
/// protocol.
#[allow(clippy::too_many_arguments)]
async fn non_streaming_response(
    mut openai_resp: openai::OpenAIResponse,
    flavor: ApiFlavor,
    upstream_model: &str,
    client_model: String,
    route: &'static str,
    start: Instant,
    config: &Config,
    stats: Arc<StatsDb>,
    session: &SessionInfo,
    tag: &str,
    overrides: &Arc<std::sync::Mutex<OverrideTrace>>,
) -> ProxyResult<Response> {
    let metrics_model = match flavor {
        ApiFlavor::Chat => &client_model,
        _ => upstream_model,
    };
    metrics::tokens(
        openai_resp.usage.prompt_tokens,
        openai_resp.usage.completion_tokens,
        metrics_model,
    );

    let tokens = openai_resp.usage.to_token_record();
    let duration_ms = start.elapsed().as_millis() as i64;
    // Snapshot the override trace before the row is built: borrowing through a
    // MutexGuard inside this struct literal self-deadlocks (a second `.lock()`
    // in the same expression waits on the first temporary guard).
    let (override_key, override_model, override_reason) = {
        let t = overrides.lock().unwrap_or_else(|p| p.into_inner());
        (t.key.clone(), t.model.clone(), t.reason())
    };
    // Record token breakdown and request log in the persistent stats DB.
    let _ = stats.record_request_log(RequestOutcome {
        model: &client_model,
        route,
        tokens: &tokens,
        duration_ms,
        streamed: false,
        status: 200,
        error: None,
        session_id: &session.session_id,
        client: session.client.tag(),
        // One lock for both fields: two `.lock()` temporaries in the same
        // expression would leave the first guard alive while the second
        // acquires, self-deadlocking the request thread.
        override_key: &override_key,
        override_model: &override_model,
        override_reason: &override_reason,
    });

    if config.verbose {
        tracing::trace!(
            "Received OpenAI response: {} {}",
            serde_json::to_string_pretty(&openai_resp).unwrap_or_default(),
            tag
        );
    }

    match flavor {
        ApiFlavor::Anthropic => {
            let anthropic_resp = pipeline::translate_response(openai_resp, upstream_model)?;
            if config.verbose {
                tracing::trace!(
                    "Transformed Anthropic response: {}",
                    serde_json::to_string_pretty(&anthropic_resp).unwrap_or_default()
                );
            }
            Ok(Json(anthropic_resp).into_response())
        }
        ApiFlavor::Responses => {
            let responses_resp =
                responses_pipeline::translate_responses_response(openai_resp, &client_model)?;
            if config.verbose {
                tracing::trace!(
                    "Transformed Responses API response: {}",
                    serde_json::to_string_pretty(&responses_resp).unwrap_or_default()
                );
            }
            Ok(Json(responses_resp).into_response())
        }
        ApiFlavor::Chat => {
            // Report the model the client asked for, not the upstream's name.
            if openai_resp.model.is_some() {
                openai_resp.model = Some(client_model);
            }
            Ok(Json(openai_resp).into_response())
        }
    }
}

/// Build the SSE response for a successful streaming upstream response.
#[allow(clippy::too_many_arguments)]
fn streaming_response(
    response: reqwest::Response,
    flavor: ApiFlavor,
    client_model: String,
    route: &'static str,
    start: Instant,
    gui_logs: Arc<crate::settings::LogBuffer>,
    stats: Arc<StatsDb>,
    session: SessionInfo,
    tag: String,
    overrides: Arc<std::sync::Mutex<OverrideTrace>>,
) -> ProxyResult<Response> {
    let upstream = response.bytes_stream();
    let sse_stream = create_flavor_sse_stream(
        upstream,
        flavor,
        client_model,
        route,
        start,
        gui_logs,
        stats,
        session,
        tag,
        overrides,
    );

    let mut headers = HeaderMap::new();
    headers.insert(
        "Content-Type",
        HeaderValue::from_static("text/event-stream"),
    );
    headers.insert("Cache-Control", HeaderValue::from_static("no-cache"));
    headers.insert("Connection", HeaderValue::from_static("keep-alive"));
    if flavor.sse_allows_cors() {
        headers.insert("Access-Control-Allow-Origin", HeaderValue::from_static("*"));
    }

    Ok((headers, Body::from_stream(sse_stream)).into_response())
}

pub async fn list_models_handler(
    Extension(config): Extension<Arc<Config>>,
    Extension(client): Extension<Client>,
    Extension(gui_logs): Extension<Arc<crate::settings::LogBuffer>>,
    headers: HeaderMap,
) -> ProxyResult<Response> {
    let api_key = resolve_api_key(&config, &headers);

    // No body on this route, so header sources are the only ones available.
    let session = resolve_session(&headers, None);
    let tag = session.log_tag();

    let incoming_headers = format_headers(&headers);
    gui_logs
        .push(
            "INFO",
            format!("GET /v1/models {} headers: {}", tag, incoming_headers),
        )
        .await;
    tracing::info!("GET /v1/models {} headers: {}", tag, incoming_headers);

    // Vendors like WorkBuddy publish their catalog on a custom config
    // endpoint instead of the OpenAI `/v1/models` route; honour it.
    if config.models_flavor == ModelsFlavor::WorkBuddyConfig {
        return list_models_via_config(&config, &client, &api_key, &gui_logs, &tag).await;
    }

    let urls = config.models_urls();
    let mut last_err = None;

    for url in &urls {
        tracing::debug!("Fetching models from {}", url);

        let mut req_builder = client.get(url).timeout(Duration::from_secs(60));
        if let Some(ref key) = api_key {
            req_builder = req_builder.header("Authorization", format!("Bearer {}", key));
        }

        match req_builder.send().await {
            Ok(response) if response.status().is_success() => {
                let openai_resp: openai::ModelsListResponse = response.json().await?;
                let anthropic_resp = pipeline::translate_models_list(openai_resp);
                return Ok(Json(anthropic_resp).into_response());
            }
            Ok(response) => {
                let status = response.status();
                let error_text = response
                    .text()
                    .await
                    .unwrap_or_else(|_| "Unknown error".to_string());
                tracing::warn!("Upstream {} returned {}: {}", url, status, error_text);
                if is_retriable_status(status.as_u16()) {
                    last_err = Some(format!("Upstream returned {}: {}", status, error_text));
                    continue;
                }
                return Err(ProxyError::Upstream(format!(
                    "Upstream returned {}: {}",
                    status, error_text
                )));
            }
            Err(err) => {
                tracing::warn!("Failed to reach {}: {:?}", url, err);
                last_err = Some(format!("HTTP error: {}", err));
                continue;
            }
        }
    }

    Err(ProxyError::Upstream(
        last_err.unwrap_or_else(|| "All upstreams failed".to_string()),
    ))
}

/// Serve `/v1/models` from a vendor config endpoint (WorkBuddy `/v3/config`).
///
/// The vendor catalog is filtered to CLI-authorized models and then shaped
/// like an OpenAI list so the existing translation applies unchanged.
async fn list_models_via_config(
    config: &Config,
    client: &Client,
    api_key: &Option<String>,
    gui_logs: &Arc<crate::settings::LogBuffer>,
    tag: &str,
) -> ProxyResult<Response> {
    let Some(url) = config.models_config_url.clone() else {
        return Err(ProxyError::Upstream(
            "provider has no models config endpoint".to_string(),
        ));
    };
    let Some(key) = api_key.clone() else {
        return Err(ProxyError::Upstream(
            "API key required to list models".to_string(),
        ));
    };

    let preset = crate::providers::ProviderPreset {
        id: "vendor".to_string(),
        name: "vendor".to_string(),
        chat_completions_url: String::new(),
        models_url: None,
        models_config_url: Some(url),
        config_headers: Default::default(),
        force_stream: false,
    };

    let models = crate::providers::fetch_models(client, &preset, &key)
        .await
        .map_err(|e| ProxyError::Upstream(e.to_string()))?;

    let openai_resp = openai::ModelsListResponse {
        object: Some("list".to_string()),
        data: models
            .into_iter()
            .map(|m| openai::ModelInfo {
                id: m.id,
                object: Some("model".to_string()),
                created: None,
                owned_by: None,
            })
            .collect(),
    };

    gui_logs
        .push(
            "INFO",
            format!(
                "GET /v1/models (vendor config) -> {} models {}",
                openai_resp.data.len(),
                tag
            ),
        )
        .await;

    Ok(Json(pipeline::translate_models_list(openai_resp)).into_response())
}

/// Resolve the API key for the Anthropic Messages API (`x-api-key` header).
fn resolve_api_key(config: &Config, headers: &HeaderMap) -> Option<String> {
    if config.passthrough_api_key {
        header_value(headers, "x-api-key")
    } else {
        config.api_key.clone()
    }
}

/// Resolve the API key for the Responses API (bearer token or `x-api-key`).
fn resolve_responses_api_key(config: &Config, headers: &HeaderMap) -> Option<String> {
    if config.passthrough_api_key {
        bearer_or_api_key(headers)
    } else {
        config.api_key.clone()
    }
}

/// Resolve the API key for Chat Completions, where the client-supplied key may
/// be used even when a static key is configured (the static key wins then).
fn resolve_chat_api_key(config: &Config, headers: &HeaderMap) -> Option<String> {
    let header_key = bearer_or_api_key(headers);
    if config.passthrough_api_key {
        header_key.or_else(|| config.api_key.clone())
    } else {
        config.api_key.clone().or(header_key)
    }
}

/// A single non-empty header value.
fn header_value(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Extract a bearer token from `authorization`, falling back to `x-api-key`.
fn bearer_or_api_key(headers: &HeaderMap) -> Option<String> {
    headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.strip_prefix("Bearer ").unwrap_or(s))
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .or_else(|| header_value(headers, "x-api-key"))
}

fn translation_policy(config: &Config) -> pipeline::TranslationPolicy {
    pipeline::TranslationPolicy {
        reasoning_model: config.reasoning_model.clone(),
        completion_model: config.completion_model.clone(),
        model_map: config.model_map.clone(),
        ignore_terms: config.system_prompt_ignore_terms.clone(),
        strip_model_suffix: config.models_flavor == ModelsFlavor::WorkBuddyConfig,
        sanitize_fingerprints: config.sanitize_fingerprints,
    }
}

/// Attach the upstream authentication + CLI fingerprint headers.
///
/// For the WorkBuddy/CodeBuddy flavor we mirror the official client's request
/// fingerprint: the `x-api-key`/`Authorization` pair plus the official
/// `User-Agent`, and a small set of low-risk correlation headers the gateway
/// expects from the genuine CLI (`X-CodeBuddy-Request`, `Accept`,
/// `X-Requested-With`). These are cheap and were validated as passing in the
/// troubleshooting doc's "full headers" test, so we send them proactively.
/// Other providers keep the plain `Authorization` flow.
///
/// `credential` — when the request is served by a login-state credential — is
/// authoritative: its bearer token replaces *both* the `Authorization` and
/// `x-api-key` values, and no static-key value survives. This must be a single
/// header map because reqwest's `.header()` *appends* rather than replaces, so
/// issuing a static-key `Authorization` and then a credential `Authorization`
/// would put two conflicting values on the wire and the Tencent `stgw` gateway
/// answers that with a bare HTML `400 Bad Request`.
fn apply_upstream_auth(
    mut req: reqwest::RequestBuilder,
    config: &Config,
    api_key: &Option<String>,
    credential: Option<&crate::workbuddy_auth::WorkBuddyCredential>,
) -> reqwest::RequestBuilder {
    let headers = upstream_auth_headers(config, api_key, credential);
    if !headers.is_empty() {
        req = req.headers(headers);
    }
    req
}

/// Build the outbound auth/fingerprint header map (pure; unit-tested).
///
/// Every value is inserted with replace semantics. This matters because the
/// caller applies the map to a `reqwest::RequestBuilder` whose `.header()` API
/// *appends*: building the map here and applying it once means a credential and
/// a static key can never both contribute an `Authorization`/`x-api-key` value,
/// which is what produced the duplicate-header `400 Bad Request` from `stgw`.
fn upstream_auth_headers(
    config: &Config,
    api_key: &Option<String>,
    credential: Option<&crate::workbuddy_auth::WorkBuddyCredential>,
) -> HeaderMap {
    let workbuddy = config.models_flavor == ModelsFlavor::WorkBuddyConfig;
    let mut headers = HeaderMap::new();

    if workbuddy {
        // Official-client fingerprint; cheap and validated as passing.
        let mut put = |name: &'static str, value: &str| {
            headers.insert(
                HeaderName::from_static(name),
                HeaderValue::from_str(value).unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        };
        put("user-agent", crate::providers::WORKBUDDY_USER_AGENT);
        put("x-codebuddy-request", "1");
        put("accept", "application/json, text/event-stream");
        put("x-requested-with", "XMLHttpRequest");
    }

    match credential {
        // Login-state credential: one consistent identity. The token is the
        // bearer *and* the api key (mirroring the proven `fetch_points` call),
        // and the session-bound fingerprint headers are added alongside. No
        // static-key value is ever inserted, so none can survive.
        Some(cred) => {
            let token = cred.bearer();
            headers.insert(
                HeaderName::from_static("authorization"),
                HeaderValue::from_str(&format!("Bearer {}", token))
                    .unwrap_or_else(|_| HeaderValue::from_static("")),
            );
            for (name, value) in crate::workbuddy_auth::upstream_headers(cred) {
                if let (Ok(header_name), Ok(header_value)) = (
                    HeaderName::from_bytes(name.as_bytes()),
                    HeaderValue::from_str(&value),
                ) {
                    headers.insert(header_name, header_value);
                }
            }
            // Set x-api-key last so it always matches the token.
            headers.insert(
                HeaderName::from_static("x-api-key"),
                HeaderValue::from_str(token).unwrap_or_else(|_| HeaderValue::from_static("")),
            );
        }
        // Static key path (unchanged behaviour): Authorization always, plus the
        // WorkBuddy `x-api-key` mirror.
        None => {
            if let Some(key) = api_key {
                if workbuddy {
                    headers.insert(
                        HeaderName::from_static("x-api-key"),
                        HeaderValue::from_str(key).unwrap_or_else(|_| HeaderValue::from_static("")),
                    );
                }
                headers.insert(
                    HeaderName::from_static("authorization"),
                    HeaderValue::from_str(&format!("Bearer {}", key))
                        .unwrap_or_else(|_| HeaderValue::from_static("")),
                );
            }
        }
    }

    headers
}

/// Whether an upstream failure should relocate the session onto another
/// credential: the plan's limit/failure classes — 429 rate limit, 402 out of
/// credit, 403 banned/permission denied, and WorkBuddy quota business codes
/// (11105/11106 family). The 11128 content-policy code is deliberately absent:
/// switching accounts cannot fix a content-policy rejection.
fn is_switchable_error(status: u16, body: &str) -> bool {
    // 401: stale/revoked token (handled first with a refresh, then failover)
    // 402: payment required / quota exceeded
    // 403: account banned / permission denied
    // 429: rate limited
    // WorkBuddy quota business codes always switch regardless of HTTP status.
    matches!(status, 401 | 402 | 403 | 429) || is_quota_business_code(body)
}

/// WorkBuddy business codes that signal quota/rate exhaustion for this account.
fn is_quota_business_code(body: &str) -> bool {
    // 11105: quota exhausted; 11106: rate/frequency limited. Kept alongside
    // the status classes so a 200-wrapped business error still switches.
    body.contains("11105") || body.contains("11106")
}

fn is_retriable_status(status: u16) -> bool {
    matches!(status, 429 | 500..=599)
}

/// Neutral, content-free system prompt used for the single degraded retry after
/// an upstream content-policy block. Deliberately minimal so it introduces no
/// fingerprint of its own. Mirrors `prompt.Degraded` in the Go upstream.
const DEGRADED_SYSTEM_PROMPT: &str =
    "You are a helpful assistant. Respond in the user's language, follow the user's instructions, and be direct and concise.";

/// Detect an upstream content-policy block. The WorkBuddy/CodeBuddy gateway
/// reports it as HTTP 400 with business code `11128` ("Illegal API invocation
/// from an unapproved channel"). We treat that specific shape as a content block
/// rather than a generic upstream failure, so the caller can attempt the
/// degraded retry instead of spinning on a bare 4xx.
fn is_content_blocked(status: u16, body: &str) -> bool {
    if status != 400 {
        return false;
    }
    body.contains("11128") || body.contains("unapproved channel")
}

/// Re-send a request as a stream and aggregate it into one non-streaming
/// response, for upstreams that reject plain bodies (`11101`).
///
/// Used as a fallback when the provider was not known to require streaming up
/// front. The client still receives a single JSON body, since it never asked
/// for a stream.
#[allow(clippy::too_many_arguments)]
async fn retry_as_stream(
    config: &Config,
    client: &Client,
    openai_req: &openai::OpenAIRequest,
    api_key: &Option<String>,
    credential: Option<&crate::workbuddy_auth::WorkBuddyCredential>,
    gui_logs: &Arc<crate::settings::LogBuffer>,
    client_model: &str,
    stats: &Arc<StatsDb>,
    flavor: ApiFlavor,
    route: &'static str,
    start: Instant,
    session: &SessionInfo,
    tag: &str,
    url: &str,
    overrides: &Arc<std::sync::Mutex<OverrideTrace>>,
) -> ProxyResult<Response> {
    let mut streamed_req = openai_req.clone();
    streamed_req.stream = Some(true);
    streamed_req.stream_options = Some(openai::StreamOptions {
        include_usage: true,
    });

    let builder = client.post(url).json(&streamed_req);
    // Always a streamed response here, so no total timeout: it would kill any
    // stream outliving it. The shared client's idle read_timeout covers stalls
    // — see forward_request for the rationale.
    let response = apply_upstream_auth(builder, config, api_key, credential)
        .send()
        .await
        .map_err(ProxyError::Http)?;

    let status = response.status();
    if !status.is_success() {
        let body = response
            .text()
            .await
            .unwrap_or_else(|_| "Unknown error".to_string());
        return Err(ProxyError::Upstream(format!(
            "Upstream returned {} (also as a stream): {}",
            status, body
        )));
    }

    let resp = collect_stream_into_response(response).await?;
    gui_logs
        .push(
            "INFO",
            format!(
                "流式重试成功，已聚合为单次响应 (model={}) {}",
                client_model, tag
            ),
        )
        .await;
    non_streaming_response(
        resp,
        flavor,
        &streamed_req.model,
        client_model.to_string(),
        route,
        start,
        config,
        stats.clone(),
        session,
        tag,
        overrides,
    )
    .await
}

/// Detect "this upstream only serves streaming bodies".
///
/// Reported as HTTP 400 with business code `11101` and the message
/// `Non-stream chat request is currently not supported`. Rather than surfacing
/// a 502 to the client, the caller re-sends the same request with
/// `stream:true` and aggregates it back into one body.
fn is_non_stream_unsupported(body: &str) -> bool {
    body.contains("11101") && body.contains("Non-stream")
}

/// Replace the leading `system` message(s) with the neutral degraded prompt.
/// Other (user/assistant/tool) messages are preserved so the user's actual
/// request still gets answered — we are only washing out the blocked system
/// template, which is exactly what the content filter object to.
fn apply_degraded_prompt(req: &mut openai::OpenAIRequest) {
    let mut replaced = false;
    for msg in req.messages.iter_mut() {
        if msg.role == "system" && !replaced {
            msg.content = Some(openai::MessageContent::Text(
                DEGRADED_SYSTEM_PROMPT.to_string(),
            ));
            msg.reasoning_content = None;
            msg.tool_calls = None;
            replaced = true;
        }
    }
    // If there was no system message at all, prepend one.
    if !replaced {
        req.messages.insert(
            0,
            openai::Message {
                role: "system".to_string(),
                content: Some(openai::MessageContent::Text(
                    DEGRADED_SYSTEM_PROMPT.to_string(),
                )),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
        );
    }
}

/// Parse a vendor error envelope into a compact, human-readable summary.
///
/// WorkBuddy/CodeBuddy report failures as
/// `{"code":11128,"msg":"...","requestId":"...","displayMsg":{"zh":"...","en":"..."}}`.
/// We surface the numeric `code` plus the message so the GUI log is actionable
/// instead of an opaque "502".
fn describe_upstream_error(status: u16, body: &str) -> String {
    let trimmed = body.trim();
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if let Some(obj) = value.as_object() {
            // WorkBuddy style: {"code":..., "msg":...}
            if let Some(code) = obj.get("code") {
                let code_str = match code {
                    serde_json::Value::Number(n) => n.to_string(),
                    serde_json::Value::String(s) => s.clone(),
                    _ => code.to_string(),
                };
                let msg = obj
                    .get("msg")
                    .or_else(|| obj.get("message"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let request_id = obj
                    .get("requestId")
                    .or_else(|| obj.get("request_id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let display = obj
                    .get("displayMsg")
                    .and_then(|d| d.get("zh").or_else(|| d.get("en")))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let mut text = format!("HTTP {} · code {} {}", status, code_str, msg);
                if !display.is_empty() && display != msg {
                    text.push_str(&format!(" · {}", display));
                }
                if !request_id.is_empty() {
                    text.push_str(&format!(" · requestId {}", request_id));
                }
                return text;
            }
            // Anthropic-style: {"error":{"message":...}}
            if let Some(err) = obj.get("error").and_then(|e| e.as_object()) {
                let m = err.get("message").and_then(|v| v.as_str()).unwrap_or("");
                if !m.is_empty() {
                    return format!("HTTP {} · {}", status, truncate(m, 400));
                }
            }
        }
    }
    format!("HTTP {} · {}", status, truncate(trimmed, 400))
}

/// A friendlier one-line hint for known WorkBuddy business codes.
fn upstream_code_hint(body: &str) -> Option<&'static str> {
    let value: serde_json::Value = serde_json::from_str(body.trim()).ok()?;
    let code = value.get("code")?.as_i64()?;
    Some(match code {
        11101 => "上游不支持非流式请求或参数解析失败，可强制 stream=true",
        11102 => "模型名不在上游可用列表（注意 [1M] 等后缀；在控制台“模型重定向”里加 请求模型:可用模型 映射，保存即生效）",
        11103 => "后端不支持该模型（如图像模型）",
        11128 => "非法 API 调用：请求形态被上游拒绝",
        11129 => "function 参数非法",
        11133 => "请求被模型提供方拒绝（参数/权限）",
        11135 => "图片数据无效",
        11148 => "tool_calls 与 tool_result 不匹配",
        11151 => "存在空内容消息",
        11152 => "工具名称不符合规范或存在重复名称（只允许字母、数字、下划线，最多64字符且不可重名）",
        _ => return None,
    })
}

/// Push a structured upstream failure into the GUI log buffer (and tracing).
#[allow(clippy::too_many_arguments)]
async fn log_upstream_failure(
    gui_logs: &Arc<crate::settings::LogBuffer>,
    stage: &str,
    url: &str,
    model: &str,
    status: u16,
    body: &str,
    elapsed_ms: u128,
    tag: &str,
) {
    let summary = describe_upstream_error(status, body);
    let mut msg = format!(
        "UPSTREAM ERROR [{}] url={} model={} {}ms {} | {}",
        stage, url, model, elapsed_ms, tag, summary
    );
    if let Some(hint) = upstream_code_hint(body) {
        msg.push_str(&format!(" | hint: {}", hint));
    }
    msg.push_str(&format!(" | body: {}", truncate(body, 800)));
    gui_logs.push("ERROR", msg.clone()).await;
    tracing::warn!("Upstream failure: {}", msg);
}

/// Compact skeleton of the outbound message list, for diagnosing upstream
/// rejections that depend on message *shape* rather than content.
///
/// A 11148 ("tool calls and tool results do not match") is unreadable without
/// knowing which calls had results and in what order, so log the roles, the
/// tool-call ids and the result ids — never the message bodies (too large, and
/// they may hold user content).
fn describe_message_shape(messages: &[crate::models::openai::Message]) -> String {
    let parts: Vec<String> = messages
        .iter()
        .map(|m| {
            let calls: Vec<String> = m
                .tool_calls
                .as_ref()
                .map(|v| {
                    v.iter()
                        .map(|t| format!("{}:{}", t.id, t.function.name))
                        .collect()
                })
                .unwrap_or_default();
            match m.role.as_str() {
                "tool" => format!("tool({})", m.tool_call_id.as_deref().unwrap_or("<no-id>")),
                "assistant" if !calls.is_empty() => format!("assistant[{}]", calls.join(",")),
                other => other.to_string(),
            }
        })
        .collect();
    format!("{} msgs: {}", messages.len(), parts.join(" > "))
}

/// Serialize one SSE event in the `event:`/`data:` framing both APIs use.
fn serialize_sse_event<T: serde::Serialize>(event_type: &str, event: &T) -> String {
    format!(
        "event: {}\ndata: {}\n\n",
        event_type,
        serde_json::to_string(event).unwrap_or_default()
    )
}

/// Owns the one request-log row for a streamed request, and writes it when the
/// stream generator is dropped.
///
/// Recording on drop rather than after the final `yield` is what makes the row
/// independent of how much of the response the client chose to read: a `yield`
/// suspends the generator, so a client that disconnects (or simply stops
/// reading) at that point drops the generator before any later statement runs.
/// Drop always runs, so the request still reaches the log — with whatever
/// tokens and error state had been observed by then, and a duration measured
/// from the start of the request.
struct StreamLedger {
    model: String,
    route: &'static str,
    start: Instant,
    stats: Arc<StatsDb>,
    /// Session identity for the row this ledger writes on drop.
    session: SessionInfo,
    tokens: TokenRecord,
    /// Set when the upstream stream failed; turns the row's status into a 500.
    error: Option<String>,
    /// Exception-override fields for the row (see [`OverrideTrace`]).
    override_key: String,
    override_model: String,
    override_reason: String,
}

impl StreamLedger {
    fn new(
        model: String,
        route: &'static str,
        start: Instant,
        stats: Arc<StatsDb>,
        session: SessionInfo,
        overrides: &Arc<std::sync::Mutex<OverrideTrace>>,
    ) -> Self {
        // Snapshot the trace once: a stream's row must reflect the overrides in
        // force when the stream started, and `reason()` needs both halves.
        let (override_key, override_model, override_reason) = {
            let t = overrides.lock().unwrap_or_else(|p| p.into_inner());
            (t.key.clone(), t.model.clone(), t.reason())
        };
        Self {
            model,
            route,
            start,
            stats,
            session,
            tokens: TokenRecord::default(),
            error: None,
            override_key,
            override_model,
            override_reason,
        }
    }
}

impl Drop for StreamLedger {
    fn drop(&mut self) {
        let status = if self.error.is_some() { 500 } else { 200 };
        let _ = self.stats.record_request_log(RequestOutcome {
            model: &self.model,
            route: self.route,
            tokens: &self.tokens,
            duration_ms: self.start.elapsed().as_millis() as i64,
            streamed: true,
            status,
            error: self.error.as_deref(),
            session_id: &self.session.session_id,
            client: self.session.client.tag(),
            override_key: &self.override_key,
            override_model: &self.override_model,
            override_reason: &self.override_reason,
        });
    }
}

/// Shared SSE framer for all three API flavors.
///
/// All three read the upstream's OpenAI-style `data: {...}` stream and differ
/// only in how each chunk is translated and re-serialized, so the buffering,
/// `[DONE]` handling, business-error detection and stats capture live here once.
#[allow(clippy::too_many_arguments)]
fn create_flavor_sse_stream(
    upstream: impl Stream<Item = Result<Bytes, impl std::fmt::Display + Send + 'static>>
        + Send
        + 'static,
    flavor: ApiFlavor,
    client_model: String,
    route: &'static str,
    start: Instant,
    gui_logs: Arc<crate::settings::LogBuffer>,
    stats: Arc<StatsDb>,
    session: SessionInfo,
    tag: String,
    overrides: Arc<std::sync::Mutex<OverrideTrace>>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    async_stream::stream! {
        // Records the request row when the stream ends — including when the
        // client drops the body early.
        //
        // A client is free to stop reading the moment it sees what it needs: a
        // Responses client stops at `response.completed` and never reads the
        // trailing `data: [DONE]`, which only exists for SDKs that read until
        // the stream closes. Since every `yield` suspends the generator, a
        // client that stops mid-stream drops it *at* that yield, and no code
        // after the last yield would ever run. Holding the recording in a guard
        // tied to this generator's lifetime makes the row independent of how
        // far the client read.
        // Read before `session` is moved into the ledger below; drives the
        // Codex stall diagnostic at the end of the stream.
        let client_kind = session.client;
        let mut ledger = StreamLedger::new(
            client_model.clone(),
            route,
            start,
            stats.clone(),
            session,
            &overrides,
        );
        let mut buffer = String::new();
        let mut usage_captured = false;

        // Only the Anthropic and Responses APIs need translation state.
        let mut anthropic_state = match flavor {
            ApiFlavor::Anthropic => Some(stream::initial_state(client_model.clone())),
            _ => None,
        };
        let mut responses_state = match flavor {
            ApiFlavor::Responses => Some(responses_pipeline::initial_stream_state(client_model.clone())),
            _ => None,
        };

        // Record usage exactly once per stream, when the first usage chunk
        // arrives. The ledger holds it until the request is recorded.
        macro_rules! capture_usage {
            ($usage:expr) => {
                if !usage_captured {
                    let usage = $usage;
                    if flavor == ApiFlavor::Chat {
                        metrics::tokens(usage.prompt_tokens, usage.completion_tokens, &client_model);
                    }
                    ledger.tokens = usage.to_token_record();
                    usage_captured = true;
                }
            };
        }

        tokio::pin!(upstream);

        while let Some(chunk) = upstream.next().await {
            match chunk {
                Ok(bytes) => {
                    buffer.push_str(&String::from_utf8_lossy(&bytes));

                    while let Some(pos) = buffer.find("\n\n") {
                        let line = buffer[..pos].to_string();
                        buffer = buffer[pos + 2..].to_string();

                        if line.trim().is_empty() {
                            continue;
                        }

                        for l in line.lines() {
                            let Some(data) = l.strip_prefix("data: ") else {
                                // Chat Completions is a passthrough: forward any
                                // non-`data:` line verbatim.
                                if flavor == ApiFlavor::Chat && !l.trim().is_empty() {
                                    yield Ok(Bytes::from(format!("{}\n", l)));
                                }
                                continue;
                            };

                            if data.trim() == "[DONE]" {
                                match flavor {
                                    ApiFlavor::Anthropic => {
                                        if let Some(state) = anthropic_state.as_mut() {
                                            for event in stream::translate_done(state) {
                                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                            }
                                        }
                                    }
                                    ApiFlavor::Responses => {
                                        if let Some(state) = responses_state.as_mut() {
                                            for event in responses_pipeline::translate_stream_done(state) {
                                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                            }
                                        }
                                        yield Ok(Bytes::from("data: [DONE]\n\n"));
                                    }
                                    ApiFlavor::Chat => {
                                        yield Ok(Bytes::from("data: [DONE]\n\n"));
                                    }
                                }
                                continue;
                            }

                            match serde_json::from_str::<openai::StreamChunk>(data) {
                                Ok(mut chunk_obj) => {
                                    if let Some(ref usage) = chunk_obj.usage {
                                        capture_usage!(usage.clone());
                                    }

                                    match flavor {
                                        ApiFlavor::Anthropic => {
                                            if let Some(state) = anthropic_state.as_mut() {
                                                for event in stream::translate_chunk(state, &chunk_obj) {
                                                    yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                                }
                                            }
                                        }
                                        ApiFlavor::Responses => {
                                            if let Some(state) = responses_state.as_mut() {
                                                for event in responses_pipeline::translate_stream_chunk(state, &chunk_obj) {
                                                    yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                                }
                                            }
                                        }
                                        ApiFlavor::Chat => {
                                            if chunk_obj.model.is_some() {
                                                chunk_obj.model = Some(client_model.clone());
                                            }
                                            let serialized = serde_json::to_string(&chunk_obj)
                                                .unwrap_or_else(|_| data.to_string());
                                            yield Ok(Bytes::from(format!("data: {}\n\n", serialized)));
                                        }
                                    }
                                }
                                Err(e) => {
                                    // Not a StreamChunk. Two cases matter.
                                    if let Ok(val) = serde_json::from_str::<serde_json::Value>(data) {
                                        // Some upstreams emit a business error as a
                                        // `data:` line inside a 200 stream. Surface it
                                        // instead of silently dropping it.
                                        if val.get("code").is_some() {
                                            let summary = describe_upstream_error(200, data);
                                            let model = anthropic_state
                                                .as_ref()
                                                .map(|s| s.model().to_string())
                                                .or_else(|| responses_state.as_ref().map(|s| s.model().to_string()))
                                                .unwrap_or_else(|| client_model.clone());
                                            let msg = format!(
                                                "UPSTREAM ERROR [stream] model={} {} | {}",
                                                model, tag, summary
                                            );
                                            gui_logs.push("ERROR", msg.clone()).await;
                                            tracing::warn!("{}", msg);

                                            match flavor {
                                                ApiFlavor::Anthropic => {
                                                    for event in stream::translate_error(format!("Upstream error: {}", summary)) {
                                                        yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                                    }
                                                    break;
                                                }
                                                ApiFlavor::Responses => {
                                                    if let Some(state) = responses_state.as_mut() {
                                                        for event in responses_pipeline::translate_stream_error(
                                                            state,
                                                            format!("Upstream error: {}", summary),
                                                        ) {
                                                            yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                                        }
                                                    }
                                                    break;
                                                }
                                                ApiFlavor::Chat => {
                                                    yield Ok(Bytes::from(format!("data: {}\n\n", data)));
                                                }
                                            }
                                        } else {
                                            tracing::debug!("Ignoring unrecognized upstream stream chunk: {}", data);
                                        }
                                    } else if data.contains("\"usage\"") {
                                        // A usage frame must never vanish without a
                                        // trace: dropping it here silently recorded
                                        // the request with zero tokens.
                                        gui_logs.push("WARN", format!(
                                            "忽略无法解析的 usage 数据帧 ({}): {}",
                                            e,
                                            crate::util::truncate(data, 200)
                                        )).await;
                                    } else {
                                        tracing::debug!("Ignoring unrecognized upstream stream chunk: {}", data);
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    ledger.error = Some(format!("{}", e));
                    match flavor {
                        ApiFlavor::Anthropic => {
                            tracing::error!("Stream error: {}", e);
                            for event in stream::translate_error(format!("Stream error: {}", e)) {
                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                            }
                        }
                        ApiFlavor::Responses => {
                            tracing::error!("Stream error: {}", e);
                            if let Some(state) = responses_state.as_mut() {
                                for event in responses_pipeline::translate_stream_error(
                                    state,
                                    format!("Stream error: {}", e),
                                ) {
                                    yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                }
                            }
                        }
                        ApiFlavor::Chat => {
                            let msg =
                                format!("STREAM READ ERROR model={} {} | {}", client_model, tag, e);
                            gui_logs.push("ERROR", msg.clone()).await;
                            tracing::warn!("{}", msg);
                        }
                    }
                    break;
                }
            }
        }

        // The Responses API stream must always terminate with a completed
        // response and `[DONE]`, even when the upstream omitted the terminator.
        // The request row is already recorded: the ledger below fires when this
        // generator is dropped, whether that happens here or mid-stream.
        if flavor == ApiFlavor::Responses {
            if let Some(state) = responses_state.as_mut() {
                for event in responses_pipeline::translate_stream_done(state) {
                    yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                }
                // Flag the stall symptom: Codex reads the Responses stream for its next
                // *action*. If a turn closes with natural-language text but no tool call,
                // there is nothing for the client to execute and it silently freezes — the
                // "said it would do X, then stopped" report. This WARN makes that self-evident
                // in the logs immediately before the user sees the stall.
                let s = state.summary();
                if client_kind == ClientKind::Codex && s.started_tool_calls == 0 && s.text_len > 0 {
                    let msg = format!(
                        "RESPONSES text-only turn (no tool call) — Codex may stall waiting for an action | text_len={} active_tool_calls={} {}",
                        s.text_len, s.active_tool_calls, tag
                    );
                    gui_logs.push("WARN", msg.clone()).await;
                    tracing::warn!("{}", msg);
                }
            }
            yield Ok(Bytes::from("data: [DONE]\n\n"));
        }
    }
}

/// Test-only aliases preserving the historical per-flavor entry points.
#[cfg(test)]
fn create_sse_stream(
    upstream: impl Stream<Item = Result<Bytes, impl std::fmt::Display + Send + 'static>>
        + Send
        + 'static,
    fallback_model: String,
    gui_logs: Arc<crate::settings::LogBuffer>,
    stats: Arc<StatsDb>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    create_flavor_sse_stream(
        upstream,
        ApiFlavor::Anthropic,
        fallback_model,
        "/v1/messages",
        Instant::now(),
        gui_logs,
        stats,
        SessionInfo::unknown(),
        "client=unknown".to_string(),
        Default::default(),
    )
}

#[cfg(test)]
fn create_responses_sse_stream(
    upstream: impl Stream<Item = Result<Bytes, impl std::fmt::Display + Send + 'static>>
        + Send
        + 'static,
    fallback_model: String,
    gui_logs: Arc<crate::settings::LogBuffer>,
    stats: Arc<StatsDb>,
) -> impl Stream<Item = Result<Bytes, std::io::Error>> + Send {
    create_flavor_sse_stream(
        upstream,
        ApiFlavor::Responses,
        fallback_model,
        "/v1/responses",
        Instant::now(),
        gui_logs,
        stats,
        SessionInfo::unknown(),
        "client=unknown".to_string(),
        Default::default(),
    )
}

/// Build a raw request whose body is `value`, so a handler test exercises the
/// same "peek the body, then hand it to `Json`" path as production.
#[cfg(test)]
fn json_request<T: serde::Serialize>(value: &T) -> Request {
    Request::builder()
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(value).expect("test body serializes"),
        ))
        .expect("test request builds")
}

#[cfg(test)]
mod tests {
    use super::create_sse_stream;
    use super::json_request;
    use super::{
        apply_degraded_prompt, is_content_blocked, is_non_stream_unsupported,
        upstream_auth_headers, OverrideTrace,
    };
    use crate::models::{openai, responses};
    use crate::session::SessionInfo;
    use axum::response::IntoResponse;
    use bytes::Bytes;
    use futures::stream::{self, StreamExt};
    use serde_json::{json, Value};
    use std::fmt;

    /// A credential override and a model override must both survive: they used
    /// to share one `reason` field, so the second `set_*` call erased the first
    /// reason and the log showed only half of a two-part override.
    #[test]
    fn override_trace_keeps_both_reasons() {
        let mut t = OverrideTrace::default();
        t.set_key("wb-b1", "429 from wb-a1");
        t.set_model("glm-5.3", "content_blocked 400");

        assert_eq!(t.key, "wb-b1");
        assert_eq!(t.model, "glm-5.3");
        let reason = t.reason();
        assert!(
            reason.contains("429 from wb-a1") && reason.contains("content_blocked 400"),
            "both reasons must be reported, got: {reason}"
        );
        let suffix = t.log_suffix();
        assert!(suffix.contains("wb-b1@glm-5.3"), "got: {suffix}");
        assert!(suffix.contains("429 from wb-a1"), "got: {suffix}");
    }

    /// A clean request leaves both the columns and the log suffix empty.
    #[test]
    fn override_trace_is_empty_when_nothing_was_overridden() {
        let t = OverrideTrace::default();
        assert!(t.log_suffix().is_empty());
        assert!(t.reason().is_empty());
    }

    #[test]
    fn content_blocked_detects_11128() {
        assert!(is_content_blocked(
            400,
            "{\"code\":11128,\"msg\":\"Illegal API invocation from an unapproved channel\"}"
        ));
        // Non-400 is never a content block.
        assert!(!is_content_blocked(502, "11128"));
        // 400 without the block signature is a generic upstream failure.
        assert!(!is_content_blocked(400, "{\"error\":\"bad request\"}"));
    }

    /// A config in the WorkBuddy flavor, which is the one that mirrors the
    /// `x-api-key`/`Authorization` pair.
    fn workbuddy_config() -> crate::config::Config {
        crate::config::Config {
            models_flavor: crate::config::ModelsFlavor::WorkBuddyConfig,
            ..Default::default()
        }
    }

    /// A login-state credential with a token distinct from any static key.
    fn test_credential() -> crate::workbuddy_auth::WorkBuddyCredential {
        crate::workbuddy_auth::WorkBuddyCredential {
            id: "wb-abc123".to_string(),
            access_token: "credential-token".to_string(),
            machine_id: "machine-1".to_string(),
            account: crate::workbuddy_auth::WorkBuddyAccount {
                uid: "uid-1".to_string(),
                enterprise_id: "ent-1".to_string(),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    /// A static key and a credential must never both authenticate one request.
    /// reqwest's `.header()` appends, so a naive implementation emits two
    /// `Authorization` values and Tencent's `stgw` gateway answers a bare HTML
    /// 400. Regression for the "都是 400 / Bad Request" report.
    #[test]
    fn credential_auth_replaces_the_static_key_on_every_shared_header() {
        let config = workbuddy_config();
        let key = Some("static-key".to_string());
        let cred = test_credential();

        let headers = upstream_auth_headers(&config, &key, Some(&cred));

        // Exactly one value per auth header, and it is the credential's.
        assert_eq!(
            headers.get_all("authorization").iter().count(),
            1,
            "must be a single Authorization header"
        );
        assert_eq!(
            headers.get("authorization").unwrap(),
            "Bearer credential-token"
        );
        assert_eq!(headers.get_all("x-api-key").iter().count(), 1);
        assert_eq!(headers.get("x-api-key").unwrap(), "credential-token");
        // No trace of the static key anywhere.
        assert!(
            !headers
                .values()
                .any(|v| v.to_str().unwrap_or_default().contains("static-key")),
            "the static key must not survive alongside a credential"
        );
        // The session fingerprint is still present.
        assert_eq!(headers.get("x-user-id").unwrap(), "uid-1");
        assert_eq!(headers.get("x-enterprise-id").unwrap(), "ent-1");
        assert_eq!(headers.get("x-machine-id").unwrap(), "machine-1");
    }

    /// Without a credential the static key keeps its 1.7.x behaviour.
    #[test]
    fn static_key_path_still_sends_authorization_and_x_api_key() {
        let config = workbuddy_config();
        let key = Some("static-key".to_string());

        let headers = upstream_auth_headers(&config, &key, None);

        assert_eq!(headers.get("authorization").unwrap(), "Bearer static-key");
        assert_eq!(headers.get("x-api-key").unwrap(), "static-key");
        assert_eq!(headers.get_all("authorization").iter().count(), 1);
        assert_eq!(headers.get_all("x-api-key").iter().count(), 1);
    }

    /// A non-WorkBuddy provider keeps a single `Authorization` and never gains
    /// the WorkBuddy `x-api-key`.
    #[test]
    fn openai_flavor_uses_authorization_only() {
        let config = crate::config::Config {
            models_flavor: crate::config::ModelsFlavor::OpenAI,
            ..Default::default()
        };
        let key = Some("provider-key".to_string());

        let headers = upstream_auth_headers(&config, &key, None);

        assert_eq!(headers.get("authorization").unwrap(), "Bearer provider-key");
        assert!(headers.get("x-api-key").is_none());
    }

    #[test]
    fn non_stream_unsupported_detects_11101() {
        assert!(is_non_stream_unsupported(
            "{\"code\":11101,\"msg\":\"Non-stream chat request is currently not supported\"}"
        ));
        // 11101 also covers parameter parse failures, which must NOT be
        // retried as a stream — only the non-stream refusal carries the
        // "Non-stream" marker.
        assert!(!is_non_stream_unsupported(
            "{\"code\":11101,\"msg\":\"invalid parameter\"}"
        ));
        assert!(!is_non_stream_unsupported(
            "{\"code\":11148,\"msg\":\"tool calls and tool results do not match\"}"
        ));
    }

    #[test]
    fn degraded_prompt_replaces_system_message() {
        let mut req = openai::OpenAIRequest {
            model: "m".to_string(),
            messages: vec![
                openai::Message {
                    role: "system".to_string(),
                    content: Some(openai::MessageContent::Text(
                        "You are Claude Code, Anthropic's official CLI for Claude".to_string(),
                    )),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
                openai::Message {
                    role: "user".to_string(),
                    content: Some(openai::MessageContent::Text(
                        "what is the cache key?".to_string(),
                    )),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
            ],
            max_tokens: Some(1),
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        apply_degraded_prompt(&mut req);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        let sys = match req.messages[0].content.as_ref().unwrap() {
            openai::MessageContent::Text(t) => t,
            _ => panic!("expected text"),
        };
        assert!(sys.contains("helpful assistant"));
        assert!(!sys.contains("Claude Code"));
        // User message preserved.
        assert_eq!(req.messages[1].role, "user");
    }

    #[test]
    fn degraded_prompt_prepends_when_no_system() {
        let mut req = openai::OpenAIRequest {
            model: "m".to_string(),
            messages: vec![openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("hi".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            max_tokens: Some(1),
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        apply_degraded_prompt(&mut req);
        assert_eq!(req.messages.len(), 2);
        assert_eq!(req.messages[0].role, "system");
        assert_eq!(req.messages[1].role, "user");
    }

    #[derive(Debug)]
    struct TestError;
    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "test error")
        }
    }

    fn openai_chunk(
        id: &str,
        model: &str,
        content: Option<&str>,
        finish_reason: Option<&str>,
    ) -> String {
        let mut delta = json!({});
        if let Some(c) = content {
            delta["content"] = json!(c);
        }
        let mut choice = json!({ "index": 0, "delta": delta });
        if let Some(fr) = finish_reason {
            choice["finish_reason"] = json!(fr);
        }
        let chunk = json!({
            "id": id,
            "model": model,
            "choices": [choice],
        });
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    }

    fn openai_chunk_with_reasoning(id: &str, model: &str, reasoning: &str) -> String {
        let chunk = json!({
            "id": id,
            "model": model,
            "choices": [{ "index": 0, "delta": { "reasoning": reasoning } }],
        });
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    }

    fn openai_chunk_with_reasoning_content(id: &str, model: &str, reasoning: &str) -> String {
        let chunk = json!({
            "id": id,
            "model": model,
            "choices": [{ "index": 0, "delta": { "reasoning_content": reasoning } }],
        });
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    }

    fn openai_chunk_with_tool_call(
        id: &str,
        model: &str,
        tool_id: Option<&str>,
        name: Option<&str>,
        args: Option<&str>,
        finish_reason: Option<&str>,
    ) -> String {
        let mut tc = json!({ "index": 0 });
        if let Some(tid) = tool_id {
            tc["id"] = json!(tid);
            tc["type"] = json!("function");
        }
        let mut func = json!({});
        if let Some(n) = name {
            func["name"] = json!(n);
        }
        if let Some(a) = args {
            func["arguments"] = json!(a);
        }
        if !func.as_object().unwrap().is_empty() {
            tc["function"] = func;
        }
        let mut choice = json!({ "index": 0, "delta": { "tool_calls": [tc] } });
        if let Some(fr) = finish_reason {
            choice["finish_reason"] = json!(fr);
        }
        let chunk = json!({
            "id": id,
            "model": model,
            "choices": [choice],
        });
        format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap())
    }

    fn openai_done() -> String {
        "data: [DONE]\n\n".to_string()
    }

    fn make_stream(
        chunks: Vec<String>,
    ) -> impl futures::Stream<Item = Result<Bytes, TestError>> + Send + 'static {
        stream::iter(chunks.into_iter().map(|c| Ok(Bytes::from(c))))
    }

    fn mock_stats() -> std::sync::Arc<crate::stats::StatsDb> {
        // Inline mode: writes are synchronous, so tests can assert on them
        // immediately without waiting on the writer thread.
        crate::stats::StatsDb::in_memory().unwrap()
    }

    async fn collect_events(chunks: Vec<String>, model: &str) -> Vec<Value> {
        let s = make_stream(chunks);
        let sse = create_sse_stream(
            s,
            model.to_string(),
            std::sync::Arc::new(crate::settings::LogBuffer::new(2000)),
            mock_stats(),
        );
        tokio::pin!(sse);

        let mut events = Vec::new();
        while let Some(Ok(bytes)) = sse.next().await {
            let text = String::from_utf8_lossy(&bytes);
            for segment in text.split("\n\n").filter(|s| !s.is_empty()) {
                if let Some(data_line) = segment.lines().find(|l| l.starts_with("data: ")) {
                    let json_str = data_line.strip_prefix("data: ").unwrap();
                    if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                        events.push(v);
                    }
                }
            }
        }
        events
    }

    use crate::config::Config;
    use axum::http::HeaderMap;

    fn make_x_api_key_header(value: &str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            axum::http::header::HeaderName::from_static("x-api-key"),
            axum::http::HeaderValue::from_str(value).unwrap(),
        );
        headers
    }

    #[tokio::test]
    async fn resolve_api_key_passthrough_extracts_x_api_key() {
        let config = Config {
            passthrough_api_key: true,
            api_key: None,
            ..Default::default()
        };
        let headers = make_x_api_key_header("sk-my-test-key");
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, Some("sk-my-test-key".to_string()));
    }

    #[tokio::test]
    async fn resolve_api_key_passthrough_ignores_empty_header() {
        let config = Config {
            passthrough_api_key: true,
            api_key: None,
            ..Default::default()
        };
        // Empty header value returns None
        let key = super::resolve_api_key(&config, &HeaderMap::new());
        assert_eq!(key, None);

        // Explicitly empty value also returns None
        let headers = make_x_api_key_header("");
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, None);
    }

    #[tokio::test]
    async fn resolve_api_key_passthrough_returns_none_when_missing() {
        let config = Config {
            passthrough_api_key: true,
            api_key: None,
            ..Default::default()
        };
        let headers = HeaderMap::new();
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, None);
    }

    #[tokio::test]
    async fn resolve_api_key_static_key_when_passthrough_disabled() {
        let config = Config {
            passthrough_api_key: false,
            api_key: Some("sk-upstream".to_string()),
            ..Default::default()
        };
        // Even if x-api-key is present, static key wins when passthrough is off
        let headers = make_x_api_key_header("sk-ignored");
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, Some("sk-upstream".to_string()));
    }

    #[tokio::test]
    async fn resolve_api_key_both_missing_returns_none() {
        let config = Config {
            passthrough_api_key: false,
            api_key: None,
            ..Default::default()
        };
        let headers = HeaderMap::new();
        let key = super::resolve_api_key(&config, &headers);
        assert_eq!(key, None);
    }

    #[tokio::test]
    async fn resolve_responses_api_key_extracts_bearer_auth() {
        let config = Config {
            passthrough_api_key: true,
            api_key: None,
            ..Default::default()
        };
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            axum::http::HeaderValue::from_static("Bearer sk-bearer-test"),
        );
        let key = super::resolve_responses_api_key(&config, &headers);
        assert_eq!(key, Some("sk-bearer-test".to_string()));
    }

    #[tokio::test]
    async fn text_stream_produces_message_start_content_block_and_stop() {
        let chunks = vec![
            openai_chunk("chatcmpl-1", "gpt-4o", Some("Hello"), None),
            openai_chunk("chatcmpl-1", "gpt-4o", Some(" world"), None),
            openai_chunk("chatcmpl-1", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];

        let events = collect_events(chunks, "fallback").await;

        assert_eq!(events[0]["type"], "message_start");
        assert_eq!(events[0]["message"]["id"], "chatcmpl-1");
        assert_eq!(events[0]["message"]["model"], "gpt-4o");
        assert_eq!(events[0]["message"]["role"], "assistant");

        assert_eq!(events[1]["type"], "content_block_start");
        assert_eq!(events[1]["content_block"]["type"], "text");

        assert_eq!(events[2]["type"], "content_block_delta");
        assert_eq!(events[2]["delta"]["type"], "text_delta");
        assert_eq!(events[2]["delta"]["text"], "Hello");

        assert_eq!(events[3]["type"], "content_block_delta");
        assert_eq!(events[3]["delta"]["text"], " world");

        assert_eq!(events[4]["type"], "content_block_stop");

        assert_eq!(events[5]["type"], "message_delta");
        assert_eq!(events[5]["delta"]["stop_reason"], "end_turn");

        assert_eq!(events[6]["type"], "message_stop");
    }

    #[tokio::test]
    async fn reasoning_is_suppressed_text_only() {
        let chunks = vec![
            openai_chunk_with_reasoning("chatcmpl-2", "gpt-4o", "Let me think..."),
            openai_chunk_with_reasoning("chatcmpl-2", "gpt-4o", " more thinking"),
            openai_chunk("chatcmpl-2", "gpt-4o", Some("The answer is 42"), None),
            openai_chunk("chatcmpl-2", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];

        let events = collect_events(chunks, "fallback").await;

        // No `thinking` content block is emitted; reasoning is suppressed.
        assert_eq!(events[0]["type"], "message_start");
        assert_eq!(events[1]["type"], "content_block_start");
        assert_eq!(events[1]["content_block"]["type"], "text");
        assert_eq!(events[1]["index"], 0);
        assert_eq!(events[2]["delta"]["type"], "text_delta");
        assert_eq!(events[2]["delta"]["text"], "The answer is 42");
        assert_eq!(events[3]["type"], "content_block_stop");
        assert_eq!(events[4]["type"], "message_delta");
        assert_eq!(events[4]["delta"]["stop_reason"], "end_turn");
        assert_eq!(events[5]["type"], "message_stop");
    }

    #[tokio::test]
    async fn reasoning_content_is_suppressed() {
        let chunks = vec![
            openai_chunk_with_reasoning_content("chatcmpl-2", "gpt-4o", "Let me think..."),
            openai_chunk("chatcmpl-2", "gpt-4o", Some("The answer is 42"), None),
            openai_chunk("chatcmpl-2", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];

        let events = collect_events(chunks, "fallback").await;

        // reasoning_content must not become a `thinking` block.
        assert_eq!(events[1]["type"], "content_block_start");
        assert_eq!(events[1]["content_block"]["type"], "text");
        assert_eq!(events[2]["delta"]["type"], "text_delta");
        assert_eq!(events[2]["delta"]["text"], "The answer is 42");
    }

    #[tokio::test]
    async fn tool_call_stream_produces_tool_use_block() {
        let chunks = vec![
            openai_chunk_with_tool_call(
                "chatcmpl-3",
                "gpt-4o",
                Some("call_abc"),
                Some("read_file"),
                None,
                None,
            ),
            openai_chunk_with_tool_call(
                "chatcmpl-3",
                "gpt-4o",
                None,
                None,
                Some("{\"path\":"),
                None,
            ),
            openai_chunk_with_tool_call(
                "chatcmpl-3",
                "gpt-4o",
                None,
                None,
                Some("\"/tmp\"}"),
                None,
            ),
            openai_chunk("chatcmpl-3", "gpt-4o", None, Some("tool_calls")),
            openai_done(),
        ];

        let events = collect_events(chunks, "fallback").await;
        assert_eq!(events[1]["content_block"]["type"], "tool_use");
        assert_eq!(events[1]["content_block"]["id"], "call_abc");
        assert_eq!(events[5]["delta"]["stop_reason"], "tool_use");
    }

    #[tokio::test]
    async fn done_without_finish_reason_still_produces_message_stop() {
        let chunks = vec![
            openai_chunk("chatcmpl-4", "gpt-4o", Some("hi"), None),
            openai_done(),
        ];
        let events = collect_events(chunks, "fallback").await;
        assert_eq!(events.last().unwrap()["type"], "message_stop");
    }

    #[tokio::test]
    async fn fallback_model_used_when_upstream_omits_model() {
        let chunk = json!({
            "choices": [{ "index": 0, "delta": { "content": "hey" } }],
        });
        let chunks = vec![
            format!("data: {}\n\n", serde_json::to_string(&chunk).unwrap()),
            openai_chunk("id", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];
        let events = collect_events(chunks, "my-fallback-model").await;
        assert_eq!(events[0]["message"]["model"], "my-fallback-model");
    }

    #[tokio::test]
    async fn empty_content_chunks_are_not_emitted() {
        let chunks = vec![
            openai_chunk("chatcmpl-5", "gpt-4o", Some(""), None),
            openai_chunk("chatcmpl-5", "gpt-4o", Some("hello"), None),
            openai_chunk("chatcmpl-5", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];
        let events = collect_events(chunks, "fallback").await;
        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
            .collect();
        assert_eq!(text_deltas.len(), 1);
        assert_eq!(text_deltas[0]["delta"]["text"], "hello");
    }

    #[tokio::test]
    async fn stream_error_produces_error_event_and_stops() {
        let items: Vec<Result<Bytes, TestError>> = vec![
            Ok(Bytes::from(openai_chunk(
                "chatcmpl-6",
                "gpt-4o",
                Some("start"),
                None,
            ))),
            Err(TestError),
        ];
        let s = stream::iter(items);
        let sse = create_sse_stream(
            s,
            "fallback".to_string(),
            std::sync::Arc::new(crate::settings::LogBuffer::new(2000)),
            mock_stats(),
        );
        tokio::pin!(sse);

        let mut events = Vec::new();
        while let Some(Ok(bytes)) = sse.next().await {
            let text = String::from_utf8_lossy(&bytes);
            for segment in text.split("\n\n").filter(|s| !s.is_empty()) {
                if let Some(data_line) = segment.lines().find(|l| l.starts_with("data: ")) {
                    let json_str = data_line.strip_prefix("data: ").unwrap();
                    if let Ok(v) = serde_json::from_str::<Value>(json_str) {
                        events.push(v);
                    }
                }
            }
        }
        let error_events: Vec<_> = events.iter().filter(|e| e["type"] == "error").collect();
        assert_eq!(error_events.len(), 1);
    }

    #[tokio::test]
    async fn chunked_delivery_handles_split_sse_frames() {
        let full_chunk = openai_chunk("chatcmpl-7", "gpt-4o", Some("split"), None);
        let mid = full_chunk.len() / 2;
        let part1 = full_chunk[..mid].to_string();
        let part2 = format!(
            "{}{}{}",
            &full_chunk[mid..],
            openai_chunk("chatcmpl-7", "gpt-4o", None, Some("stop")),
            openai_done()
        );
        let events = collect_events(vec![part1, part2], "fallback").await;
        let text_deltas: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "content_block_delta" && e["delta"]["type"] == "text_delta")
            .collect();
        assert_eq!(text_deltas.len(), 1);
        assert_eq!(text_deltas[0]["delta"]["text"], "split");
    }

    #[tokio::test]
    async fn text_then_tool_call_produces_two_blocks() {
        let chunks = vec![
            openai_chunk("chatcmpl-8", "gpt-4o", Some("I'll read that file."), None),
            openai_chunk_with_tool_call(
                "chatcmpl-8",
                "gpt-4o",
                Some("call_xyz"),
                Some("read_file"),
                None,
                None,
            ),
            openai_chunk_with_tool_call(
                "chatcmpl-8",
                "gpt-4o",
                None,
                None,
                Some("{\"path\":\"/etc\"}"),
                None,
            ),
            openai_chunk("chatcmpl-8", "gpt-4o", None, Some("tool_calls")),
            openai_done(),
        ];
        let events = collect_events(chunks, "fallback").await;
        let block_starts: Vec<_> = events
            .iter()
            .filter(|e| e["type"] == "content_block_start")
            .collect();
        assert_eq!(block_starts.len(), 2);
        assert_eq!(block_starts[0]["content_block"]["type"], "text");
        assert_eq!(block_starts[1]["content_block"]["type"], "tool_use");
    }

    #[tokio::test]
    async fn responses_sse_stream_produces_valid_events() {
        let chunks = vec![
            openai_chunk("chatcmpl-resp", "gpt-4o", Some("Hello responses"), None),
            openai_chunk("chatcmpl-resp", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];
        let stream = stream::iter(
            chunks
                .into_iter()
                .map(|s| Ok::<_, TestError>(Bytes::from(s))),
        );
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let sse =
            super::create_responses_sse_stream(stream, "gpt-4o".to_string(), logs, mock_stats());
        tokio::pin!(sse);
        let mut raw = String::new();
        while let Some(item) = sse.next().await {
            let bytes = item.unwrap();
            raw.push_str(&String::from_utf8_lossy(&bytes));
        }

        assert!(raw.contains("event: response.created"));
        assert!(raw.contains("event: response.output_item.added"));
        assert!(raw.contains("event: response.output_text.delta"));
        assert!(raw.contains("event: response.completed"));
        assert!(raw.contains("data: [DONE]"));
    }

    /// A Responses client stops reading as soon as the terminal
    /// `response.completed` event arrives — it has no reason to wait for the
    /// trailing `data: [DONE]` sentinel, which only exists for SDKs that read
    /// until the stream closes.
    ///
    /// The recording block sits *after* the last `yield`, so dropping the
    /// response body at that point skips it and the request never reaches the
    /// request log. This is the regression: `/v1/responses` traffic that
    /// succeeds but writes no row.
    #[tokio::test]
    async fn responses_client_stopping_at_completed_still_logs_the_request() {
        let chunks = vec![
            openai_chunk("chatcmpl-resp", "gpt-4o", Some("Hello"), None),
            openai_chunk("chatcmpl-resp", "gpt-4o", None, Some("stop")),
            openai_done(),
        ];
        let s = make_stream(chunks);
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let stats = mock_stats();
        let sse = super::create_responses_sse_stream(s, "gpt-4o".to_string(), logs, stats.clone());
        // Box it, as `Body::from_stream` does for the real response: the guard
        // has to fire when the *body* is dropped, not when the local is.
        let mut sse = Box::pin(sse);

        // Read only up to and including the `response.completed` event, then
        // drop the body — exactly what an SDK doing
        // `stream.until_completed()` leaves behind.
        let mut raw = String::new();
        while let Some(item) = sse.next().await {
            raw.push_str(&String::from_utf8_lossy(&item.unwrap()));
            if raw.contains("event: response.completed") {
                break;
            }
        }
        drop(sse);

        let rows = stats
            .query_request_logs(&crate::stats::RequestLogFilter::default())
            .unwrap()
            .items;
        assert_eq!(
            rows.len(),
            1,
            "a completed /v1/responses stream must leave exactly one log row, found {}",
            rows.len()
        );
        assert_eq!(rows[0].route, "/v1/responses");
        assert_eq!(rows[0].status, 200);
    }

    /// Upstreams such as WorkBuddy send `"finish_reason": ""` on every chunk.
    /// The stream must stay open through those, and only close on the real
    /// reason — otherwise the client gets an empty completed response.
    #[tokio::test]
    async fn responses_stream_with_empty_finish_reason_still_delivers_text() {
        let chunks = vec![
            openai_chunk("cmb-1", "glm-5.3-flash", Some("OK"), Some("")),
            openai_chunk("cmb-1", "glm-5.3-flash", Some("!"), Some("")),
            openai_chunk("cmb-1", "glm-5.3-flash", None, Some("stop")),
            openai_done(),
        ];
        let s = stream::iter(
            chunks
                .into_iter()
                .map(|c| Ok::<_, TestError>(Bytes::from(c))),
        );
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let sse =
            super::create_responses_sse_stream(s, "glm-5.3-flash".to_string(), logs, mock_stats());
        tokio::pin!(sse);
        let mut raw = String::new();
        while let Some(item) = sse.next().await {
            raw.push_str(&String::from_utf8_lossy(&item.unwrap()));
        }

        // `completed` must come last and carry the accumulated text — not
        // arrive first with an empty `output` array.
        let first_done = raw.find("response.output_text.done").unwrap();
        let completed = raw.find("response.completed").unwrap();
        assert!(
            first_done < completed,
            "text must be closed before the response completes: {raw}"
        );
        // The completed event itself must carry the accumulated text, not an
        // empty `output` array. (`response.created` legitimately has one.)
        let completed_body = &raw[completed..];
        assert!(
            completed_body.contains("OK!"),
            "completed response must carry the text: {raw}"
        );
        assert!(
            completed_body.contains("\"status\":\"completed\""),
            "completed event must be the terminal one: {raw}"
        );
    }

    #[tokio::test]
    async fn responses_handler_end_to_end_non_streaming() {
        use axum::routing::post;
        use axum::Json;
        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            post(|Json(req): Json<openai::OpenAIRequest>| async move {
                assert_eq!(req.messages.len(), 1);
                assert_eq!(req.messages[0].role, "user");
                Json(openai::OpenAIResponse {
                    id: Some("chatcmpl-mock".to_string()),
                    object: Some("chat.completion".to_string()),
                    created: Some(1712345678),
                    model: Some("gpt-4o".to_string()),
                    choices: vec![openai::Choice {
                        index: 0,
                        message: openai::ChoiceMessage {
                            role: "assistant".to_string(),
                            content: Some("Response from mock".to_string()),
                            tool_calls: None,
                        },
                        finish_reason: Some("stop".to_string()),
                    }],
                    usage: openai::Usage {
                        prompt_tokens: 12,
                        completion_tokens: 6,
                        total_tokens: 18,
                        prompt_tokens_details: None,
                        cache_creation_input_tokens: None,
                        cache_read_input_tokens: None,
                        ..Default::default()
                    },
                    system_fingerprint: None,
                })
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_upstream).await.unwrap();
        });

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
            ..Default::default()
        });
        let client = reqwest::Client::new();
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let service = crate::service::ServiceController::new(false);
        service.mark_running();
        // The real Codex header set, reduced to the fields the session
        // resolver reads, so this test also proves the end-to-end attribution.
        let mut headers = HeaderMap::new();
        headers.insert("originator", "Codex".parse().unwrap());
        headers.insert(
            "session-id",
            "01a0cc30-3318-74d2-b045-650a0b0c2e1c".parse().unwrap(),
        );
        headers.insert(
            "x-codex-turn-metadata",
            r#"{"session_id":"01a0cc30-3318-74d2-b045-650a0b0c2e1c","turn_id":"01a0cc31-bc18-77c3-85d8-de3c452f781c"}"#
                .parse()
                .unwrap(),
        );
        let stats_db = mock_stats();
        let req = responses::ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: responses::ResponsesInput::Text("Hello proxy".to_string()),
            instructions: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            max_tokens: None,
            stream: Some(false),
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
            prompt_cache_key: None,
        };

        let response = super::responses_proxy_handler(
            axum::Extension(config),
            axum::Extension(client),
            axum::Extension(logs.clone()),
            axum::Extension(service),
            axum::Extension(stats_db.clone()),
            axum::Extension(std::sync::Arc::new(
                crate::session_pool::CredentialPool::empty(),
            )),
            headers,
            json_request(&req),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // The session must reach the log line, the request-log row and the
        // stats filter, which is the entire point of the change.
        let lines = logs.snapshot().await;
        let headers_line = lines
            .iter()
            .find(|l| l.message.starts_with("POST /v1/responses client=codex"))
            .expect("headers line carries the resolved session");
        assert!(
            headers_line
                .message
                .contains("session_id=codex:01a0cc30-3318-74d2-b045-650a0b0c2e1c"),
            "unexpected log line: {}",
            headers_line.message
        );
        assert!(
            headers_line
                .message
                .contains("turn=01a0cc31-bc18-77c3-85d8-de3c452f781c"),
            "unexpected log line: {}",
            headers_line.message
        );

        let rows = stats_db
            .query_request_logs(&crate::stats::RequestLogFilter {
                session_id: Some("codex:01a0cc30-3318-74d2-b045-650a0b0c2e1c".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(rows.items.len(), 1, "row is filterable by session id");
        assert_eq!(rows.items[0].client, "codex");
        assert_eq!(rows.clients, vec!["codex".to_string()]);

        // A different session must not match.
        let other = stats_db
            .query_request_logs(&crate::stats::RequestLogFilter {
                session_id: Some("codex:00000000-0000-7000-8000-000000000000".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(other.items.len(), 0);
    }

    #[tokio::test]
    async fn responses_handler_end_to_end_streaming() {
        use axum::routing::post;
        use axum::Json;
        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            post(|Json(_req): Json<openai::OpenAIRequest>| async move {
                let stream = futures::stream::iter(vec![
                    Ok::<_, std::io::Error>(Bytes::from(openai_chunk(
                        "chatcmpl-stream-mock",
                        "gpt-4o",
                        Some("Streamed response"),
                        None,
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from(openai_chunk(
                        "chatcmpl-stream-mock",
                        "gpt-4o",
                        None,
                        Some("stop"),
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from(openai_done())),
                ]);
                let mut headers = HeaderMap::new();
                headers.insert(
                    "Content-Type",
                    axum::http::HeaderValue::from_static("text/event-stream"),
                );
                (headers, axum::body::Body::from_stream(stream)).into_response()
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_upstream).await.unwrap();
        });

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
            ..Default::default()
        });
        let client = reqwest::Client::new();
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let service = crate::service::ServiceController::new(false);
        service.mark_running();
        let headers = HeaderMap::new();
        let req = responses::ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: responses::ResponsesInput::Text("Hello stream".to_string()),
            instructions: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            max_tokens: None,
            stream: Some(true),
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
            prompt_cache_key: None,
        };

        let response = super::responses_proxy_handler(
            axum::Extension(config),
            axum::Extension(client),
            axum::Extension(logs),
            axum::Extension(service),
            axum::Extension(mock_stats()),
            axum::Extension(std::sync::Arc::new(
                crate::session_pool::CredentialPool::empty(),
            )),
            headers,
            json_request(&req),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );
    }

    #[test]
    fn test_usage_to_token_record_with_cache_variants() {
        // 1. OpenAI format with prompt_tokens_details.cached_tokens
        let usage_openai = openai::Usage {
            prompt_tokens: 100,
            completion_tokens: 30,
            total_tokens: 130,
            prompt_tokens_details: Some(openai::PromptTokensDetails {
                cached_tokens: 80,
                audio_tokens: 0,
            }),
            ..Default::default()
        };
        let rec = usage_openai.to_token_record();
        assert_eq!(rec.input, 20);
        assert_eq!(rec.cache_read, 80);
        assert_eq!(rec.cache_write, 0);
        assert_eq!(rec.output, 30);

        // 2. DeepSeek format with prompt_cache_hit_tokens
        let usage_deepseek = openai::Usage {
            prompt_tokens: 100,
            completion_tokens: 25,
            total_tokens: 125,
            prompt_cache_hit_tokens: Some(70),
            prompt_cache_miss_tokens: Some(30),
            ..Default::default()
        };
        let rec = usage_deepseek.to_token_record();
        assert_eq!(rec.input, 30);
        assert_eq!(rec.cache_read, 70);
        assert_eq!(rec.output, 25);

        // 3. Anthropic format with cache_read_input_tokens & cache_creation_input_tokens
        let usage_anthropic = openai::Usage {
            prompt_tokens: 150,
            completion_tokens: 50,
            total_tokens: 200,
            cache_read_input_tokens: Some(100),
            cache_creation_input_tokens: Some(30),
            ..Default::default()
        };
        let rec = usage_anthropic.to_token_record();
        assert_eq!(rec.input, 20); // 150 - 100 - 30
        assert_eq!(rec.cache_read, 100);
        assert_eq!(rec.cache_write, 30);
        assert_eq!(rec.output, 50);

        // 4. Top-level cached_tokens
        let usage_toplevel = openai::Usage {
            prompt_tokens: 80,
            completion_tokens: 10,
            total_tokens: 90,
            cached_tokens: Some(50),
            ..Default::default()
        };
        let rec = usage_toplevel.to_token_record();
        assert_eq!(rec.input, 30);
        assert_eq!(rec.cache_read, 50);
        assert_eq!(rec.output, 10);

        // 5. WorkBuddy shape: Anthropic-style fields present but zeroed, with
        //    the real hit count in the DeepSeek field. The zero must not win.
        let usage_workbuddy = openai::Usage {
            prompt_tokens: 1337,
            completion_tokens: 10,
            total_tokens: 1347,
            cache_read_input_tokens: Some(0),
            cache_creation_input_tokens: Some(0),
            prompt_cache_hit_tokens: Some(1216),
            prompt_cache_miss_tokens: Some(121),
            prompt_tokens_details: Some(openai::PromptTokensDetails {
                cached_tokens: 1216,
                audio_tokens: 0,
            }),
            ..Default::default()
        };
        let rec = usage_workbuddy.to_token_record();
        assert_eq!(
            rec.cache_read, 1216,
            "zero-valued fields must not mask a real hit"
        );
        assert_eq!(
            rec.input, 121,
            "uncached input should be prompt_tokens - hits"
        );
        assert_eq!(rec.output, 10);
    }

    /// A response with no cache information reports zero rather than inventing
    /// a hit from an unrelated field.
    #[test]
    fn test_usage_cache_read_zero_when_absent() {
        let usage = openai::Usage {
            prompt_tokens: 500,
            completion_tokens: 20,
            total_tokens: 520,
            cache_read_input_tokens: Some(0),
            ..Default::default()
        };
        let rec = usage.to_token_record();
        assert_eq!(rec.cache_read, 0);
        assert_eq!(rec.input, 500);
    }

    /// Explicit cache fields are still honored when they carry the real value.
    #[test]
    fn test_usage_explicit_zero_is_not_overridden_by_details() {
        // Anthropic-style reads win only when positive; a zero reads as "no
        // cache" and the remaining sources are consulted instead.
        let usage = openai::Usage {
            prompt_tokens: 200,
            completion_tokens: 5,
            total_tokens: 205,
            cache_read_input_tokens: Some(0),
            cached_tokens: Some(150),
            ..Default::default()
        };
        assert_eq!(usage.cache_read_tokens(), 150);
    }

    #[tokio::test]
    async fn chat_completions_handler_end_to_end_non_streaming() {
        use axum::routing::post;
        use axum::Json;

        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            post(|Json(req): Json<openai::OpenAIRequest>| async move {
                // Verify model was remapped by proxy
                assert_eq!(req.model, "gpt-4o-upstream");
                // Verify extra parameter was preserved
                assert_eq!(
                    req.extra.get("reasoning_effort").and_then(|v| v.as_str()),
                    Some("low")
                );

                let resp = openai::OpenAIResponse {
                    id: Some("chatcmpl-test-123".to_string()),
                    object: Some("chat.completion".to_string()),
                    created: Some(1700000000),
                    model: Some("gpt-4o-upstream".to_string()),
                    choices: vec![openai::Choice {
                        index: 0,
                        message: openai::ChoiceMessage {
                            role: "assistant".to_string(),
                            content: Some("Hello from OpenAI mock!".to_string()),
                            tool_calls: None,
                        },
                        finish_reason: Some("stop".to_string()),
                    }],
                    usage: openai::Usage {
                        prompt_tokens: 100,
                        completion_tokens: 40,
                        total_tokens: 140,
                        prompt_tokens_details: Some(openai::PromptTokensDetails {
                            cached_tokens: 60,
                            audio_tokens: 0,
                        }),
                        ..Default::default()
                    },
                    system_fingerprint: None,
                };
                Json(resp).into_response()
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_upstream).await.unwrap();
        });

        let mut model_map = std::collections::BTreeMap::new();
        model_map.insert("gpt-alias".to_string(), "gpt-4o-upstream".to_string());

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
            model_map,
            ..Default::default()
        });
        let client = reqwest::Client::new();
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let service = crate::service::ServiceController::new(false);
        service.mark_running();
        let stats_db = mock_stats();

        let mut extra = serde_json::Map::new();
        extra.insert("reasoning_effort".to_string(), json!("low"));

        let req = openai::OpenAIRequest {
            model: "gpt-alias".to_string(),
            messages: vec![openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("Hi".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            max_tokens: Some(100),
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra,
        };

        let response = super::chat_completions_proxy_handler(
            axum::Extension(config),
            axum::Extension(client),
            axum::Extension(logs),
            axum::Extension(service),
            axum::Extension(stats_db.clone()),
            axum::Extension(std::sync::Arc::new(
                crate::session_pool::CredentialPool::empty(),
            )),
            HeaderMap::new(),
            json_request(&req),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);

        // Verify stats in SQLite
        let today = stats_db.query_today().unwrap();
        assert_eq!(today.requests_success, 1);
        assert_eq!(today.tokens_input, 40); // 100 - 60 cached
        assert_eq!(today.tokens_cache_read, 60);
        assert_eq!(today.tokens_output, 40);
        assert_eq!(today.cache_hit_pct(), 60);
    }

    #[tokio::test]
    async fn chat_completions_handler_end_to_end_streaming() {
        use axum::routing::post;
        use axum::Json;

        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            post(|Json(req): Json<openai::OpenAIRequest>| async move {
                // Verify include_usage was set to true for stream
                assert!(req.stream_options.map(|s| s.include_usage).unwrap_or(false));

                let chunk1 = json!({
                    "id": "chatcmpl-stream-1",
                    "object": "chat.completion.chunk",
                    "created": 1700000000,
                    "model": "gpt-4o",
                    "choices": [{
                        "index": 0,
                        "delta": { "content": "Streamed " },
                        "finish_reason": null
                    }]
                });

                let chunk2 = json!({
                    "id": "chatcmpl-stream-1",
                    "object": "chat.completion.chunk",
                    "created": 1700000000,
                    "model": "gpt-4o",
                    "choices": [{
                        "index": 0,
                        "delta": { "content": "chat!" },
                        "finish_reason": "stop"
                    }]
                });

                let chunk_usage = json!({
                    "id": "chatcmpl-stream-1",
                    "object": "chat.completion.chunk",
                    "created": 1700000000,
                    "model": "gpt-4o",
                    "choices": [],
                    "usage": {
                        "prompt_tokens": 100,
                        "completion_tokens": 20,
                        "total_tokens": 120,
                        "prompt_cache_hit_tokens": 80,
                        "prompt_cache_miss_tokens": 20
                    }
                });

                let stream = futures::stream::iter(vec![
                    Ok::<_, std::io::Error>(Bytes::from(format!(
                        "data: {}\n\n",
                        serde_json::to_string(&chunk1).unwrap()
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from(format!(
                        "data: {}\n\n",
                        serde_json::to_string(&chunk2).unwrap()
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from(format!(
                        "data: {}\n\n",
                        serde_json::to_string(&chunk_usage).unwrap()
                    ))),
                    Ok::<_, std::io::Error>(Bytes::from("data: [DONE]\n\n")),
                ]);

                let mut headers = HeaderMap::new();
                headers.insert(
                    "Content-Type",
                    axum::http::HeaderValue::from_static("text/event-stream"),
                );
                (headers, axum::body::Body::from_stream(stream)).into_response()
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_upstream).await.unwrap();
        });

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
            ..Default::default()
        });
        let client = reqwest::Client::new();
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let service = crate::service::ServiceController::new(false);
        service.mark_running();
        let stats_db = mock_stats();

        let req = openai::OpenAIRequest {
            model: "gpt-4o".to_string(),
            messages: vec![openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("Stream test".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(true),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };

        let response = super::chat_completions_proxy_handler(
            axum::Extension(config),
            axum::Extension(client),
            axum::Extension(logs),
            axum::Extension(service),
            axum::Extension(stats_db.clone()),
            axum::Extension(std::sync::Arc::new(
                crate::session_pool::CredentialPool::empty(),
            )),
            HeaderMap::new(),
            json_request(&req),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "text/event-stream"
        );

        // Read the stream to completion so create_chat_sse_stream finishes
        let _ = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();

        // Verify stats in SQLite
        let today = stats_db.query_today().unwrap();
        assert_eq!(today.requests_success, 1);
        assert_eq!(today.tokens_input, 20); // 100 - 80 cached
        assert_eq!(today.tokens_cache_read, 80);
        assert_eq!(today.tokens_output, 20);
        assert_eq!(today.cache_hit_pct(), 80);
    }

    /// A provider that only serves streaming bodies (`force_stream`): the
    /// non-streaming client still gets one plain JSON body, and the upstream
    /// sees exactly one request, with `stream:true` on it.
    #[tokio::test]
    async fn force_stream_provider_serves_a_non_streaming_client_one_json_body() {
        use axum::routing::post;
        use axum::Json;

        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let calls_for_handler = calls.clone();

        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            post(move |Json(req): Json<openai::OpenAIRequest>| {
                let calls = calls_for_handler.clone();
                async move {
                    calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);

                    // A plain body is refused outright, exactly as WorkBuddy
                    // does. If the proxy ever stops upgrading, this fires.
                    if req.stream != Some(true) {
                        return (
                            axum::http::StatusCode::BAD_REQUEST,
                            Json(json!({
                                "code": 11101,
                                "msg": "Non-stream chat request is currently not supported"
                            })),
                        )
                            .into_response();
                    }
                    assert!(
                        req.stream_options.map(|s| s.include_usage).unwrap_or(false),
                        "an upgraded request must ask for usage"
                    );

                    let chunk1 = json!({
                        "id": "cmb-1", "object": "chat.completion.chunk",
                        "created": 1700000000, "model": "glm",
                        "choices": [{"index": 0, "delta": {"content": "Hello "}}]
                    });
                    let chunk2 = json!({
                        "id": "cmb-1", "object": "chat.completion.chunk",
                        "created": 1700000000, "model": "glm",
                        "choices": [{"index": 0, "delta": {"content": "world"}, "finish_reason": "stop"}]
                    });
                    let chunk_usage = json!({
                        "id": "cmb-1", "object": "chat.completion.chunk",
                        "created": 1700000000, "model": "glm", "choices": [],
                        "usage": {"prompt_tokens": 7, "completion_tokens": 2, "total_tokens": 9}
                    });

                    let stream = futures::stream::iter(
                        [chunk1, chunk2, chunk_usage]
                            .into_iter()
                            .map(|c| {
                                Ok::<_, std::io::Error>(Bytes::from(format!(
                                    "data: {}\n\n",
                                    serde_json::to_string(&c).unwrap()
                                )))
                            })
                            .chain(std::iter::once(Ok::<_, std::io::Error>(Bytes::from(
                                "data: [DONE]\n\n",
                            )))),
                    );

                    let mut headers = HeaderMap::new();
                    headers.insert(
                        "Content-Type",
                        axum::http::HeaderValue::from_static("text/event-stream"),
                    );
                    (headers, axum::body::Body::from_stream(stream)).into_response()
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            axum::serve(listener, mock_upstream).await.unwrap();
        });

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://127.0.0.1:{}", port)],
            force_stream_upstream: true,
            ..Default::default()
        });
        let service = crate::service::ServiceController::new(false);
        service.mark_running();

        let req = openai::OpenAIRequest {
            model: "glm".to_string(),
            messages: vec![openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("Hi".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            // The client explicitly asked for a single body, not a stream.
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };

        let response = super::chat_completions_proxy_handler(
            axum::Extension(config),
            axum::Extension(reqwest::Client::new()),
            axum::Extension(std::sync::Arc::new(crate::settings::LogBuffer::new(10))),
            axum::Extension(service),
            axum::Extension(mock_stats()),
            axum::Extension(std::sync::Arc::new(
                crate::session_pool::CredentialPool::empty(),
            )),
            HeaderMap::new(),
            json_request(&req),
        )
        .await
        .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(
            response.headers().get("content-type").unwrap(),
            "application/json",
            "a non-streaming client must not be handed an event stream"
        );

        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let parsed: openai::OpenAIResponse = serde_json::from_slice(&body).unwrap();

        // The two SSE chunks were aggregated back into one assistant message.
        assert_eq!(parsed.choices.len(), 1);
        assert_eq!(
            parsed.choices[0].message.content.as_deref(),
            Some("Hello world")
        );
        assert_eq!(parsed.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(parsed.usage.prompt_tokens, 7);
        assert_eq!(parsed.usage.completion_tokens, 2);

        // And it took a single upstream call: the upgrade happened up front,
        // rather than as a retry after the 11101 refusal.
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    /// Feeds SSE text through `collect_stream_into_response` by serving it
    /// from a real HTTP socket, so the bytes arrive as a genuine response body.
    async fn aggregate(sse: &str) -> openai::OpenAIResponse {
        let body = sse.to_string();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(v) => v,
                    Err(_) => return,
                };
                let payload = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                use tokio::io::AsyncWriteExt;
                let _ = sock.write_all(payload.as_bytes()).await;
                let _ = sock.flush().await;
            }
        });

        let resp = reqwest::get(format!("http://{}/", addr)).await.unwrap();
        super::collect_stream_into_response(resp).await.unwrap()
    }

    fn text_chunk(content: &str, finish: Option<&str>) -> String {
        let delta = json!({"content": content});
        let mut choice = json!({"index": 0, "delta": delta});
        if let Some(f) = finish {
            choice["finish_reason"] = json!(f);
        }
        format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id": "cmb-1", "model": "glm", "choices": [choice]
            }))
            .unwrap()
        )
    }

    #[tokio::test]
    async fn aggregate_merges_text_chunks() {
        let sse = format!(
            "{}{}{}",
            text_chunk("Hello", Some("")),
            text_chunk(" world", Some("")),
            text_chunk("", Some("stop")),
        );
        let resp = aggregate(&sse).await;
        assert_eq!(resp.choices.len(), 1);
        assert_eq!(
            resp.choices[0].message.content.as_deref(),
            Some("Hello world")
        );
        assert_eq!(resp.choices[0].finish_reason.as_deref(), Some("stop"));
        assert_eq!(resp.choices[0].message.role, "assistant");
    }

    #[tokio::test]
    async fn aggregate_merges_split_tool_call_arguments() {
        // Arguments arrive in fragments across chunks, keyed by index.
        let mk = |id: Option<&str>, name: Option<&str>, args: Option<&str>| {
            let mut func = json!({});
            if let Some(n) = name {
                func["name"] = json!(n);
            }
            if let Some(a) = args {
                func["arguments"] = json!(a);
            }
            let mut tc = json!({"index": 0, "function": func});
            if let Some(i) = id {
                tc["id"] = json!(i);
                tc["type"] = json!("function");
            }
            format!(
                "data: {}\n\n",
                serde_json::to_string(&json!({
                    "id": "cmb-1", "model": "glm",
                    "choices": [{"index": 0, "delta": {"tool_calls": [tc]}}]
                }))
                .unwrap()
            )
        };

        let sse = format!(
            "{}{}{}{}",
            mk(Some("c1"), Some("exec_command"), None),
            mk(None, None, Some(r#"{\"cmd\""#)),
            mk(None, None, Some(r#":\"ls\"}"#)),
            text_chunk("", Some("tool_calls")),
        );
        let resp = aggregate(&sse).await;
        let calls = resp.choices[0]
            .message
            .tool_calls
            .as_ref()
            .expect("tool call");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[0].function.name, "exec_command");
        assert_eq!(calls[0].function.arguments, r#"{\"cmd\":\"ls\"}"#);
    }

    #[tokio::test]
    async fn aggregate_captures_usage_and_drops_half_open_calls() {
        // A tool call that never received an id would be unanswerable, so it
        // must not reach the client.
        let usage_chunk = format!(
            "data: {}\n\n",
            serde_json::to_string(&json!({
                "id": "cmb-1", "model": "glm",
                "choices": [{"index": 0, "delta": {"tool_calls": [
                    {"index": 0, "function": {"name": "orphan"}}
                ]}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 10, "completion_tokens": 4, "total_tokens": 14}
            }))
            .unwrap()
        );
        let sse = format!("{}{}", text_chunk("hi", Some("")), usage_chunk);
        let resp = aggregate(&sse).await;
        assert_eq!(resp.usage.prompt_tokens, 10);
        assert_eq!(resp.usage.completion_tokens, 4);
        assert!(
            resp.choices[0].message.tool_calls.is_none(),
            "half-open tool call must be dropped"
        );
    }

    #[tokio::test]
    async fn non_streaming_request_is_upgraded_when_provider_requires_stream() {
        // WorkBuddy rejects a plain body (11101), so the outbound request must
        // ask for a stream even though the client did not.
        let seen = std::sync::Arc::new(std::sync::Mutex::new(None));
        let seen_for_handler = seen.clone();
        let mock_upstream = axum::Router::new().route(
            "/v1/chat/completions",
            axum::routing::post(
                |axum::Json(req): axum::Json<openai::OpenAIRequest>| async move {
                    *seen_for_handler.lock().unwrap() =
                        Some((req.stream, req.stream_options.is_some()));
                    axum::Json(openai::OpenAIResponse {
                        id: Some("cmb-1".into()),
                        object: Some("chat.completion".into()),
                        created: Some(1),
                        model: Some("glm".into()),
                        choices: vec![openai::Choice {
                            index: 0,
                            message: openai::ChoiceMessage {
                                role: "assistant".into(),
                                content: Some("direct".into()),
                                tool_calls: None,
                            },
                            finish_reason: Some("stop".into()),
                        }],
                        usage: openai::Usage::default(),
                        system_fingerprint: None,
                    })
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock_upstream).await.unwrap() });

        let config = std::sync::Arc::new(Config {
            upstream_urls: vec![format!("http://{}/v1/chat/completions", addr)],
            force_stream_upstream: true,
            ..Default::default()
        });
        let req = openai::OpenAIRequest {
            model: "glm".into(),
            messages: vec![openai::Message {
                role: "user".into(),
                content: Some(openai::MessageContent::Text("hi".into())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            }],
            max_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: None,
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };

        let resp = super::forward_request(
            config,
            reqwest::Client::new(),
            req,
            None,
            std::sync::Arc::new(crate::settings::LogBuffer::new(10)),
            "glm".to_string(),
            mock_stats(),
            super::ApiFlavor::Chat,
            false,
            "/v1/chat/completions",
            std::time::Instant::now(),
            SessionInfo::unknown(),
            "client=unknown".to_string(),
            Default::default(),
            std::sync::Arc::new(crate::session_pool::CredentialPool::empty()),
            None,
        )
        .await
        .unwrap();
        assert_eq!(resp.status(), axum::http::StatusCode::OK);

        let (stream, has_opts) = seen.lock().unwrap().unwrap();
        assert_eq!(stream, Some(true), "must upgrade to stream upstream");
        assert!(has_opts, "must request usage in the stream");
    }
}
