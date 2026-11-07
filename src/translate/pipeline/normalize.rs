//! Message-sequence normalization (merge adjacent same-role turns).

use super::*;

/// Merges multiple `openai::MessageContent` values into a single `MessageContent`.
///
/// If all parts are text, they are joined with double newlines (`\n\n`).
/// If image parts are present, all content parts are flattened into `MessageContent::Parts`.
/// Returns `None` if the merged parts are empty.
fn merge_message_contents(contents: Vec<openai::MessageContent>) -> Option<openai::MessageContent> {
    let mut all_parts = Vec::new();
    for content in contents {
        match content {
            openai::MessageContent::Text(t) => {
                if !t.is_empty() {
                    all_parts.push(openai::ContentPart::Text { text: t });
                }
            }
            openai::MessageContent::Parts(parts) => {
                for p in parts {
                    match p {
                        openai::ContentPart::Text { text } => {
                            if !text.is_empty() {
                                all_parts.push(openai::ContentPart::Text { text });
                            }
                        }
                        img @ openai::ContentPart::ImageUrl { .. } => {
                            all_parts.push(img);
                        }
                    }
                }
            }
        }
    }

    if all_parts.is_empty() {
        return None;
    }

    let all_text = all_parts
        .iter()
        .all(|p| matches!(p, openai::ContentPart::Text { .. }));
    if all_text {
        let joined = all_parts
            .into_iter()
            .filter_map(|p| match p {
                openai::ContentPart::Text { text } => Some(text),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n\n");
        if joined.is_empty() {
            None
        } else {
            Some(openai::MessageContent::Text(joined))
        }
    } else {
        Some(openai::MessageContent::Parts(all_parts))
    }
}

/// Normalizes an OpenAI message sequence to conform strictly to OpenAI turn-taking rules.
///
/// 1. Merges adjacent consecutive `assistant` messages into a single `assistant` message:
///    - Text contents are joined with double newlines.
///    - `reasoning_content` chunks are joined with double newlines.
///    - `tool_calls` are concatenated, stripping empty or corrupted tool calls (empty id / empty name).
/// 2. Aligns and orders following `tool` messages strictly matching the `tool_calls` order:
///    - If a tool call has no matching tool result, a synthetic stub `[result omitted]` is added
///      so strict upstreams (MiniMax 11133 / GLM 11133) do not reject incomplete tool records.
///    - Out-of-order tool results are reordered to match the tool_calls definition.
///    - Orphan tool messages (results for uncalled tools or tool messages without preceding assistant)
///      are pruned to prevent upstream 11148 ("tool calls and tool results do not match").
pub fn normalize_message_sequence(messages: Vec<openai::Message>) -> Vec<openai::Message> {
    let mut normalized = Vec::with_capacity(messages.len());
    let mut i = 0;

    while i < messages.len() {
        if messages[i].role == "assistant" {
            let mut merged_contents = Vec::new();
            let mut merged_reasoning = Vec::new();
            let mut merged_tool_calls = Vec::new();

            while i < messages.len() && messages[i].role == "assistant" {
                let curr = &messages[i];
                if let Some(content) = &curr.content {
                    merged_contents.push(content.clone());
                }
                if let Some(rc) = &curr.reasoning_content {
                    if !rc.is_empty() {
                        merged_reasoning.push(rc.clone());
                    }
                }
                if let Some(tcs) = &curr.tool_calls {
                    for tc in tcs {
                        if !tc.id.trim().is_empty() && !tc.function.name.trim().is_empty() {
                            merged_tool_calls.push(tc.clone());
                        }
                    }
                }
                i += 1;
            }

            let final_content = merge_message_contents(merged_contents);
            let final_reasoning = if merged_reasoning.is_empty() {
                None
            } else {
                Some(merged_reasoning.join("\n\n"))
            };
            let has_tools = !merged_tool_calls.is_empty();

            if final_content.is_none() && final_reasoning.is_none() && !has_tools {
                continue;
            }

            normalized.push(openai::Message {
                role: "assistant".to_string(),
                content: final_content,
                reasoning_content: final_reasoning,
                tool_calls: if has_tools {
                    Some(merged_tool_calls.clone())
                } else {
                    None
                },
                tool_call_id: None,
                name: None,
            });

            if has_tools {
                let mut tool_results_by_id: std::collections::HashMap<
                    String,
                    Vec<openai::Message>,
                > = std::collections::HashMap::new();
                while i < messages.len() && messages[i].role == "tool" {
                    if let Some(id) = &messages[i].tool_call_id {
                        let trimmed = id.trim();
                        if !trimmed.is_empty() {
                            tool_results_by_id
                                .entry(trimmed.to_string())
                                .or_default()
                                .push(messages[i].clone());
                        }
                    }
                    i += 1;
                }

                for tc in &merged_tool_calls {
                    if let Some(list) = tool_results_by_id.get_mut(&tc.id) {
                        if !list.is_empty() {
                            normalized.push(list.remove(0));
                            continue;
                        }
                    }
                    normalized.push(openai::Message {
                        role: "tool".to_string(),
                        content: Some(openai::MessageContent::Text("[result omitted]".to_string())),
                        reasoning_content: None,
                        tool_calls: None,
                        tool_call_id: Some(tc.id.clone()),
                        name: None,
                    });
                }
            } else {
                while i < messages.len() && messages[i].role == "tool" {
                    i += 1;
                }
            }
        } else {
            normalized.push(messages[i].clone());
            i += 1;
        }
    }

    normalized
}
