//! Buffering, aggregation and streaming passthrough.

use super::*;

/// Accumulates one streaming upstream response into a single
/// `OpenAIResponse`, for clients that asked for a non-streaming reply.
///
/// Needed because some providers (WorkBuddy) only accept `stream:true` and
/// reject a plain body with `11101`. Merging is done on the same chunk shape
/// `create_flavor_sse_stream` consumes, so both paths agree on how content,
/// tool calls and usage are read.
pub(crate) async fn collect_stream_into_response(
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
pub(crate) async fn non_streaming_response(
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
    // Label the Prometheus token metric with the served upstream model for
    // every flavor (previously Chat reported the client model, which left a
    // `free` request counted under `free` instead of the resolved node).
    metrics::tokens(
        openai_resp.usage.prompt_tokens,
        openai_resp.usage.completion_tokens,
        upstream_model,
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
            model: upstream_model,
            client_model: &client_model,
            route,
            tokens: &tokens,
            duration_ms,
            timing,
            streamed: false,
            status: 200,
            error: None,
            session_id: &session.session_id,
            parent_session_id: &session.parent_session_id,
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
pub(crate) fn streaming_response(
    response: reqwest::Response,
    flavor: ApiFlavor,
    upstream_model: &str,
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
        upstream_model.to_string(),
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
