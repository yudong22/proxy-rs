//! Responses API request → OpenAI Chat request, including tool normalization.

use super::*;

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
                        let role = if role == "developer" {
                            "system".to_string()
                        } else {
                            role
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
                            let role = if role == "developer" { "system" } else { role };
                            let content_text = val
                                .get("content")
                                .and_then(|c| c.as_str())
                                .unwrap_or("")
                                .to_string();
                            let tool_calls: Option<Vec<openai::ToolCall>> = val
                                .get("tool_calls")
                                .and_then(|tc| serde_json::from_value(tc.clone()).ok());
                            let tool_call_id = val
                                .get("tool_call_id")
                                .and_then(|tci| tci.as_str())
                                .map(|s| s.to_string());
                            let content = if content_text.is_empty() && tool_calls.is_some() {
                                None
                            } else {
                                Some(openai::MessageContent::Text(content_text))
                            };
                            messages.push(openai::Message {
                                role: role.to_string(),
                                content,
                                reasoning_content: None,
                                tool_calls,
                                tool_call_id,
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
    if let Some(ref r) = req.reasoning {
        let effort = if let Some(s) = r.as_str() {
            Some(s.to_string())
        } else {
            r.get("effort")
                .and_then(|v| v.as_str())
                .map(|eff| eff.to_string())
        };
        if let Some(effort) = effort {
            extra.insert("reasoning_effort".to_string(), Value::String(effort));
        }
    }

    Ok(openai::OpenAIRequest {
        model,
        messages,
        max_tokens,
        max_completion_tokens: req.max_output_tokens,
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
                ("function" | "custom", Some(raw_name)) => {
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
