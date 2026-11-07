//! The non-streaming Responses API response.

use super::*;

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

    if output.is_empty() {
        let text = choice
            .message
            .content
            .clone()
            .or_else(|| choice.message.reasoning_content.clone())
            .unwrap_or_default();
        output.push(responses::OutputItem::Message {
            id: generate_id("msg"),
            status: "completed".to_string(),
            role: choice.message.role.clone(),
            content: vec![responses::OutputContentPart::OutputText { text: text.clone() }],
        });
        response_content_items.push(responses::ResponseContentItem {
            content_type: "output_text".to_string(),
            text: Some(text),
        });
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
