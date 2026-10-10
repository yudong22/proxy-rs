//! Request path for all three client protocols.
//!
//! This file is the module root: the protocol handlers and the shared upstream
//! send/retry path, plus the module declarations for the clusters that were split
//! out to keep this file readable:
//!
//!   * [`auth_resolvers`] — which API key the caller presented;
//!   * [`auth_headers`] — upstream identity + CLI fingerprint headers;
//!   * [`failover`] — when to relocate a session off the configured default;
//!   * [`upstream_errors`] — classifying, describing and repairing upstream failures;
//!   * [`sse`] — SSE framing, the request-log ledger and the TTFT/speed tracker.
//!
//! Everything moved out is re-exported below under its original name, so every
//! pre-existing `crate::proxy::X` path (and the tests' `super::X`) keeps resolving
//! unchanged — the split is a file-layout change, not an API change.

mod auth_headers;
mod auth_resolvers;
mod failover;
mod sse;
mod upstream_errors;

#[cfg(test)]
mod tests;

// Bring the split-out items back into this module's namespace under their
// historical names, so every existing `crate::proxy::X` path and the tests'
// `super::X` keep resolving. `pub(crate) use` is required, not cosmetic: the test
// module is a *sibling* of these files (`proxy/tests.rs`), so `super::X` resolves
// through `crate::proxy` and a private import would not be visible to it.
pub(crate) use auth_headers::*;
pub(crate) use auth_resolvers::*;
pub(crate) use failover::*;
pub(crate) use sse::*;
pub(crate) use upstream_errors::*;

use crate::config::{Config, ModelsFlavor};
use crate::error::{ProxyError, ProxyResult};
use crate::metrics;
use crate::models::{anthropic, openai, responses};
use crate::service;
use crate::session::{self, ClientKind, SessionInfo};
use crate::stats::{RequestOutcome, StatsDb, TokenRecord, UpstreamTiming};
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
                0,
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

    let estimated_input_tokens = openai_req.estimate_input_tokens();

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
        estimated_input_tokens,
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
                0,
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

    let estimated_input_tokens = openai_req.estimate_input_tokens();

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
        estimated_input_tokens,
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

    // Normalize role: "developer" to "system" (for compatibility with upstreams expecting standard roles)
    for msg in &mut req.messages {
        if msg.role == "developer" {
            msg.role = "system".to_string();
        }
    }

    // Fall back max_tokens to max_completion_tokens if not explicitly set
    if req.max_tokens.is_none() && req.max_completion_tokens.is_some() {
        req.max_tokens = req.max_completion_tokens;
    }

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

    let estimated_input_tokens = req.estimate_input_tokens();

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
        estimated_input_tokens,
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

    record_stats_row(
        stats,
        RequestOutcome {
            model: "",
            route,
            tokens: &TokenRecord::default(),
            duration_ms,
            // Rejected before any upstream attempt: nothing was ever sent.
            timing: UpstreamTiming::default(),
            streamed: false,
            status,
            error: Some(&message),
            session_id: &session.session_id,
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
fn record_stats_row(stats: &Arc<StatsDb>, outcome: RequestOutcome<'_>) {
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
                    model: client_model,
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
    let mut current_api_key = api_key;
    let mut current_key_id = if active_credential.is_none() {
        if let Some(entry) = crate::workbuddy_auth::active_api_key_entry() {
            Some(entry.id)
        } else if let Some(ref k) = current_api_key {
            crate::workbuddy_auth::enabled_api_keys()
                .into_iter()
                .find(|e| e.key.trim() == k.trim())
                .map(|e| e.id)
        } else {
            None
        }
    } else {
        None
    };
    let mut tried_key_ids = std::collections::HashSet::new();
    if let Some(ref kid) = current_key_id {
        tried_key_ids.insert(kid.clone());
    }

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
    let enabled_keys_count = crate::workbuddy_auth::enabled_api_keys().len();
    let total_identities = pool_size + enabled_keys_count;
    let max_credential_attempts = if config.session_switch_enabled {
        total_identities.clamp(1, 5)
    } else {
        1
    };

    'credential: for credential_attempt in 0..max_credential_attempts {
        let mut current_credential = active_credential.clone();

        // A credential that a recent request already proved broken is skipped
        // outright, instead of re-paying the failing round-trip (and the 3 s
        // `not_found` wait) on every single request. Only the very first
        // attempt consults the memory; once the request is under way the
        // normal discovery path owns it, so a memory that turns out to be
        // wrong just costs one ordinary retry.
        if credential_attempt == 0 {
            if let Some(target) = remembered_failover(&pool, current_credential.as_ref()).await {
                match target {
                    FailoverTarget::Credential(id) => {
                        if let Some(next) = pool
                            .snapshot()
                            .await
                            .into_iter()
                            .find(|c| c.id == id && c.enabled)
                        {
                            if let Ok(mut t) = overrides.lock() {
                                t.set_key(next.id.clone(), "复用近期可用凭据");
                                t.set_model(
                                    openai_req.model.clone(),
                                    "复用近期可用凭据(跳过失败探测)",
                                );
                            }
                            active_credential = Some(next.clone());
                            current_credential = Some(next);
                        }
                    }
                    FailoverTarget::ApiKey(id) => {
                        if let Some(key) = crate::workbuddy_auth::enabled_api_keys()
                            .into_iter()
                            .find(|k| k.id == id)
                        {
                            if let Ok(mut t) = overrides.lock() {
                                t.set_key(key.id.clone(), "复用近期可用密钥");
                                t.set_model(
                                    openai_req.model.clone(),
                                    "复用近期可用密钥(跳过失败探测)",
                                );
                            }
                            tried_key_ids.insert(key.id.clone());
                            active_credential = None;
                            current_credential = None;
                            current_api_key = Some(key.key.clone());
                            current_key_id = Some(key.id.clone());
                        }
                    }
                }
            }
        }

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
                    &current_api_key,
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
                    // ── 401 special-case: distinguish token expiry from routing failure ──
                    //
                    // Tencent / WorkBuddy upstreams return HTTP 401 for two very
                    // different root causes that call for opposite treatments:
                    //
                    //  a) {"message":"not_found"} — the gateway could not route to
                    //     the session / pod. The access token is fine; refreshing it
                    //     wastes ~200 ms and won't fix a routing hiccup. A 3s
                    //     wait lets the load-balancer resolve the bad path; if the
                    //     retry still fails we fall through to the account / key switch.
                    //
                    //  b) Any other 401 — the access token is stale or revoked.
                    //     Renew it and retry the *same* credential once before
                    //     switching; with a single account, failing over has nowhere
                    //     to go and would surface the 401 to the client directly.
                    if status.as_u16() == 401 && body.contains("not_found") {
                        // Only pay the exploratory wait while the failure is
                        // still unknown. Once a recent request has already
                        // proven this credential broken, re-probing it costs
                        // the client 3 s for an answer we can predict — go
                        // straight to the failover we know works.
                        let already_known = current_credential
                            .as_ref()
                            .map(|c| failover_is_known(&c.id))
                            .unwrap_or(false);
                        if attempt == 0 && !already_known {
                            // First attempt: brief 3s pause, then retry without refresh.
                            gui_logs
                                .push(
                                    "WARN",
                                    format!(
                                        "UPSTREAM 401 not_found (疑似瞬时路由故障), 3s 后重试同一凭据 model={} {}",
                                        openai_req.model, tag
                                    ),
                                )
                                .await;
                            tokio::time::sleep(Duration::from_millis(3000)).await;
                            continue; // attempt 0 → 1, no token refresh
                        }
                        // Known failure (or attempt ≥ 1): fall through to
                        // is_switchable_error → account / key failover.
                    } else if status.as_u16() == 401 && !token_refresh_attempted {
                        if let Some(cred) = current_credential.as_ref() {
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
                    }

                    if is_switchable_error(status.as_u16(), &body) {
                        if let Some(cred) = current_credential.as_ref() {
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
                                    // Park the failing credential while the
                                    // session moves to a replacement. A daily
                                    // quota-exhausted account (WorkBuddy business
                                    // codes 14018/11105/11106) is sidelined until
                                    // the next local midnight — its quota window
                                    // resets then, not in 60 s — so the pool
                                    // round-robins to the next healthy account
                                    // instead of re-picking the exhausted one
                                    // every minute. Any other switchable error
                                    // keeps the ordinary 60 s cooldown.
                                    let cooldown = if is_quota_business_code(&body) {
                                        crate::util::ms_until_local_midnight(60_000)
                                    } else {
                                        60_000
                                    };
                                    pool.mark_limited(&from, cooldown).await;
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
                                        t.set_model(
                                            openai_req.model.clone(),
                                            format!("failover {status} from {from}"),
                                        );
                                    }
                                    active_credential = Some(next.clone());
                                    // Remember it: the next request must not
                                    // pay for this discovery again.
                                    remember_failover(
                                        &from,
                                        FailoverTarget::Credential(next.id.clone()),
                                    );
                                    continue 'credential; // retry the request on the replacement
                                }
                                None => {
                                    // No healthy replacement account available in the pool.
                                    // Apply short 5s cooldown so the pool recovers quickly.
                                    pool.mark_limited(&from, 5_000).await;

                                    // Fallback: check if API key pool has available keys for failover
                                    if config.session_switch_enabled {
                                        let available_keys =
                                            crate::workbuddy_auth::enabled_api_keys();
                                        if let Some(key) = available_keys
                                            .into_iter()
                                            .find(|k| !tried_key_ids.contains(&k.id))
                                        {
                                            tried_key_ids.insert(key.id.clone());
                                            gui_logs
                                                .push(
                                                    "WARN",
                                                    format!(
                                                        "账号池无可用替换账号 from={} reason={}，自动降级至密钥池: key_id={} label={} {} session={}",
                                                        from, status, key.id, key.label, tag, session.session_id
                                                    ),
                                                )
                                                .await;
                                            if let Ok(mut t) = overrides.lock() {
                                                t.set_key(
                                                    key.id.clone(),
                                                    format!("{status} from {from} (降级密钥池)"),
                                                );
                                                t.set_model(
                                                    openai_req.model.clone(),
                                                    format!("failover {status} from {from}"),
                                                );
                                            }
                                            active_credential = None;
                                            current_api_key = Some(key.key.clone());
                                            current_key_id = Some(key.id.clone());
                                            // The account pool was exhausted, so
                                            // the key pool is the only thing that
                                            // worked — remember exactly that.
                                            remember_failover(
                                                &from,
                                                FailoverTarget::ApiKey(key.id.clone()),
                                            );
                                            continue 'credential; // retry the request on the fallback API key
                                        }
                                    }

                                    gui_logs
                                        .push(
                                            "WARN",
                                            format!(
                                                "凭据池与密钥池均无可用替换身份 from={} reason={} {} (短暂冷却5s后恢复)",
                                                from, status, tag
                                            ),
                                        )
                                        .await;
                                }
                            }
                        } else if config.session_switch_enabled {
                            // Currently using API key (current_credential is None)
                            let from = current_key_id
                                .clone()
                                .unwrap_or_else(|| "api-key".to_string());
                            tried_key_ids.insert(from.clone());
                            let available_keys = crate::workbuddy_auth::enabled_api_keys();
                            if let Some(key) = available_keys
                                .into_iter()
                                .find(|k| !tried_key_ids.contains(&k.id))
                            {
                                tried_key_ids.insert(key.id.clone());
                                gui_logs
                                    .push(
                                        "WARN",
                                        format!(
                                            "密钥失败 from={} reason={}，自动轮询切换至下一密钥: key_id={} label={} {} session={}",
                                            from, status, key.id, key.label, tag, session.session_id
                                        ),
                                    )
                                    .await;
                                if let Ok(mut t) = overrides.lock() {
                                    t.set_key(key.id.clone(), format!("{status} from {from}"));
                                    t.set_model(
                                        openai_req.model.clone(),
                                        format!("failover {status} from {from}"),
                                    );
                                }
                                active_credential = None;
                                current_api_key = Some(key.key.clone());
                                current_key_id = Some(key.id.clone());
                                continue 'credential;
                            } else if config.credential_pool_enabled && pool.is_enabled().await {
                                if let Some(next) = pool.pick(&session.session_id).await {
                                    gui_logs
                                        .push(
                                            "WARN",
                                            format!(
                                                "密钥池已耗尽 from={} reason={}，自动切换至账号池: cred_id={} {} session={}",
                                                from, status, next.id, tag, session.session_id
                                            ),
                                        )
                                        .await;
                                    if let Ok(mut t) = overrides.lock() {
                                        t.set_key(
                                            next.id.clone(),
                                            format!("{status} from {from} (切换账号池)"),
                                        );
                                        t.set_model(
                                            openai_req.model.clone(),
                                            format!("failover {status} from {from}"),
                                        );
                                    }
                                    active_credential = Some(next);
                                    continue 'credential;
                                }
                            }

                            gui_logs
                                .push(
                                    "WARN",
                                    format!(
                                        "密钥池无可用替换密钥 from={} reason={} {}",
                                        from, status, tag
                                    ),
                                )
                                .await;
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
                            &current_api_key,
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

                // The identity serving this request succeeded, so it is
                // healthy. If it was the one parked in failover memory, the
                // half-open probe just passed: forget the memory so later
                // requests return to the default-first path instead of staying
                // pinned to the override.
                if let Some(cred) = current_credential.as_ref() {
                    if failover_is_known(&cred.id) {
                        clear_failover();
                        tracing::info!(
                            "credential {} served a request successfully; leaving failover memory",
                            cred.id
                        );
                    }
                }

                return if streaming {
                    streaming_response(
                        response,
                        flavor,
                        client_model,
                        route,
                        start,
                        upstream_start,
                        gui_logs,
                        stats,
                        session,
                        tag,
                        Arc::clone(&overrides),
                    )
                } else {
                    // The client wants one JSON body. If the request was upgraded
                    // to a stream upstream, aggregate it first — that path sees
                    // every chunk, so it is also the one that can report TTFT and
                    // the model's generation span. A plain JSON response exposes
                    // neither, so its timings stay at zero ("unknown").
                    //
                    // Reading the body happens *here* rather than through `?`
                    // because it can fail mid-transfer: reqwest reports a
                    // truncated body as `Decode`/“error decoding response body”,
                    // which the `?` turned into a 502/500 the client could not
                    // act on even though the next attempt normally succeeds.
                    let materialized = if upstream_streaming {
                        collect_stream_into_response(response, upstream_start).await
                    } else {
                        response
                            .json::<openai::OpenAIResponse>()
                            .await
                            .map(|resp| (resp, UpstreamTiming::default()))
                            .map_err(ProxyError::Http)
                    };

                    let (resp, timing) = match materialized {
                        Ok(pair) => pair,
                        Err(err) => {
                            // A body cut short mid-transfer is the upstream or
                            // the network, not this request — retry on the next
                            // URL (and, if none is left, the next credential)
                            // instead of failing the client outright. A body
                            // that arrived whole but could not be deserialized
                            // will fail identically everywhere, so it is
                            // reported rather than retried.
                            if !err.is_transient_transport_error() {
                                return Err(err);
                            }
                            let message = err.diagnostic();
                            metrics::upstream_error("chat_completions");
                            gui_logs
                                .push(
                                    "WARN",
                                    format!(
                                        "UPSTREAM BODY FAILED MID-TRANSFER url={} model={} {}ms {} | {} (retrying next upstream)",
                                        url,
                                        openai_req.model,
                                        upstream_start.elapsed().as_millis(),
                                        tag,
                                        message
                                    ),
                                )
                                .await;
                            tracing::warn!("Upstream body failed mid-transfer: {}", message);
                            last_err = Some(err);
                            continue 'url; // try the next upstream URL
                        }
                    };

                    non_streaming_response(
                        resp,
                        flavor,
                        &openai_req.model,
                        client_model,
                        route,
                        start,
                        timing,
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
    upstream_start: Instant,
) -> ProxyResult<(openai::OpenAIResponse, UpstreamTiming)> {
    use futures::StreamExt;

    let mut accumulated = Aggregate::default();
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    // The same clock the streaming path uses, so a client that asked for JSON
    // still gets real TTFT/TPS numbers when the upstream served a stream.
    let mut timing = TimingTracker::default();

    while let Some(chunk) = stream.next().await {
        let bytes = chunk.map_err(ProxyError::Http)?;
        buffer.push_str(&String::from_utf8_lossy(&bytes));

        // Same guard as the streaming path: an upstream that never emits a
        // frame separator must not grow this buffer for the whole request.
        if buffer.len() > MAX_SSE_FRAME_BYTES {
            return Err(ProxyError::Upstream(format!(
                "upstream sent an unterminated SSE frame ({} bytes without a blank line)",
                buffer.len()
            )));
        }

        while let Some(pos) = buffer.find("\n\n") {
            let frame = buffer[..pos].to_string();
            buffer.drain(..pos + 2);
            for line in frame.lines() {
                let Some(data) = line.strip_prefix("data: ") else {
                    continue;
                };
                if data.trim() == "[DONE]" {
                    continue;
                }
                if let Ok(chunk_obj) = serde_json::from_str::<openai::StreamChunk>(data) {
                    timing.observe(upstream_start, &chunk_obj);
                    accumulated.absorb(&chunk_obj);
                }
            }
        }
    }

    Ok((accumulated.into_response(), timing.finish()))
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

        let reasoning_content = if self.reasoning.is_empty() {
            None
        } else {
            Some(self.reasoning)
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
                    reasoning_content,
                    refusal: None,
                    tool_calls,
                },
                logprobs: None,
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
    // Upstream TTFT / model span / tool-turn facts. All zero when the response
    // was a plain JSON body, which exposes no timing at all.
    timing: UpstreamTiming,
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
    record_stats_row(
        &stats,
        RequestOutcome {
            model: &client_model,
            route,
            tokens: &tokens,
            duration_ms,
            timing,
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
        },
    );

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
            if openai_resp.object.is_none() {
                openai_resp.object = Some("chat.completion".to_string());
            }
            if openai_resp.id.is_none() {
                openai_resp.id = Some(crate::translate::responses::generate_id("chatcmpl"));
            }
            if openai_resp.created.is_none() {
                openai_resp.created = Some(crate::translate::responses::current_timestamp() as u64);
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
    // When the winning upstream attempt was *sent*. Timings are measured from
    // here, not from `start`, so credential selection, retries and body
    // translation are not misattributed to the model.
    upstream_start: Instant,
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
        upstream_start,
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

    let mut data: Vec<_> = models
        .into_iter()
        .map(|m| openai::ModelInfo {
            id: m.id,
            object: Some("model".to_string()),
            created: None,
            owned_by: None,
        })
        .collect();

    // Advertise the virtual `free` model so clients can pin to it. It is not a
    // real upstream id (the gateway resolves it per request), so expose it only
    // once and only when the catalog would not already contain it.
    if !data.iter().any(|m| m.id == pipeline::FREE_MODEL_NAME) {
        data.push(openai::ModelInfo {
            id: pipeline::FREE_MODEL_NAME.to_string(),
            object: Some("model".to_string()),
            created: None,
            owned_by: Some("proxy-free".to_string()),
        });
    }

    let openai_resp = openai::ModelsListResponse {
        object: Some("list".to_string()),
        data,
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
