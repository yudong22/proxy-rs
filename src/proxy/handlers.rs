//! The three axum protocol handlers.

use super::*;

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
    // Who called, when the inbound gate is on. Empty when the gate is off, so
    // the log format stays uniform rather than sometimes dropping the field.
    let account = crate::client_auth::account_label(&request);
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
    let account_note = account_log_field(&account);
    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/messages {} headers: {}{}",
                tag, incoming_headers, account_note
            ),
        )
        .await;
    tracing::info!(
        "POST /v1/messages {} headers: {}{}",
        tag,
        incoming_headers,
        account_note
    );

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
    // See `proxy_handler`: empty when the inbound gate is off.
    let account = crate::client_auth::account_label(&request);
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
    let account_note = account_log_field(&account);
    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/responses {} headers: {}{}",
                tag, incoming_headers, account_note
            ),
        )
        .await;
    tracing::info!(
        "POST /v1/responses {} headers: {}{}",
        tag,
        incoming_headers,
        account_note
    );

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
    // See `proxy_handler`: empty when the inbound gate is off.
    let account = crate::client_auth::account_label(&request);
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
    let account_note = account_log_field(&account);
    gui_logs
        .push(
            "INFO",
            format!(
                "POST /v1/chat/completions {} headers: {}{}",
                tag, incoming_headers, account_note
            ),
        )
        .await;
    tracing::info!(
        "POST /v1/chat/completions {} headers: {}{}",
        tag,
        incoming_headers,
        account_note
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
