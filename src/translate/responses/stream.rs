//! The streaming state machine: OpenAI chunks → Responses SSE events.

use super::*;

/// Cap on concurrently streamed tool calls tracked by one response.
///
/// Purely a resource guard. The upstream frame supplies the tool-call `index`,
/// and the translator grows `active_tool_calls` until it covers that index — so
/// an unvalidated index is an allocation primitive. Real traffic uses a
/// handful; 256 is generous while keeping one malicious frame from exhausting
/// memory (the release profile is `panic = "abort"`, so an OOM here would take
/// the whole desktop app down, not just this request).
const MAX_STREAMING_TOOL_CALLS: usize = 256;

/// State tracker for translating an upstream OpenAI Chat SSE stream to Responses API SSE events.
#[derive(Debug)]
pub struct ResponsesStreamState {
    pub response_id: String,
    pub model: Option<String>,
    pub fallback_model: String,
    pub created_at: i64,
    created_emitted: bool,
    text_item_started: bool,
    text_item_id: String,
    /// Output index assigned when the text item was opened. Events for the
    /// text item must echo it: a model that streams a tool call *before* text
    /// gets a text index above 0, and a hardcoded 0 corrupts the client's
    /// item accounting (Codex drops or mis-associates the item).
    text_item_output_index: usize,
    accumulated_text: String,
    pub(crate) active_tool_calls: Vec<StreamingToolCall>,
    output_index_counter: usize,
    items_closed: bool,
    finalized: bool,
    pending_usage: Option<openai::Usage>,
}

#[derive(Debug, Clone)]
pub(crate) struct StreamingToolCall {
    id: String,
    pub(crate) call_id: String,
    name: String,
    arguments: String,
    output_index: usize,
    pub(crate) started: bool,
    pub(crate) done: bool,
}

pub fn initial_stream_state(fallback_model: String) -> ResponsesStreamState {
    ResponsesStreamState {
        response_id: generate_id("resp"),
        model: None,
        fallback_model,
        created_at: current_timestamp(),
        created_emitted: false,
        text_item_started: false,
        text_item_id: generate_id("msg"),
        text_item_output_index: 0,
        accumulated_text: String::new(),
        active_tool_calls: Vec::new(),
        output_index_counter: 0,
        items_closed: false,
        finalized: false,
        pending_usage: None,
    }
}

impl ResponsesStreamState {
    pub fn model(&self) -> &str {
        self.model
            .as_deref()
            .unwrap_or(self.fallback_model.as_str())
    }

    /// A compact summary of what this streamed turn actually produced, used to
    /// flag stalls. Codex reads the Responses stream for *actions*; if a turn
    /// closes with text but no tool call, the client has nothing to execute and
    /// simply stops — exactly the "said it would do X, then froze" symptom. The
    /// raw `active_tool_calls` count is reported alongside the *started* count so
    /// a model that emitted a bare `tool_calls` delta without an id/name (a known
    /// non-OpenAI-upstream shape) is distinguishable from one that emitted none.
    pub fn summary(&self) -> ResponsesTurnSummary {
        let active_tool_calls = self.active_tool_calls.len();
        let started_tool_calls = self.active_tool_calls.iter().filter(|t| t.started).count();
        ResponsesTurnSummary {
            text_len: self.accumulated_text.chars().count(),
            active_tool_calls,
            started_tool_calls,
        }
    }
}

/// What an upstream streamed turn resolved into, for stall diagnostics.
#[derive(Debug, Clone, Copy)]
pub struct ResponsesTurnSummary {
    /// Characters of assistant text accumulated this turn.
    pub text_len: usize,
    /// Tool-call slots the upstream opened (may exceed `started_tool_calls` when
    /// a call arrived without an id/name and was therefore never emitted).
    pub active_tool_calls: usize,
    /// Tool calls that actually became `function_call` items Codex can run.
    pub started_tool_calls: usize,
}

/// Translates a single incoming OpenAI `StreamChunk` into a series of `ResponsesStreamEvent`s.
pub fn translate_stream_chunk(
    state: &mut ResponsesStreamState,
    chunk: &openai::StreamChunk,
) -> Vec<responses::ResponsesStreamEvent> {
    let mut events = Vec::new();

    if let Some(id) = &chunk.id {
        if state.response_id.starts_with("resp_") && !id.is_empty() {
            // Can retain or track original id if helpful
        }
    }
    if let Some(m) = &chunk.model {
        if state.model.is_none() {
            state.model = Some(m.clone());
        }
    }
    if chunk.usage.is_some() {
        state.pending_usage = chunk.usage.clone();
    }

    // 1. Emit `response.created` on the first chunk
    if !state.created_emitted {
        state.created_emitted = true;
        events.push(responses::ResponsesStreamEvent::Created {
            response: responses::ResponseStreamMetadata {
                id: state.response_id.clone(),
                object: "response".to_string(),
                status: "in_progress".to_string(),
                model: state.model().to_string(),
                output: Vec::new(),
                usage: None,
            },
        });
    }

    let Some(choice) = chunk.choices.first() else {
        // If choices is empty (e.g. trailing usage chunk in OpenAI stream)
        // and stream items have already been closed, emit Completed event now.
        if state.items_closed && !state.finalized && state.pending_usage.is_some() {
            events.extend(emit_completed(state));
        }
        return events;
    };

    // 2. Handle text content delta
    if let Some(ref content) = choice.delta.content {
        if !content.is_empty() {
            if !state.text_item_started {
                state.text_item_started = true;
                state.text_item_output_index = state.output_index_counter;
                let output_index = state.output_index_counter;
                state.output_index_counter += 1;

                events.push(responses::ResponsesStreamEvent::OutputItemAdded {
                    response_id: state.response_id.clone(),
                    output_index,
                    item: responses::OutputItem::Message {
                        id: state.text_item_id.clone(),
                        status: "in_progress".to_string(),
                        role: "assistant".to_string(),
                        content: Vec::new(),
                    },
                });

                events.push(responses::ResponsesStreamEvent::ContentPartAdded {
                    response_id: state.response_id.clone(),
                    item_id: state.text_item_id.clone(),
                    output_index,
                    content_index: 0,
                    part: responses::OutputContentPart::OutputText {
                        text: String::new(),
                    },
                });
            }

            state.accumulated_text.push_str(content);

            events.push(responses::ResponsesStreamEvent::OutputTextDelta {
                response_id: state.response_id.clone(),
                item_id: state.text_item_id.clone(),
                output_index: state.text_item_output_index,
                content_index: 0,
                delta: content.clone(),
            });
        }
    }

    // 3. Handle tool calls delta
    if let Some(ref tool_calls) = choice.delta.tool_calls {
        for call in tool_calls {
            let index = call.index;
            // `index` comes straight off the upstream frame, so it is not
            // trusted: it is only ever used to *extend* `active_tool_calls`,
            // and `while len <= index { push }` with a hostile value (e.g.
            // `usize::MAX`) allocates until the process dies — fatal here,
            // because the release profile is `panic = "abort"`.
            //
            // A real response carries one tool call per distinct index and
            // never needs thousands, so anything past the cap is a malformed
            // or hostile frame: skip it rather than growing the vector.
            if index >= MAX_STREAMING_TOOL_CALLS {
                tracing::warn!(
                    "ignoring tool call with out-of-range index {} (cap {})",
                    index,
                    MAX_STREAMING_TOOL_CALLS
                );
                continue;
            }
            while state.active_tool_calls.len() <= index {
                let output_index = state.output_index_counter;
                state.output_index_counter += 1;
                state.active_tool_calls.push(StreamingToolCall {
                    id: generate_id("call"),
                    call_id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    output_index,
                    started: false,
                    done: false,
                });
            }

            let tool_entry = &mut state.active_tool_calls[index];
            if let Some(ref id) = call.id {
                tool_entry.call_id = id.clone();
                tool_entry.id = id.clone();
            }
            if let Some(ref func) = call.function {
                if let Some(ref name) = func.name {
                    tool_entry.name.push_str(name);
                }
            }

            // Only start the tool call item when we have both call_id and a non-empty name,
            // avoiding empty tool name events on initial ID-only chunks.
            if !tool_entry.started && !tool_entry.name.is_empty() && !tool_entry.call_id.is_empty()
            {
                tool_entry.started = true;
                events.push(responses::ResponsesStreamEvent::OutputItemAdded {
                    response_id: state.response_id.clone(),
                    output_index: tool_entry.output_index,
                    item: responses::OutputItem::FunctionCall {
                        id: tool_entry.id.clone(),
                        call_id: tool_entry.call_id.clone(),
                        status: "in_progress".to_string(),
                        name: tool_entry.name.clone(),
                        arguments: String::new(),
                    },
                });
            }

            if let Some(ref func) = call.function {
                if let Some(ref args) = func.arguments {
                    if !args.is_empty() {
                        tool_entry.arguments.push_str(args);
                        events.push(
                            responses::ResponsesStreamEvent::FunctionCallArgumentsDelta {
                                response_id: state.response_id.clone(),
                                item_id: tool_entry.id.clone(),
                                output_index: tool_entry.output_index,
                                call_id: tool_entry.call_id.clone(),
                                delta: args.clone(),
                            },
                        );
                    }
                }
            }
        }
    }

    // 4. Handle finish_reason.
    //
    // Only finalize on a *meaningful* reason: some upstreams send
    // `"finish_reason": ""` on every chunk, and treating that as the end
    // closes (and empties) the response before any text arrives — the client
    // then renders nothing. Mirrors `stream::translate_chunk`.
    if let Some(finish_reason) = &choice.finish_reason {
        if !finish_reason.is_empty() {
            if !state.items_closed {
                events.extend(close_active_items(state));
            }
            if state.pending_usage.is_some() && !state.finalized {
                events.extend(emit_completed(state));
            }
        }
    }

    events
}

fn close_active_items(state: &mut ResponsesStreamState) -> Vec<responses::ResponsesStreamEvent> {
    let mut events = Vec::new();
    if state.items_closed {
        return events;
    }
    state.items_closed = true;

    // Close text item if active
    if state.text_item_started {
        events.push(responses::ResponsesStreamEvent::OutputTextDone {
            response_id: state.response_id.clone(),
            item_id: state.text_item_id.clone(),
            output_index: state.text_item_output_index,
            content_index: 0,
            text: state.accumulated_text.clone(),
        });

        events.push(responses::ResponsesStreamEvent::OutputItemDone {
            response_id: state.response_id.clone(),
            output_index: state.text_item_output_index,
            item: responses::OutputItem::Message {
                id: state.text_item_id.clone(),
                status: "completed".to_string(),
                role: "assistant".to_string(),
                content: vec![responses::OutputContentPart::OutputText {
                    text: state.accumulated_text.clone(),
                }],
            },
        });
    }

    // Close tool calls if active
    for tool_entry in &mut state.active_tool_calls {
        if tool_entry.started && !tool_entry.done {
            tool_entry.done = true;
            events.push(responses::ResponsesStreamEvent::FunctionCallArgumentsDone {
                response_id: state.response_id.clone(),
                item_id: tool_entry.id.clone(),
                output_index: tool_entry.output_index,
                call_id: tool_entry.call_id.clone(),
                arguments: tool_entry.arguments.clone(),
            });

            events.push(responses::ResponsesStreamEvent::OutputItemDone {
                response_id: state.response_id.clone(),
                output_index: tool_entry.output_index,
                item: responses::OutputItem::FunctionCall {
                    id: tool_entry.id.clone(),
                    call_id: tool_entry.call_id.clone(),
                    status: "completed".to_string(),
                    name: tool_entry.name.clone(),
                    arguments: tool_entry.arguments.clone(),
                },
            });
        }
    }

    events
}

fn emit_completed(state: &mut ResponsesStreamState) -> Vec<responses::ResponsesStreamEvent> {
    let mut events = Vec::new();
    if state.finalized {
        return events;
    }
    state.finalized = true;

    // Assemble completed output items
    let mut output = Vec::new();
    let mut response_content_items = Vec::new();

    if state.text_item_started {
        output.push(responses::OutputItem::Message {
            id: state.text_item_id.clone(),
            status: "completed".to_string(),
            role: "assistant".to_string(),
            content: vec![responses::OutputContentPart::OutputText {
                text: state.accumulated_text.clone(),
            }],
        });
        response_content_items.push(responses::ResponseContentItem {
            content_type: "output_text".to_string(),
            text: Some(state.accumulated_text.clone()),
        });
    }

    for tool_entry in &state.active_tool_calls {
        if tool_entry.started {
            output.push(responses::OutputItem::FunctionCall {
                id: tool_entry.id.clone(),
                call_id: tool_entry.call_id.clone(),
                status: "completed".to_string(),
                name: tool_entry.name.clone(),
                arguments: tool_entry.arguments.clone(),
            });
        }
    }

    let response_output = if !response_content_items.is_empty() {
        Some(responses::ResponseOutput {
            role: Some("assistant".to_string()),
            content: Some(response_content_items),
        })
    } else {
        None
    };

    let usage = state
        .pending_usage
        .as_ref()
        .map(|u| responses::ResponsesUsage {
            total_tokens: u.total_tokens,
            input_tokens: u.prompt_tokens,
            output_tokens: u.completion_tokens,
            input_tokens_details: u.prompt_tokens_details.clone(),
        });

    events.push(responses::ResponsesStreamEvent::Completed {
        response: responses::ResponsesResponse {
            id: state.response_id.clone(),
            object: "response".to_string(),
            created_at: state.created_at,
            status: "completed".to_string(),
            model: state.model().to_string(),
            output,
            usage,
            response: response_output,
        },
    });

    events
}

/// Closes any unfinalized stream items and emits `response.completed` upon stream completion.
pub fn translate_stream_done(
    state: &mut ResponsesStreamState,
) -> Vec<responses::ResponsesStreamEvent> {
    let mut events = Vec::new();
    if !state.items_closed {
        events.extend(close_active_items(state));
    }
    if !state.finalized {
        events.extend(emit_completed(state));
    }
    events
}

/// Translates a stream error into `response.failed`.
///
/// Takes `&mut` and marks the state finalized: the failure event *is* the
/// terminal event for the turn. Without the flag, the end-of-stream path also
/// emits `response.completed` after `response.failed`, and a client that
/// honours both closes the turn with an empty completed response instead of
/// the error.
pub fn translate_stream_error(
    state: &mut ResponsesStreamState,
    error_message: String,
) -> Vec<responses::ResponsesStreamEvent> {
    state.finalized = true;
    vec![responses::ResponsesStreamEvent::Failed {
        response_id: state.response_id.clone(),
        error: responses::StreamErrorDetail {
            message: error_message,
            error_type: "server_error".to_string(),
            code: None,
        },
    }]
}
