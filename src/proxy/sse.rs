//! Streaming plumbing: SSE framing, the per-request accounting ledger, and the
//! TTFT / output-speed tracker.
//!
//! Split out of `proxy.rs`. Everything here is about turning an upstream byte stream
//! into a client stream while recording one accurate request-log row.

use super::*;
use crate::error::{describe_error_chain, is_transport_body_error};

/// Serialize one SSE event in the `event:`/`data:` framing both APIs use.
pub(crate) fn serialize_sse_event<T: serde::Serialize>(event_type: &str, event: &T) -> String {
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
pub(crate) struct StreamLedger {
    model: String,
    route: &'static str,
    start: Instant,
    /// When the winning upstream attempt was sent — the anchor for TTFT and the
    /// model's generation span.
    upstream_start: Instant,
    stats: Arc<StatsDb>,
    /// Session identity for the row this ledger writes on drop.
    session: SessionInfo,
    tokens: TokenRecord,
    /// TTFT / model span / tool-turn facts, folded from parsed chunks.
    timing: TimingTracker,
    /// Set when the upstream stream failed; turns the row's status into a 502.
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
        upstream_start: Instant,
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
            upstream_start,
            stats,
            session,
            tokens: TokenRecord::default(),
            timing: TimingTracker::default(),
            error: None,
            override_key,
            override_model,
            override_reason,
        }
    }

    /// Fold one parsed upstream chunk into the timing state.
    fn observe_chunk(&mut self, chunk: &openai::StreamChunk) {
        self.timing.observe(self.upstream_start, chunk);
    }
}

impl Drop for StreamLedger {
    fn drop(&mut self) {
        // A failed stream is an **upstream** fault, so the row records the same
        // 502 `ProxyError::Http`/`ProxyError::Upstream` map to (see
        // `ProxyError::status`). It used to record 500, which said "this server
        // broke" about a truncation or reset that happened upstream — and since
        // the client had already received HTTP 200 plus an error frame, the 500
        // was never the status of a real response either. That left the GUI's
        // error filter showing red 5xx rows whose cause the row itself named as
        // upstream, which is exactly what sent the 2026-10-07 investigation to
        // the wrong place.
        let status = if self.error.is_some() { 502 } else { 200 };
        record_stats_row(
            &self.stats,
            RequestOutcome {
                model: &self.model,
                route: self.route,
                tokens: &self.tokens,
                duration_ms: self.start.elapsed().as_millis() as i64,
                timing: self.timing.finish(),
                streamed: true,
                status,
                error: self.error.as_deref(),
                session_id: &self.session.session_id,
                client: self.session.client.tag(),
                override_key: &self.override_key,
                override_model: &self.override_model,
                override_reason: &self.override_reason,
            },
        );
    }
}

/// Accumulates TTFT / model-span / tool-turn facts from parsed upstream chunks.
///
/// Shared by the two paths that can actually see the upstream's chunks: the
/// streaming SSE framer and the stream-aggregating non-streaming path. Keeping
/// one implementation is what stops the two from disagreeing about what "first
/// token" means.
#[derive(Default)]
pub(crate) struct TimingTracker {
    /// Milliseconds to the first output delta. `None` = never observed.
    pub(crate) ttft_ms: Option<i64>,
    /// Milliseconds to the most recent output delta: the model's generation span.
    last_delta_ms: Option<i64>,
    /// The last non-empty `finish_reason` seen.
    finish_reason: Option<String>,
}

impl TimingTracker {
    /// Fold one parsed upstream chunk into the timing state.
    ///
    /// "Output" means a content delta **or** a tool-call delta: a turn that
    /// opens with a tool call has produced its first output token just as much
    /// as one that opens with text, and excluding it would report a tool-only
    /// turn as having no TTFT at all.
    ///
    /// Reasoning deltas are deliberately excluded from TTFT. The Anthropic
    /// translator never surfaces upstream reasoning to the client (see
    /// `translate::stream`), so counting it would report a first token the user
    /// did not receive.
    pub(crate) fn observe(&mut self, upstream_start: Instant, chunk: &openai::StreamChunk) {
        let Some(choice) = chunk.choices.first() else {
            return;
        };

        let has_text = choice
            .delta
            .content
            .as_deref()
            .is_some_and(|c| !c.is_empty());
        let has_tool = choice
            .delta
            .tool_calls
            .as_ref()
            .is_some_and(|calls| !calls.is_empty());

        if has_text || has_tool {
            let elapsed = upstream_start.elapsed().as_millis() as i64;
            if self.ttft_ms.is_none() {
                self.ttft_ms = Some(elapsed);
            }
            // Monotonic: a later frame must never shrink the span.
            self.last_delta_ms = Some(match self.last_delta_ms {
                Some(prev) => prev.max(elapsed),
                None => elapsed,
            });
        }

        if let Some(reason) = choice.finish_reason.as_ref() {
            if !reason.is_empty() {
                self.finish_reason = Some(reason.clone());
            }
        }
    }

    /// Freeze into the row's timing columns.
    pub(crate) fn finish(&self) -> UpstreamTiming {
        UpstreamTiming {
            ttft_ms: self.ttft_ms.unwrap_or(0),
            model_ms: self.last_delta_ms.unwrap_or(0),
            ended_with_tool_call: matches!(
                self.finish_reason.as_deref(),
                Some("tool_calls") | Some("function_call")
            ),
        }
    }
}

/// Cap on how much of an unterminated SSE frame the reassembly buffer keeps.
/// A well-formed event is one `data:` line, so this is generous; it exists only
/// so an upstream that never emits a frame separator cannot grow the buffer for
/// the whole life of a stream (see `create_flavor_sse_stream`).
pub(crate) const MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;

/// Describe a mid-stream upstream read failure for the log and the request row.
///
/// A reqwest body failure prints only as `error decoding response body`, which
/// says nothing about what happened; the cause chain carries the diagnosis
/// (`Connection reset by peer`, `end of file before message length reached`,
/// `operation timed out`). For transport failures the wording names the shape —
/// a stalled upstream versus a truncated body — because those point at
/// different causes and the distinction is the actionable part.
///
/// Shares [`ProxyError::diagnostic`]'s wording (via the same
/// `describe_transport_failure`) with the non-streaming path, so one failure
/// cannot be described two ways depending on which path met it.
pub(crate) fn upstream_read_error(error: &(dyn std::error::Error + 'static)) -> String {
    match error.downcast_ref::<reqwest::Error>() {
        Some(reqwest) if is_transport_body_error(reqwest) => {
            crate::error::describe_transport_failure(reqwest)
        }
        _ => describe_error_chain(error),
    }
}

/// Shared SSE framer for all three API flavors.
///
/// All three read the upstream's OpenAI-style `data: {...}` stream and differ
/// only in how each chunk is translated and re-serialized, so the buffering,
/// `[DONE]` handling, business-error detection and stats capture live here once.
#[allow(clippy::too_many_arguments)]
pub(super) fn create_flavor_sse_stream(
    upstream: impl Stream<Item = Result<Bytes, impl std::error::Error + Send + Sync + 'static>>
        + Send
        + 'static,
    flavor: ApiFlavor,
    client_model: String,
    route: &'static str,
    start: Instant,
    // When the upstream request was sent; anchors TTFT and the model span.
    upstream_start: Instant,
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
            upstream_start,
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

                    // The buffer only ever holds the tail of an unterminated
                    // frame. An upstream that never sends a blank line — a
                    // stalled or hostile one — would grow it without limit
                    // while the stream stays open, which is reachable from any
                    // client that opts into streaming. Past the cap the frame
                    // cannot be a legitimate SSE event, so stop and surface the
                    // error instead of accumulating.
                    if buffer.len() > MAX_SSE_FRAME_BYTES {
                        let summary = format!(
                            "upstream sent an unterminated SSE frame ({} bytes without a blank line)",
                            buffer.len()
                        );
                        let msg = format!(
                            "UPSTREAM ERROR [stream] model={} {} | {}",
                            client_model, tag, summary
                        );
                        gui_logs.push("ERROR", msg.clone()).await;
                        tracing::warn!("{}", msg);
                        ledger.error = Some(summary.clone());

                        // Close the stream the same way a read error does, so
                        // the client is told instead of just seeing the
                        // connection end after a long silence.
                        match flavor {
                            ApiFlavor::Anthropic => {
                                for event in stream::translate_error(format!("Upstream error: {}", summary)) {
                                    yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                }
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
                            }
                            ApiFlavor::Chat => {
                                // Passthrough: an `error` event keeps the
                                // framing but is not part of Chat's schema, so
                                // the log line above is the report.
                            }
                        }
                        break;
                    }

                    while let Some(pos) = buffer.find("\n\n") {
                        let line = buffer[..pos].to_string();
                        buffer.drain(..pos + 2);

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

                                    // Upstream timings, measured from the send.
                                    // Every parsed chunk that carries output is
                                    // an opportunity to advance the model's span;
                                    // the first one also fixes TTFT. `max` keeps
                                    // this monotonic across interleaved frames.
                                    ledger.observe_chunk(&chunk_obj);

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
                                            if chunk_obj.object.is_none() {
                                                chunk_obj.object = Some("chat.completion.chunk".to_string());
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
                    // The recorded reason must name the real failure. reqwest
                    // prints every body failure as "error decoding response
                    // body" (the phrase on the 2026-10-07 500 row), so a
                    // truncated stream reached the user with nothing to act on.
                    // `upstream_read_error` keeps the cause chain and, for a
                    // transport failure, says whether the upstream stalled or
                    // the body was cut short.
                    let summary = upstream_read_error(&e);
                    ledger.error = Some(summary.clone());
                    match flavor {
                        ApiFlavor::Anthropic => {
                            tracing::error!("Stream error: {}", summary);
                            for event in stream::translate_error(format!("Stream error: {}", summary)) {
                                yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                            }
                        }
                        ApiFlavor::Responses => {
                            tracing::error!("Stream error: {}", summary);
                            if let Some(state) = responses_state.as_mut() {
                                for event in responses_pipeline::translate_stream_error(
                                    state,
                                    format!("Stream error: {}", summary),
                                ) {
                                    yield Ok(Bytes::from(serialize_sse_event(event.event_type(), &event)));
                                }
                            }
                        }
                        ApiFlavor::Chat => {
                            let msg =
                                format!("STREAM READ ERROR model={} {} | {}", client_model, tag, summary);
                            gui_logs.push("ERROR", msg.clone()).await;
                            tracing::warn!("{}", msg);
                            // Chat Completions has no dedicated stream-error event,
                            // but its schema carries failures as a `data:` frame
                            // with an `error` object — which is exactly what the
                            // openai SDKs look for (`data.error` throws). Emitting
                            // it turns a truncated stream from a silently
                            // half-finished answer into an error the client can
                            // surface (and retry), instead of an HTTP 200 whose
                            // body merely stops.
                            yield Ok(Bytes::from(format!(
                                "data: {}\n\n",
                                serde_json::json!({
                                    "error": {
                                        "type": "upstream_error",
                                        "message": summary,
                                    }
                                })
                            )));
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
pub(crate) fn create_sse_stream(
    upstream: impl Stream<Item = Result<Bytes, impl std::error::Error + Send + Sync + 'static>>
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
        Instant::now(),
        gui_logs,
        stats,
        SessionInfo::unknown(),
        "client=unknown".to_string(),
        Default::default(),
    )
}

#[cfg(test)]
pub(crate) fn create_responses_sse_stream(
    upstream: impl Stream<Item = Result<Bytes, impl std::error::Error + Send + Sync + 'static>>
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
        Instant::now(),
        gui_logs,
        stats,
        SessionInfo::unknown(),
        "client=unknown".to_string(),
        Default::default(),
    )
}
