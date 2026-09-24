use crate::error::{ProxyError, ProxyResult};
use crate::models::{openai, responses};
use crate::translate::core::normalize_schema;
use crate::translate::pipeline::{resolve_upstream_model, sanitize_prompt, TranslationPolicy};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

static ID_COUNTER: AtomicU64 = AtomicU64::new(1);

pub fn generate_id(prefix: &str) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    let count = ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("{}_{:x}{:x}", prefix, now, count)
}

pub fn current_timestamp() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

/// Translates an incoming OpenAI Responses API request into an upstream OpenAI Chat Completions request.
pub fn translate_responses_request(
    req: responses::ResponsesRequest,
    policy: &TranslationPolicy,
) -> ProxyResult<openai::OpenAIRequest> {
    // Shared precedence with the Anthropic route, so the same client model
    // cannot resolve differently depending on the endpoint it arrived at.
    // Responses has no `thinking` flag, so it resolves against
    // `completion_model` like the Chat path.
    let model = resolve_upstream_model(&req.model, policy.completion_model.as_ref(), policy);

    let mut messages = Vec::new();

    // 1. If instructions are present, add as system prompt
    if let Some(instructions) = req.instructions {
        let sanitized = sanitize_prompt(instructions, &policy.ignore_terms);
        let trimmed = sanitized.trim();
        if !trimmed.is_empty() {
            messages.push(openai::Message {
                role: "system".to_string(),
                content: Some(openai::MessageContent::Text(trimmed.to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
    }

    // 2. Translate input items into OpenAI messages
    match req.input {
        responses::ResponsesInput::Text(text) => {
            messages.push(openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text(text)),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            });
        }
        responses::ResponsesInput::Items(items) => {
            // Track which call ids this request actually declared, so a result
            // arriving for an unknown id can be reconciled instead of sent as
            // an orphan (strict upstreams reject the pairing).
            let mut known_call_ids: std::collections::BTreeSet<String> =
                std::collections::BTreeSet::new();
            for item in &items {
                if let responses::ResponseInputItem::FunctionCall { id, call_id, .. } = item {
                    if let Some(resolved) = call_id.clone().or_else(|| id.clone()) {
                        known_call_ids.insert(resolved);
                    }
                }
            }
            for item in items {
                match item {
                    responses::ResponseInputItem::Message { role, content, .. } => {
                        let msg_content = match content {
                            responses::ResponseMessageContent::Text(t) => {
                                openai::MessageContent::Text(t)
                            }
                            responses::ResponseMessageContent::Parts(parts) => {
                                let mut conv_parts = Vec::new();
                                for p in parts {
                                    if let Some(txt) = p.text {
                                        conv_parts.push(openai::ContentPart::Text { text: txt });
                                    } else if let Some(img) = p.image_url {
                                        let url = if let Some(s) = img.as_str() {
                                            s.to_string()
                                        } else if let Some(u) =
                                            img.get("url").and_then(|v| v.as_str())
                                        {
                                            u.to_string()
                                        } else {
                                            img.to_string()
                                        };
                                        conv_parts.push(openai::ContentPart::ImageUrl {
                                            image_url: openai::ImageUrl { url },
                                        });
                                    }
                                }
                                openai::MessageContent::Parts(conv_parts)
                            }
                        };
                        messages.push(openai::Message {
                            role,
                            content: Some(msg_content),
                            reasoning_content: None,
                            tool_calls: None,
                            tool_call_id: None,
                            name: None,
                        });
                    }
                    responses::ResponseInputItem::FunctionCall {
                        id,
                        call_id,
                        name,
                        arguments,
                        ..
                    } => {
                        let resolved_id = call_id.or(id).unwrap_or_else(|| generate_id("call"));
                        let call = openai::ToolCall {
                            id: resolved_id,
                            call_type: "function".to_string(),
                            function: openai::FunctionCall {
                                name: sanitize_tool_name(&name),
                                arguments,
                            },
                        };

                        // Parallel tool calls arrive as consecutive
                        // `function_call` items, but Chat Completions models one
                        // assistant turn as a single message carrying every
                        // call. Emitting one message per call leaves the first
                        // call unanswered at the point the next assistant
                        // message starts, and strict upstreams reject the whole
                        // request (`11148 tool calls and tool results do not
                        // match`), so append to the open turn instead.
                        if let Some(last) = messages.last_mut() {
                            if last.role == "assistant" && last.content.is_none() {
                                if let Some(calls) = last.tool_calls.as_mut() {
                                    calls.push(call);
                                    continue;
                                }
                            }
                        }

                        messages.push(openai::Message {
                            role: "assistant".to_string(),
                            content: None,
                            reasoning_content: None,
                            tool_calls: Some(vec![call]),
                            tool_call_id: None,
                            name: None,
                        });
                    }
                    responses::ResponseInputItem::FunctionCallOutput {
                        call_id, output, ..
                    } => {
                        let content_str = match output {
                            Value::String(s) => s,
                            Value::Null => String::new(),
                            other => other.to_string(),
                        };
                        // If no `function_call` declared this id, the upstream
                        // receives a tool result with nothing to answer — one
                        // of the shapes that yields `11148 tool calls and tool
                        // results do not match`. Emit a matching call once so
                        // the pair is well-formed: the synthetic call must carry
                        // the *same* id the result claims, or the mismatch
                        // remains.
                        if !known_call_ids.contains(&call_id) {
                            messages.push(openai::Message {
                                role: "assistant".to_string(),
                                content: None,
                                reasoning_content: None,
                                tool_calls: Some(vec![openai::ToolCall {
                                    id: call_id.clone(),
                                    call_type: "function".to_string(),
                                    function: openai::FunctionCall {
                                        name: "tool_result".to_string(),
                                        arguments: "{}".to_string(),
                                    },
                                }]),
                                tool_call_id: None,
                                name: None,
                            });
                        }
                        messages.push(openai::Message {
                            role: "tool".to_string(),
                            content: Some(openai::MessageContent::Text(content_str)),
                            reasoning_content: None,
                            tool_calls: None,
                            tool_call_id: Some(call_id),
                            name: None,
                        });
                    }
                    responses::ResponseInputItem::Raw(val) => {
                        if let Some(role) = val.get("role").and_then(|r| r.as_str()) {
                            let content_text = val
                                .get("content")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            messages.push(openai::Message {
                                role: role.to_string(),
                                content: Some(openai::MessageContent::Text(content_text)),
                                reasoning_content: None,
                                tool_calls: None,
                                tool_call_id: None,
                                name: None,
                            });
                        }
                    }
                }
            }
        }
    }

    // 3. Translate tools
    //
    // The Responses API carries several tool shapes that Chat Completions has no
    // equivalent for. Codex sends `custom` (freeform `apply_patch`), `namespace`
    // (a group of nested functions) and `web_search` alongside ordinary
    // functions. Emitting those verbatim as `{"type": <kind>, "function": {...}}`
    // is not valid Chat Completions and the gateway rejects the whole request
    // with `11133 Invalid request parameters`, so each shape is mapped onto a
    // plain function tool (or dropped when it has no callable equivalent).
    let tools = req.tools.and_then(|tools| {
        let mapped = normalize_response_tools(tools);
        if mapped.is_empty() {
            None
        } else {
            Some(mapped)
        }
    });
    let tool_choice = normalize_tool_choice(req.tool_choice, &tools);

    let max_tokens = req.max_output_tokens.or(req.max_tokens);
    let stream = req.stream;
    let stream_options = stream.and_then(|s| {
        s.then_some(openai::StreamOptions {
            include_usage: true,
        })
    });

    // Chat Completions has no field for these Responses parameters, but carrying
    // them through is worthwhile where the upstream understands them: Codex
    // relies on `prompt_cache_key` for cross-turn prompt-cache reuse, and
    // `parallel_tool_calls` for concurrent tool execution. Both are sent via the
    // flatten escape hatch so they land on the upstream JSON body untouched.
    let mut extra = serde_json::Map::new();
    if let Some(key) = req.prompt_cache_key.filter(|k| !k.is_empty()) {
        extra.insert("prompt_cache_key".to_string(), Value::String(key));
    }
    if let Some(parallel) = req.parallel_tool_calls {
        extra.insert("parallel_tool_calls".to_string(), Value::Bool(parallel));
    }

    Ok(openai::OpenAIRequest {
        model,
        messages,
        max_tokens,
        temperature: req.temperature,
        top_p: req.top_p,
        stop: None,
        stream,
        stream_options,
        tools,
        tool_choice,
        extra,
    })
}

/// Sanitize a tool name to satisfy upstream constraints (e.g. Tencent Copilot / OpenAI format).
/// Allowed: ^[a-zA-Z0-9_]{1,64}$.
/// Non-alphanumeric characters (like `.`, `-`, `:`, `/`) are replaced with `_`.
/// If the first character is a digit, prepend `_`.
/// Truncated to at most 64 characters.
pub fn sanitize_tool_name(name: &str) -> String {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return String::new();
    }

    let mut sanitized = String::with_capacity(trimmed.len() + 1);
    for c in trimmed.chars() {
        if c.is_ascii_alphanumeric() || c == '_' {
            sanitized.push(c);
        } else {
            sanitized.push('_');
        }
    }

    if sanitized.starts_with(|c: char| c.is_ascii_digit()) {
        sanitized.insert(0, '_');
    }

    if sanitized.len() > 64 {
        sanitized.truncate(64);
    }

    sanitized
}

/// Convert Responses API tools into the Chat Completions function shape.
///
/// Chat Completions only understands `{"type":"function","function":{...}}`.
/// Responses clients send richer kinds, so each is either flattened or dropped:
///
/// * `function` — passed through, defaulting missing parameters to an open object.
/// * `namespace` — a group whose nested `function` tools are flattened to the top
///   level, since Chat Completions has no grouping concept.
/// * `custom` — a freeform tool (Codex `apply_patch`) whose input is a raw string
///   rather than JSON. Modelled as a single required string parameter so the
///   model can still call it.
/// * anything else (`web_search`, `local_shell`, …) — dropped: there is no
///   upstream equivalent, and forwarding the unknown type makes the gateway
///   reject the entire request.
///
/// Tool names are sanitized against `^[a-zA-Z0-9_]{1,64}$` and deduplicated so
/// that duplicate or colliding tool declarations do not trigger upstream 11152 errors.
fn normalize_response_tools(tools: Vec<responses::ResponseTool>) -> Vec<openai::Tool> {
    let mut out = Vec::new();
    let mut seen_names = std::collections::HashSet::new();
    normalize_response_tools_inner(tools, &mut out, &mut seen_names);
    out
}

fn normalize_response_tools_inner(
    tools: Vec<responses::ResponseTool>,
    out: &mut Vec<openai::Tool>,
    seen_names: &mut std::collections::HashSet<String>,
) {
    for tool in tools {
        match tool.tool_type.as_str() {
            // A nested namespace is a container, not a callable tool.
            "namespace" => {
                if let Some(nested) = tool.tools {
                    normalize_response_tools_inner(nested, out, seen_names);
                }
            }
            "custom" => {
                let Some(raw_name) = tool.name.filter(|n| !n.is_empty()) else {
                    continue;
                };
                let name = sanitize_tool_name(&raw_name);
                if name.is_empty() || !seen_names.insert(name.clone()) {
                    continue;
                }
                // `format` carries a grammar/lark definition that Chat Completions
                // cannot express; expose the freeform payload as one string input.
                let description = tool.description.map(|d| {
                    format!("{d}\n\nProvide the tool input as a single string in `input`.")
                });
                out.push(openai::Tool {
                    tool_type: "function".to_string(),
                    function: openai::Function {
                        name,
                        description,
                        parameters: json!({
                            "type": "object",
                            "properties": {
                                "input": {
                                    "type": "string",
                                    "description": "Raw freeform tool input."
                                }
                            },
                            "required": ["input"]
                        }),
                    },
                });
            }
            "function" => {
                if let Some(mut func) = tool.function {
                    // An empty tool name is rejected upstream; drop rather than fail
                    // the entire turn.
                    if func.name.is_empty() {
                        continue;
                    }
                    let name = sanitize_tool_name(&func.name);
                    if name.is_empty() || !seen_names.insert(name.clone()) {
                        continue;
                    }
                    func.name = name;
                    func.parameters = normalize_schema(func.parameters);
                    out.push(openai::Tool {
                        tool_type: "function".to_string(),
                        function: func,
                    });
                } else if let Some(raw_name) = tool.name.filter(|n| !n.is_empty()) {
                    let name = sanitize_tool_name(&raw_name);
                    if name.is_empty() || !seen_names.insert(name.clone()) {
                        continue;
                    }
                    let raw_schema = tool
                        .parameters
                        .unwrap_or_else(|| json!({"type": "object", "properties": {}}));
                    out.push(openai::Tool {
                        tool_type: "function".to_string(),
                        function: openai::Function {
                            name,
                            description: tool.description,
                            parameters: normalize_schema(raw_schema),
                        },
                    });
                }
            }
            _ => {}
        }
    }
}

/// Translate a Responses `tool_choice` into a Chat Completions value.
///
/// String forms (`auto` / `none` / `required`) are shared. The Responses object
/// form `{"type":"function","name":…}` has to become the Chat Completions
/// `{"type":"function","function":{"name":…}}`, because upstream rejects the
/// object form outright ("cannot unmarshal object into Go struct field
/// Request.tool_choice of type string").
fn normalize_tool_choice(
    choice: Option<Value>,
    tools: &Option<Vec<openai::Tool>>,
) -> Option<Value> {
    let choice = choice?;

    match choice {
        Value::String(_) => Some(choice),
        Value::Object(ref obj) => {
            let kind = obj.get("type").and_then(Value::as_str).unwrap_or_default();
            // `namespace` selection refers to nested tools upstream cannot see, and a
            // dangling reference would be rejected; let the model choose freely.
            let name = obj.get("name").and_then(Value::as_str).or_else(|| {
                obj.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
            });

            match (kind, name) {
                ("function", Some(raw_name)) => {
                    let sanitized = sanitize_tool_name(raw_name);
                    if tool_exists(tools, &sanitized) {
                        Some(json!({"type": "function", "function": {"name": sanitized}}))
                    } else {
                        Some(json!("auto"))
                    }
                }
                // A choice naming a dropped tool (e.g. web_search) cannot be honoured.
                _ => Some(json!("auto")),
            }
        }
        _ => Some(json!("auto")),
    }
}

/// Whether a tool with `name` survived normalization.
fn tool_exists(tools: &Option<Vec<openai::Tool>>, name: &str) -> bool {
    tools
        .as_ref()
        .is_some_and(|tools| tools.iter().any(|t| t.function.name == name))
}

/// Translates an upstream non-streaming OpenAI Chat Completions response into an OpenAI Responses API response.
pub fn translate_responses_response(
    resp: openai::OpenAIResponse,
    requested_model: &str,
) -> ProxyResult<responses::ResponsesResponse> {
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| ProxyError::Transform("No choices in response".to_string()))?;

    let response_id = resp.id.unwrap_or_else(|| generate_id("resp"));
    let model = resp.model.unwrap_or_else(|| requested_model.to_string());
    let created_at = resp
        .created
        .map(|c| c as i64)
        .unwrap_or_else(current_timestamp);

    let mut output = Vec::new();
    let mut response_content_items = Vec::new();

    if let Some(ref text) = choice.message.content {
        if !text.is_empty() {
            output.push(responses::OutputItem::Message {
                id: generate_id("msg"),
                status: "completed".to_string(),
                role: choice.message.role.clone(),
                content: vec![responses::OutputContentPart::OutputText { text: text.clone() }],
            });

            response_content_items.push(responses::ResponseContentItem {
                content_type: "output_text".to_string(),
                text: Some(text.clone()),
            });
        }
    }

    if let Some(ref tool_calls) = choice.message.tool_calls {
        for call in tool_calls {
            output.push(responses::OutputItem::FunctionCall {
                id: call.id.clone(),
                call_id: call.id.clone(),
                status: "completed".to_string(),
                name: call.function.name.clone(),
                arguments: call.function.arguments.clone(),
            });
        }
    }

    let response_output = if !response_content_items.is_empty() {
        Some(responses::ResponseOutput {
            role: Some(choice.message.role.clone()),
            content: Some(response_content_items),
        })
    } else {
        None
    };

    let usage = Some(responses::ResponsesUsage {
        total_tokens: resp.usage.total_tokens,
        input_tokens: resp.usage.prompt_tokens,
        output_tokens: resp.usage.completion_tokens,
        input_tokens_details: resp.usage.prompt_tokens_details,
    });

    Ok(responses::ResponsesResponse {
        id: response_id,
        object: "response".to_string(),
        created_at,
        status: "completed".to_string(),
        model,
        output,
        usage,
        response: response_output,
    })
}

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
    accumulated_text: String,
    active_tool_calls: Vec<StreamingToolCall>,
    output_index_counter: usize,
    finalized: bool,
    pending_usage: Option<openai::Usage>,
}

#[derive(Debug, Clone)]
struct StreamingToolCall {
    id: String,
    call_id: String,
    name: String,
    arguments: String,
    output_index: usize,
    started: bool,
    done: bool,
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
        accumulated_text: String::new(),
        active_tool_calls: Vec::new(),
        output_index_counter: 0,
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
        return events;
    };

    // 2. Handle text content delta
    if let Some(ref content) = choice.delta.content {
        if !content.is_empty() {
            if !state.text_item_started {
                state.text_item_started = true;
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
                output_index: 0,
                content_index: 0,
                delta: content.clone(),
            });
        }
    }

    // 3. Handle tool calls delta
    if let Some(ref tool_calls) = choice.delta.tool_calls {
        for call in tool_calls {
            let index = call.index;
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

            if !tool_entry.started
                && (!tool_entry.name.is_empty() || !tool_entry.call_id.is_empty())
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
            events.extend(close_stream_items(state));
        }
    }

    events
}

fn close_stream_items(state: &mut ResponsesStreamState) -> Vec<responses::ResponsesStreamEvent> {
    let mut events = Vec::new();
    if state.finalized {
        return events;
    }

    // Close text item if active
    if state.text_item_started {
        events.push(responses::ResponsesStreamEvent::OutputTextDone {
            response_id: state.response_id.clone(),
            item_id: state.text_item_id.clone(),
            output_index: 0,
            content_index: 0,
            text: state.accumulated_text.clone(),
        });

        events.push(responses::ResponsesStreamEvent::OutputItemDone {
            response_id: state.response_id.clone(),
            output_index: 0,
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

    state.finalized = true;
    events
}

/// Closes any unfinalized stream items and emits `response.completed` upon stream completion.
pub fn translate_stream_done(
    state: &mut ResponsesStreamState,
) -> Vec<responses::ResponsesStreamEvent> {
    if !state.finalized {
        close_stream_items(state)
    } else {
        Vec::new()
    }
}

/// Translates a stream error into `response.failed`.
pub fn translate_stream_error(
    state: &ResponsesStreamState,
    error_message: String,
) -> Vec<responses::ResponsesStreamEvent> {
    vec![responses::ResponsesStreamEvent::Failed {
        response_id: state.response_id.clone(),
        error: responses::StreamErrorDetail {
            message: error_message,
            error_type: "server_error".to_string(),
            code: None,
        },
    }]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn test_policy() -> TranslationPolicy {
        TranslationPolicy {
            reasoning_model: None,
            completion_model: None,
            model_map: BTreeMap::new(),
            ignore_terms: vec!["ignore-me".to_string()],
            strip_model_suffix: false,
            sanitize_fingerprints: true,
        }
    }

    fn req_for_model(model: &str) -> responses::ResponsesRequest {
        responses::ResponsesRequest {
            model: model.to_string(),
            input: responses::ResponsesInput::Text("hi".to_string()),
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
        }
    }

    #[test]
    fn a_configured_model_outranks_the_client_model() {
        // Regression: this path used to look up `model_map` against the *client*
        // model first and only fall back to `completion_model`, so a configured
        // model lost to a stale mapping and the same request reached a
        // different upstream model here than on `/v1/messages`.
        let policy = TranslationPolicy {
            completion_model: Some("gpt-5".to_string()),
            model_map: [("gpt-4o".to_string(), "glm-5.3".to_string())]
                .into_iter()
                .collect(),
            ..test_policy()
        };

        let openai_req = translate_responses_request(req_for_model("gpt-4o"), &policy).unwrap();
        assert_eq!(openai_req.model, "gpt-5");
    }

    #[test]
    fn the_client_model_survives_when_nothing_overrides_it() {
        let openai_req =
            translate_responses_request(req_for_model("gpt-4o"), &test_policy()).unwrap();
        assert_eq!(openai_req.model, "gpt-4o");
    }

    #[test]
    fn test_translate_request_string_input_and_instructions() {
        let req = responses::ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: responses::ResponsesInput::Text("Hello AI".to_string()),
            instructions: Some("You are helpful. ignore-me".to_string()),
            tools: None,
            tool_choice: None,
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_output_tokens: Some(100),
            max_tokens: None,
            stream: Some(true),
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
            prompt_cache_key: None,
        };

        let policy = test_policy();
        let openai_req = translate_responses_request(req, &policy).unwrap();

        assert_eq!(openai_req.model, "gpt-4o");
        assert_eq!(openai_req.messages.len(), 2);
        assert_eq!(openai_req.messages[0].role, "system");
        if let Some(openai::MessageContent::Text(ref txt)) = openai_req.messages[0].content {
            assert_eq!(txt, "You are helpful.");
        } else {
            panic!("Expected text content");
        }

        assert_eq!(openai_req.messages[1].role, "user");
        if let Some(openai::MessageContent::Text(ref txt)) = openai_req.messages[1].content {
            assert_eq!(txt, "Hello AI");
        } else {
            panic!("Expected text content");
        }

        assert_eq!(openai_req.temperature, Some(0.7));
        assert_eq!(openai_req.max_tokens, Some(100));
        assert_eq!(openai_req.stream, Some(true));
    }

    #[test]
    fn test_translate_request_with_tool_call_and_output() {
        let items = vec![
            responses::ResponseInputItem::Message {
                item_type: Some("message".to_string()),
                id: None,
                role: "user".to_string(),
                content: responses::ResponseMessageContent::Text(
                    "What is the weather?".to_string(),
                ),
            },
            responses::ResponseInputItem::FunctionCall {
                item_type: "function_call".to_string(),
                id: Some("call_123".to_string()),
                call_id: Some("call_123".to_string()),
                name: "get_weather".to_string(),
                arguments: "{\"location\":\"Paris\"}".to_string(),
            },
            responses::ResponseInputItem::FunctionCallOutput {
                item_type: "function_call_output".to_string(),
                call_id: "call_123".to_string(),
                output: Value::String("22°C, Sunny".to_string()),
            },
        ];

        let req = responses::ResponsesRequest {
            model: "gpt-4o".to_string(),
            input: responses::ResponsesInput::Items(items),
            instructions: None,
            tools: Some(vec![responses::ResponseTool {
                tool_type: "function".to_string(),
                name: Some("get_weather".to_string()),
                description: Some("Get current weather".to_string()),
                parameters: Some(json!({"type": "object"})),
                function: None,
                tools: None,
            }]),
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            max_tokens: None,
            stream: None,
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
            prompt_cache_key: None,
        };

        let policy = test_policy();
        let openai_req = translate_responses_request(req, &policy).unwrap();

        assert_eq!(openai_req.messages.len(), 3);
        assert_eq!(openai_req.messages[0].role, "user");
        assert_eq!(openai_req.messages[1].role, "assistant");
        assert!(openai_req.messages[1].tool_calls.is_some());
        assert_eq!(openai_req.messages[2].role, "tool");
        assert_eq!(
            openai_req.messages[2].tool_call_id,
            Some("call_123".to_string())
        );

        let tools = openai_req.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "get_weather");
    }

    /// Parallel tool calls must collapse into one assistant turn.
    ///
    /// Regression: Codex issues several `function_call` items in a row. Emitting
    /// one assistant message per call left the first call unanswered when the
    /// next assistant message began, and strict upstreams rejected the whole
    /// request (`11148 tool calls and tool results do not match`) on every
    /// subsequent turn of the conversation.
    #[test]
    fn parallel_function_calls_share_one_assistant_message() {
        let items = vec![
            responses::ResponseInputItem::FunctionCall {
                item_type: "function_call".to_string(),
                id: Some("call_00".to_string()),
                call_id: Some("call_00".to_string()),
                name: "exec_command".to_string(),
                arguments: "{\"cmd\":\"ls\"}".to_string(),
            },
            responses::ResponseInputItem::FunctionCall {
                item_type: "function_call".to_string(),
                id: Some("call_01".to_string()),
                call_id: Some("call_01".to_string()),
                name: "exec_command".to_string(),
                arguments: "{\"cmd\":\"pwd\"}".to_string(),
            },
            responses::ResponseInputItem::FunctionCallOutput {
                item_type: "function_call_output".to_string(),
                call_id: "call_00".to_string(),
                output: Value::String("files".to_string()),
            },
            responses::ResponseInputItem::FunctionCallOutput {
                item_type: "function_call_output".to_string(),
                call_id: "call_01".to_string(),
                output: Value::String("/root".to_string()),
            },
        ];

        let req = responses::ResponsesRequest {
            model: "hy3".to_string(),
            input: responses::ResponsesInput::Items(items),
            instructions: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            max_tokens: None,
            stream: None,
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
            prompt_cache_key: None,
        };

        let out = translate_responses_request(req, &test_policy()).unwrap();

        let roles: Vec<&str> = out.messages.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(
            roles,
            vec!["assistant", "tool", "tool"],
            "both calls must live on a single assistant turn: {roles:?}"
        );

        let calls = out.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2, "both calls must be preserved: {calls:?}");
        assert_eq!(calls[0].id, "call_00");
        assert_eq!(calls[1].id, "call_01");

        assert_eq!(out.messages[1].tool_call_id, Some("call_00".to_string()));
        assert_eq!(out.messages[2].tool_call_id, Some("call_01".to_string()));
    }

    /// A tool call following an assistant *text* message starts a new turn, since
    /// merging into it would attach calls to unrelated prose.
    #[test]
    fn function_call_after_assistant_text_starts_new_message() {
        let items = vec![
            responses::ResponseInputItem::Message {
                item_type: Some("message".to_string()),
                id: None,
                role: "assistant".to_string(),
                content: responses::ResponseMessageContent::Text("thinking".to_string()),
            },
            responses::ResponseInputItem::FunctionCall {
                item_type: "function_call".to_string(),
                id: Some("call_00".to_string()),
                call_id: Some("call_00".to_string()),
                name: "exec_command".to_string(),
                arguments: "{}".to_string(),
            },
        ];

        let req = responses::ResponsesRequest {
            model: "hy3".to_string(),
            input: responses::ResponsesInput::Items(items),
            instructions: None,
            tools: None,
            tool_choice: None,
            temperature: None,
            top_p: None,
            max_output_tokens: None,
            max_tokens: None,
            stream: None,
            parallel_tool_calls: None,
            reasoning: None,
            store: None,
            include: None,
            prompt_cache_key: None,
        };

        let out = translate_responses_request(req, &test_policy()).unwrap();
        assert_eq!(out.messages.len(), 2);
        assert!(
            out.messages[0].tool_calls.is_none(),
            "text message unchanged"
        );
        assert_eq!(out.messages[1].tool_calls.as_ref().unwrap().len(), 1);
    }

    /// Build a minimal request carrying `tools`, for the shape-normalization tests.
    fn req_with_tools(tools: Vec<responses::ResponseTool>) -> responses::ResponsesRequest {
        responses::ResponsesRequest {
            model: "hy3".to_string(),
            input: responses::ResponsesInput::Text("hi".to_string()),
            instructions: None,
            tools: Some(tools),
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
        }
    }

    fn function_tool(name: &str) -> responses::ResponseTool {
        responses::ResponseTool {
            tool_type: "function".to_string(),
            name: Some(name.to_string()),
            description: None,
            parameters: None,
            function: None,
            tools: None,
        }
    }

    /// Codex sends `custom` and `namespace` tools; emitting them verbatim used to
    /// make the gateway reject the whole turn with 11133.
    #[test]
    fn codex_custom_and_namespace_tools_become_functions() {
        let tools = vec![
            responses::ResponseTool {
                tool_type: "custom".to_string(),
                name: Some("apply_patch".to_string()),
                description: Some("Edit files.".to_string()),
                parameters: None,
                function: None,
                tools: None,
            },
            responses::ResponseTool {
                tool_type: "namespace".to_string(),
                name: Some("multi_agent_v1".to_string()),
                description: None,
                parameters: None,
                function: None,
                tools: Some(vec![
                    function_tool("close_agent"),
                    function_tool("spawn_agent"),
                ]),
            },
        ];

        let out = translate_responses_request(req_with_tools(tools), &test_policy()).unwrap();
        let tools = out.tools.expect("tools should survive");

        assert_eq!(
            tools
                .iter()
                .map(|t| t.function.name.as_str())
                .collect::<Vec<_>>(),
            vec!["apply_patch", "close_agent", "spawn_agent"]
        );
        // Every emitted tool must be a plain function; anything else is rejected upstream.
        assert!(tools.iter().all(|t| t.tool_type == "function"));
        // The freeform tool must expose its payload as a required string.
        let apply_patch = &tools[0].function.parameters;
        assert_eq!(apply_patch["required"], json!(["input"]));
        assert_eq!(apply_patch["properties"]["input"]["type"], "string");
    }

    /// Tools with no Chat Completions equivalent are dropped rather than forwarded.
    #[test]
    fn unsupported_tool_kinds_are_dropped() {
        let tools = vec![
            function_tool("exec_command"),
            responses::ResponseTool {
                tool_type: "web_search".to_string(),
                name: None,
                description: None,
                parameters: None,
                function: None,
                tools: None,
            },
        ];

        let out = translate_responses_request(req_with_tools(tools), &test_policy()).unwrap();
        let tools = out.tools.expect("kept tools should survive");
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "exec_command");
    }

    /// A tool list that normalizes to nothing must omit `tools` entirely.
    #[test]
    fn only_unsupported_tools_yields_no_tools_field() {
        let tools = vec![responses::ResponseTool {
            tool_type: "web_search".to_string(),
            name: None,
            description: None,
            parameters: None,
            function: None,
            tools: None,
        }];

        let out = translate_responses_request(req_with_tools(tools), &test_policy()).unwrap();
        assert!(out.tools.is_none());
    }

    /// The object form of `tool_choice` must be rewritten for Chat Completions.
    #[test]
    fn object_tool_choice_is_rewritten_to_chat_shape() {
        let mut req = req_with_tools(vec![function_tool("exec_command")]);
        req.tool_choice = Some(json!({"type": "function", "name": "exec_command"}));

        let out = translate_responses_request(req, &test_policy()).unwrap();
        assert_eq!(
            out.tool_choice,
            Some(json!({"type": "function", "function": {"name": "exec_command"}}))
        );
    }

    /// Choosing a dropped tool cannot be honoured, so fall back to `auto`.
    #[test]
    fn tool_choice_naming_dropped_tool_falls_back_to_auto() {
        let mut req = req_with_tools(vec![
            function_tool("exec_command"),
            responses::ResponseTool {
                tool_type: "web_search".to_string(),
                name: None,
                description: None,
                parameters: None,
                function: None,
                tools: None,
            },
        ]);
        req.tool_choice = Some(json!({"type": "web_search"}));

        let out = translate_responses_request(req, &test_policy()).unwrap();
        assert_eq!(out.tool_choice, Some(json!("auto")));
    }

    /// String forms are shared with Chat Completions and pass through unchanged.
    #[test]
    fn string_tool_choice_passes_through() {
        for value in ["auto", "none", "required"] {
            let mut req = req_with_tools(vec![function_tool("exec_command")]);
            req.tool_choice = Some(json!(value));
            let out = translate_responses_request(req, &test_policy()).unwrap();
            assert_eq!(out.tool_choice, Some(json!(value)));
        }
    }

    /// Codex sends the session id as `prompt_cache_key`; dropping it removes
    /// cross-turn prompt-cache reuse, so it must reach the upstream body.
    #[test]
    fn prompt_cache_key_is_forwarded() {
        let mut req = req_with_tools(vec![function_tool("exec_command")]);
        req.prompt_cache_key = Some("01a0c952-88b9-7390-ab02-df5c7e8a41a6".to_string());

        let out = translate_responses_request(req, &test_policy()).unwrap();
        assert_eq!(
            out.extra.get("prompt_cache_key"),
            Some(&json!("01a0c952-88b9-7390-ab02-df5c7e8a41a6"))
        );
    }

    /// An absent or empty key must not add a field to the upstream body.
    #[test]
    fn absent_prompt_cache_key_adds_nothing() {
        for value in [None, Some(String::new())] {
            let mut req = req_with_tools(vec![function_tool("exec_command")]);
            req.prompt_cache_key = value;
            let out = translate_responses_request(req, &test_policy()).unwrap();
            assert!(!out.extra.contains_key("prompt_cache_key"));
        }
    }

    /// `parallel_tool_calls` has no Chat Completions field but upstream honors it.
    #[test]
    fn parallel_tool_calls_is_forwarded() {
        for value in [true, false] {
            let mut req = req_with_tools(vec![function_tool("exec_command")]);
            req.parallel_tool_calls = Some(value);
            let out = translate_responses_request(req, &test_policy()).unwrap();
            assert_eq!(out.extra.get("parallel_tool_calls"), Some(&json!(value)));
        }
    }

    /// Nothing extra should be emitted when the client sent none of these.
    #[test]
    fn no_optional_fields_yields_empty_extra() {
        let out =
            translate_responses_request(req_with_tools(vec![function_tool("t")]), &test_policy())
                .unwrap();
        assert!(
            out.extra.is_empty(),
            "unexpected extra fields: {:?}",
            out.extra
        );
    }

    /// A `function` entry may arrive with a top-level name instead of `function`.
    #[test]
    fn function_tool_with_top_level_name_keeps_parameters() {
        let tools = vec![responses::ResponseTool {
            tool_type: "function".to_string(),
            name: Some("get_weather".to_string()),
            description: Some("weather".to_string()),
            parameters: Some(json!({"type": "object", "properties": {"city": {"type": "string"}}})),
            function: None,
            tools: None,
        }];

        let out = translate_responses_request(req_with_tools(tools), &test_policy()).unwrap();
        let tools = out.tools.unwrap();
        assert_eq!(tools[0].function.name, "get_weather");
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(
            tools[0].function.parameters["properties"]["city"]["type"],
            "string"
        );
    }

    /// A `function` tool without parameters still gets a valid empty schema.
    #[test]
    fn function_tool_without_parameters_gets_empty_schema() {
        let out = translate_responses_request(
            req_with_tools(vec![function_tool("ping")]),
            &test_policy(),
        )
        .unwrap();
        let tools = out.tools.unwrap();
        assert_eq!(
            tools[0].function.parameters,
            json!({"type": "object", "properties": {}, "required": []})
        );
    }

    /// An unnamed function tool is dropped instead of sending an empty name upstream.
    #[test]
    fn function_tool_with_empty_name_is_dropped() {
        let out = translate_responses_request(
            req_with_tools(vec![function_tool(""), function_tool("keep_me")]),
            &test_policy(),
        )
        .unwrap();
        let tools = out.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].function.name, "keep_me");
    }

    #[test]
    fn test_translate_non_streaming_response() {
        let openai_resp = openai::OpenAIResponse {
            id: Some("chatcmpl-test".to_string()),
            object: Some("chat.completion".to_string()),
            created: Some(1712345678),
            model: Some("gpt-4o-2024-05-13".to_string()),
            choices: vec![openai::Choice {
                index: 0,
                message: openai::ChoiceMessage {
                    role: "assistant".to_string(),
                    content: Some("Hello! How can I assist you?".to_string()),
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: openai::Usage {
                prompt_tokens: 10,
                completion_tokens: 8,
                total_tokens: 18,
                prompt_tokens_details: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                ..Default::default()
            },
            system_fingerprint: None,
        };

        let resp = translate_responses_response(openai_resp, "gpt-4o").unwrap();
        assert_eq!(resp.id, "chatcmpl-test");
        assert_eq!(resp.model, "gpt-4o-2024-05-13");
        assert_eq!(resp.output.len(), 1);
        match &resp.output[0] {
            responses::OutputItem::Message { content, role, .. } => {
                assert_eq!(role, "assistant");
                assert_eq!(content.len(), 1);
                match &content[0] {
                    responses::OutputContentPart::OutputText { text } => {
                        assert_eq!(text, "Hello! How can I assist you?");
                    }
                }
            }
            _ => panic!("Expected Message output item"),
        }

        assert_eq!(resp.usage.as_ref().unwrap().total_tokens, 18);
        assert!(resp.response.is_some());
    }

    /// A chunk carrying a tool-call delta, as the upstream opens a call: the
    /// first delta names the function, later ones stream the arguments.
    fn tool_chunk(index: usize, id: Option<&str>, name: Option<&str>) -> openai::StreamChunk {
        let mut chunk = upstream_chunk("", None);
        chunk.choices[0].delta.content = None;
        chunk.choices[0].delta.tool_calls = Some(vec![openai::DeltaToolCall {
            index,
            id: id.map(str::to_string),
            call_type: Some("function".to_string()),
            function: Some(openai::DeltaFunctionCall {
                name: name.map(str::to_string),
                arguments: Some(String::new()),
            }),
        }]);
        chunk
    }

    #[test]
    fn summary_flags_a_text_only_turn_as_unactionable() {
        // The stall diagnostic: text with no started tool call means Codex has
        // nothing to execute. The summary must make both halves visible.
        let mut state = initial_stream_state("glm-5.3-flash".to_string());
        translate_stream_chunk(
            &mut state,
            &upstream_chunk("I'll fix that now.", Some("stop")),
        );

        let s = state.summary();
        assert_eq!(s.text_len, "I'll fix that now.".chars().count());
        assert_eq!(s.started_tool_calls, 0, "no tool call was opened");
        assert_eq!(s.active_tool_calls, 0);
    }

    #[test]
    fn summary_counts_a_started_tool_call() {
        let mut state = initial_stream_state("gpt-4o".to_string());
        translate_stream_chunk(&mut state, &upstream_chunk("Working on it.", None));
        translate_stream_chunk(&mut state, &tool_chunk(0, Some("call_1"), Some("shell")));

        let s = state.summary();
        assert!(s.text_len > 0);
        assert_eq!(
            s.started_tool_calls, 1,
            "a named call must count as started"
        );
        assert_eq!(s.active_tool_calls, 1);
    }

    #[test]
    fn summary_separates_a_bare_slot_from_a_started_call() {
        // Some upstreams emit a `tool_calls` delta with neither id nor name —
        // the slot exists but never started, so nothing was emitted to the
        // client. Reporting both counts keeps that distinguishable from a
        // model that emitted no tool call at all.
        let mut state = initial_stream_state("gpt-4o".to_string());
        translate_stream_chunk(&mut state, &tool_chunk(0, None, None));

        let s = state.summary();
        assert_eq!(s.active_tool_calls, 1, "the slot was opened");
        assert_eq!(s.started_tool_calls, 0, "but no item reached the client");
    }

    /// A chunk exactly as WorkBuddy/copilot.tencent.com emits it:
    /// `"finish_reason": ""` on every intermediate chunk, with the real reason
    /// only on the last one.
    fn upstream_chunk(content: &str, finish_reason: Option<&str>) -> openai::StreamChunk {
        openai::StreamChunk {
            id: Some("cmb-1".to_string()),
            object: Some("chat.completion.chunk".to_string()),
            created: Some(1712345678),
            model: Some("glm-5.3-flash".to_string()),
            choices: vec![openai::StreamChoice {
                index: 0,
                delta: openai::Delta {
                    role: Some("assistant".to_string()),
                    content: Some(content.to_string()),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                },
                finish_reason: finish_reason.map(|s| s.to_string()),
            }],
            usage: None,
        }
    }

    #[test]
    fn empty_finish_reason_does_not_close_the_response() {
        // Regression: upstreams that send `"finish_reason": ""` on every chunk
        // used to finalize the response on chunk #1, so `response.completed`
        // arrived with empty output before any text — and the client rendered
        // nothing at all.
        let mut state = initial_stream_state("glm-5.3-flash".to_string());

        let first = translate_stream_chunk(&mut state, &upstream_chunk("OK", Some("")));
        let types: Vec<&str> = first.iter().map(|e| e.event_type()).collect();
        assert!(
            !types.contains(&"response.completed"),
            "empty finish_reason must not finalize: {types:?}"
        );
        assert!(
            types.contains(&"response.output_text.delta"),
            "text delta should still be emitted: {types:?}"
        );

        // A second content chunk must still be accepted into the same item.
        let second = translate_stream_chunk(&mut state, &upstream_chunk("!", Some("")));
        let types: Vec<&str> = second.iter().map(|e| e.event_type()).collect();
        assert_eq!(
            types,
            vec!["response.output_text.delta"],
            "no new item should be started: {types:?}"
        );

        // Only the real reason closes it, and the text is carried through.
        let last = translate_stream_chunk(&mut state, &upstream_chunk("", Some("stop")));
        let types: Vec<&str> = last.iter().map(|e| e.event_type()).collect();
        assert_eq!(
            types,
            vec![
                "response.output_text.done",
                "response.output_item.done",
                "response.completed"
            ]
        );

        match &last[0] {
            responses::ResponsesStreamEvent::OutputTextDone { text, .. } => {
                assert_eq!(text, "OK!");
            }
            other => panic!("expected OutputTextDone, got {other:?}"),
        }

        match &last[2] {
            responses::ResponsesStreamEvent::Completed { response } => {
                assert_eq!(response.output.len(), 1, "completed must carry the text");
            }
            other => panic!("expected Completed, got {other:?}"),
        }
    }

    #[test]
    fn test_stream_lifecycle() {
        let mut state = initial_stream_state("gpt-4o".to_string());

        let chunk1 = openai::StreamChunk {
            id: Some("chunk-1".to_string()),
            object: Some("chat.completion.chunk".to_string()),
            created: Some(1712345678),
            model: Some("gpt-4o".to_string()),
            choices: vec![openai::StreamChoice {
                index: 0,
                delta: openai::Delta {
                    role: Some("assistant".to_string()),
                    content: Some("Hello".to_string()),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                },
                finish_reason: None,
            }],
            usage: None,
        };

        let events1 = translate_stream_chunk(&mut state, &chunk1);
        // Should have Created, OutputItemAdded, ContentPartAdded, OutputTextDelta
        assert_eq!(events1.len(), 4);
        assert_eq!(events1[0].event_type(), "response.created");
        assert_eq!(events1[1].event_type(), "response.output_item.added");
        assert_eq!(events1[2].event_type(), "response.content_part.added");
        assert_eq!(events1[3].event_type(), "response.output_text.delta");

        let chunk2 = openai::StreamChunk {
            id: Some("chunk-2".to_string()),
            object: Some("chat.completion.chunk".to_string()),
            created: Some(1712345679),
            model: Some("gpt-4o".to_string()),
            choices: vec![openai::StreamChoice {
                index: 0,
                delta: openai::Delta {
                    role: None,
                    content: Some(" world!".to_string()),
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(openai::Usage {
                prompt_tokens: 5,
                completion_tokens: 3,
                total_tokens: 8,
                prompt_tokens_details: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                ..Default::default()
            }),
        };

        let events2 = translate_stream_chunk(&mut state, &chunk2);
        // OutputTextDelta, OutputTextDone, OutputItemDone, Completed
        assert_eq!(events2.len(), 4);
        assert_eq!(events2[0].event_type(), "response.output_text.delta");
        assert_eq!(events2[1].event_type(), "response.output_text.done");
        assert_eq!(events2[2].event_type(), "response.output_item.done");
        assert_eq!(events2[3].event_type(), "response.completed");

        let done_events = translate_stream_done(&mut state);
        assert!(done_events.is_empty(), "Already finalized");
    }

    #[test]
    fn sanitize_tool_name_formats_validly() {
        assert_eq!(sanitize_tool_name("read_file"), "read_file");
        assert_eq!(
            sanitize_tool_name("workspace.edit_file"),
            "workspace_edit_file"
        );
        assert_eq!(
            sanitize_tool_name("multi:agent-tool/call"),
            "multi_agent_tool_call"
        );
        assert_eq!(sanitize_tool_name("123action"), "_123action");
        assert_eq!(sanitize_tool_name(""), "");
        let long_name = "a".repeat(100);
        assert_eq!(sanitize_tool_name(&long_name).len(), 64);
    }

    #[test]
    fn duplicate_and_colliding_tools_are_deduplicated() {
        let tools = vec![
            function_tool("execute_code"),
            function_tool("execute_code"),
            function_tool("execute.code"),
            responses::ResponseTool {
                tool_type: "custom".to_string(),
                name: Some("execute_code".to_string()),
                description: None,
                parameters: None,
                function: None,
                tools: None,
            },
            responses::ResponseTool {
                tool_type: "namespace".to_string(),
                name: Some("ns".to_string()),
                description: None,
                parameters: None,
                function: None,
                tools: Some(vec![
                    function_tool("execute_code"),
                    function_tool("other_tool"),
                ]),
            },
        ];

        let out = translate_responses_request(req_with_tools(tools), &test_policy()).unwrap();
        let tools = out.tools.expect("tools should survive");

        assert_eq!(
            tools
                .iter()
                .map(|t| t.function.name.as_str())
                .collect::<Vec<_>>(),
            vec!["execute_code", "other_tool"]
        );
    }

    #[test]
    fn tool_parameters_are_normalized() {
        let mut tool = function_tool("test_schema");
        tool.parameters = Some(json!({
            "type": "object",
            "properties": {
                "foo": { "type": "string" }
            }
        }));

        let out = translate_responses_request(req_with_tools(vec![tool]), &test_policy()).unwrap();
        let tools = out.tools.unwrap();
        // Object schema must have "required" array added by normalize_schema
        assert_eq!(tools[0].function.parameters["required"], json!([]));
    }

    fn parse_items(json: &str) -> Vec<openai::Message> {
        let req: responses::ResponsesRequest = serde_json::from_str(json).unwrap();
        translate_responses_request(req, &test_policy())
            .unwrap()
            .messages
    }

    #[test]
    fn parallel_tool_calls_share_one_assistant_message() {
        // Responses sends parallel calls as consecutive items; Chat Completions
        // needs them on one assistant message, otherwise the upstream sees an
        // assistant turn whose calls are never answered before the next turn.
        let msgs = parse_items(
            r#"{"model":"m","input":[
                {"type":"function_call","call_id":"c1","name":"f1","arguments":"{}"},
                {"type":"function_call","call_id":"c2","name":"f2","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"r1"},
                {"type":"function_call_output","call_id":"c2","output":"r2"}
            ]}"#,
        );

        let assistants: Vec<_> = msgs.iter().filter(|m| m.role == "assistant").collect();
        assert_eq!(
            assistants.len(),
            1,
            "both calls must share one assistant message: {msgs:#?}"
        );
        let calls = assistants[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "c1");
        assert_eq!(calls[1].id, "c2");
        assert_eq!(msgs.iter().filter(|m| m.role == "tool").count(), 2);
    }

    #[test]
    fn orphan_tool_result_gets_a_matching_call() {
        // A result for a call the request never declared would reach the
        // upstream unpaired, which it rejects with `11148 tool calls and tool
        // results do not match`.
        let msgs = parse_items(
            r#"{"model":"m","input":[
                {"type":"message","role":"user","content":"hi"},
                {"type":"function_call_output","call_id":"ghost","output":"r"}
            ]}"#,
        );

        let tool = msgs.iter().find(|m| m.role == "tool").expect("tool result");
        assert_eq!(tool.tool_call_id.as_deref(), Some("ghost"));
        let paired = msgs.iter().any(|m| {
            m.role == "assistant"
                && m.tool_calls
                    .as_ref()
                    .is_some_and(|v| v.iter().any(|t| t.id == "ghost"))
        });
        assert!(
            paired,
            "orphan result must be paired with a call: {msgs:#?}"
        );
    }

    #[test]
    fn sequential_tool_turns_stay_separate() {
        // Two turns (call, result, call, result) must NOT be merged — only
        // consecutive calls within one turn merge.
        let msgs = parse_items(
            r#"{"model":"m","input":[
                {"type":"function_call","call_id":"c1","name":"f","arguments":"{}"},
                {"type":"function_call_output","call_id":"c1","output":"r1"},
                {"type":"function_call","call_id":"c2","name":"f","arguments":"{}"},
                {"type":"function_call_output","call_id":"c2","output":"r2"}
            ]}"#,
        );
        let assistants = msgs.iter().filter(|m| m.role == "assistant").count();
        assert_eq!(assistants, 2, "separate turns stay separate: {msgs:#?}");
    }
}
