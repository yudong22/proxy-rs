//! Request/response translation entry points and the policy they take.

use super::*;

pub struct TranslationPolicy {
    pub reasoning_model: Option<String>,
    pub completion_model: Option<String>,
    pub model_map: BTreeMap<String, String>,
    pub ignore_terms: Vec<String>,
    /// When true (WorkBuddy/CodeBuddy flavor), strip a trailing `[…]` tag such
    /// as `[1M]` from the model id before sending upstream. Clients like
    /// Claude Code append context-size hints that the gateway does not
    /// understand and rejects with `11102 model … service info not found`.
    pub strip_model_suffix: bool,
    /// When true (WorkBuddy/CodeBuddy flavor), neutralize upstream content-filter
    /// fingerprints across every outbound message field (content, tool arguments,
    /// reasoning) and the developer→system role. Ported from sanitize.go.
    pub sanitize_fingerprints: bool,
}

/// Remove a single trailing `[…]` suffix (e.g. `deepseek-v4.1-flash[1M]` →
/// `deepseek-v4.1-flash`). Returns the model unchanged if there is no such
/// suffix or the remainder would be empty.
pub(crate) fn strip_model_suffix(model: &str) -> String {
    if let Some(open) = model.rfind('[') {
        let suffix = &model[open..];
        if suffix.ends_with(']') && suffix.len() > 1 {
            let prefix = &model[..open];
            if !prefix.is_empty() {
                return prefix.to_string();
            }
        }
    }
    model.to_string()
}

pub fn translate_request(
    req: anthropic::AnthropicRequest,
    policy: &TranslationPolicy,
) -> ProxyResult<openai::OpenAIRequest> {
    let model = select_model(&req, policy);

    let mut openai_messages = Vec::new();

    if let Some(system) = req.system {
        match system {
            anthropic::SystemPrompt::Single(text) => {
                openai_messages.push(openai::Message {
                    role: "system".to_string(),
                    content: Some(openai::MessageContent::Text(sanitize_prompt(
                        text,
                        &policy.ignore_terms,
                    ))),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                });
            }
            anthropic::SystemPrompt::Multiple(messages) => {
                for msg in messages {
                    openai_messages.push(openai::Message {
                        role: "system".to_string(),
                        content: Some(openai::MessageContent::Text(sanitize_prompt(
                            msg.text,
                            &policy.ignore_terms,
                        ))),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: None,
                        name: None,
                    });
                }
            }
        }
    }

    for msg in req.messages {
        openai_messages.extend(core::translate_message(msg)?);
    }

    openai_messages = normalize_message_sequence(openai_messages);

    let tools = req.tools.and_then(|tools| {
        let mut seen = std::collections::HashSet::new();
        let filtered: Vec<_> = tools
            .into_iter()
            .filter(|t| !core::is_unsupported_tool(t))
            .filter(|t| seen.insert(t.name.clone()))
            .map(core::translate_tool)
            .collect();

        if filtered.is_empty() {
            None
        } else {
            Some(filtered)
        }
    });

    Ok(openai::OpenAIRequest {
        model,
        messages: openai_messages,
        max_tokens: Some(req.max_tokens),
        max_completion_tokens: None,
        temperature: req.temperature,
        top_p: req.top_p,
        stop: req.stop_sequences,
        stream: req.stream,
        stream_options: req.stream.and_then(|stream| {
            stream.then_some(openai::StreamOptions {
                include_usage: true,
            })
        }),
        tools,
        tool_choice: None,
        extra: serde_json::Map::new(),
    })
}

pub fn translate_response(
    resp: openai::OpenAIResponse,
    fallback_model: &str,
) -> ProxyResult<anthropic::AnthropicResponse> {
    let choice = resp
        .choices
        .first()
        .ok_or_else(|| ProxyError::Transform("No choices in response".to_string()))?;

    let mut content = Vec::new();

    if let Some(text) = &choice.message.content {
        if !text.is_empty() {
            content.push(anthropic::ResponseContent::Text {
                content_type: "text".to_string(),
                text: text.clone(),
            });
        }
    }

    if let Some(tool_calls) = &choice.message.tool_calls {
        for tool_call in tool_calls {
            let input: Value =
                serde_json::from_str(&tool_call.function.arguments).unwrap_or_else(|_| json!({}));

            content.push(anthropic::ResponseContent::ToolUse {
                content_type: "tool_use".to_string(),
                id: tool_call.id.clone(),
                name: tool_call.function.name.clone(),
                input,
            });
        }
    }

    let stop_reason = choice
        .finish_reason
        .as_ref()
        .map(|r| match r.as_str() {
            "tool_calls" => "tool_use",
            "stop" => "end_turn",
            "length" => "max_tokens",
            _ => "end_turn",
        })
        .map(String::from);

    Ok(anthropic::AnthropicResponse {
        id: resp.id.unwrap_or_else(|| "msg_proxy".to_string()),
        response_type: "message".to_string(),
        role: "assistant".to_string(),
        content,
        model: resp.model.unwrap_or_else(|| fallback_model.to_string()),
        stop_reason,
        stop_sequence: None,
        usage: anthropic::Usage {
            input_tokens: resp.usage.prompt_tokens,
            output_tokens: resp.usage.completion_tokens,
            cache_creation_input_tokens: resp.usage.cache_creation_input_tokens.unwrap_or(0),
            cache_read_input_tokens: resp
                .usage
                .cache_read_input_tokens
                .or(resp
                    .usage
                    .prompt_tokens_details
                    .as_ref()
                    .map(|d| d.cached_tokens))
                .unwrap_or(0),
        },
    })
}

pub fn translate_models_list(resp: openai::ModelsListResponse) -> anthropic::ModelsListResponse {
    let mut data: Vec<_> = resp
        .data
        .into_iter()
        .map(|model| anthropic::ModelInfo {
            created_at: "1970-01-01T00:00:00Z".to_string(),
            display_name: model.id.clone(),
            id: model.id,
            model_type: "model".to_string(),
        })
        .collect();

    // Advertise the virtual `free` model so clients can pin to it. It is not a
    // real upstream id (the gateway resolves it per request), so expose it only
    // once and only when the catalog would not already contain it.
    if !data.iter().any(|m| m.id == FREE_MODEL_NAME) {
        data.push(anthropic::ModelInfo {
            created_at: "1970-01-01T00:00:00Z".to_string(),
            display_name: FREE_MODEL_NAME.to_string(),
            id: FREE_MODEL_NAME.to_string(),
            model_type: "model".to_string(),
        });
    }

    let first_id = data.first().map(|m| m.id.clone());
    let last_id = data.last().map(|m| m.id.clone());

    anthropic::ModelsListResponse {
        data,
        first_id,
        has_more: false,
        last_id,
    }
}
