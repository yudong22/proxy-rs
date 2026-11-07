//! Translation for the OpenAI **Responses** API surface.
//!
//! ## File layout
//!
//! Split from one 2,300-line file into the two halves that never call each other:
//! a request translator and a response renderer, plus the streaming state machine
//! that the response half assembles.
//!
//! | file | owns |
//! |---|---|
//! | [`request`] | Responses request → OpenAI Chat request, tool/choice normalization |
//! | [`response`] | the non-streaming Responses response |
//! | [`stream`] | the streaming state machine (chunks → SSE events) |
//!
//! The state machine is the piece with the invariants worth reading: every
//! `response.output_item.added` is matched by exactly one
//! `response.output_item.done` before the next item opens, and
//! [`stream::MAX_STREAMING_TOOL_CALLS`] bounds the bookkeeping so a hostile
//! upstream cannot grow the map without limit.

use crate::error::{ProxyError, ProxyResult};
use crate::models::{openai, responses};
use crate::translate::core::normalize_schema;
use crate::translate::pipeline::{resolve_upstream_model, sanitize_prompt, TranslationPolicy};
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

mod request;
mod response;
mod stream;

pub use request::*;
pub use response::*;
pub use stream::*;

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
                    reasoning_content: None,
                    refusal: None,
                    tool_calls: None,
                },
                logprobs: None,
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
                    refusal: None,
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                },
                logprobs: None,
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

        // Only the real reason closes items, and stream completion emits Completed.
        let last = translate_stream_chunk(&mut state, &upstream_chunk("", Some("stop")));
        let types: Vec<&str> = last.iter().map(|e| e.event_type()).collect();
        assert_eq!(
            types,
            vec!["response.output_text.done", "response.output_item.done",]
        );

        match &last[0] {
            responses::ResponsesStreamEvent::OutputTextDone { text, .. } => {
                assert_eq!(text, "OK!");
            }
            other => panic!("expected OutputTextDone, got {other:?}"),
        }

        let done = translate_stream_done(&mut state);
        let done_types: Vec<&str> = done.iter().map(|e| e.event_type()).collect();
        assert_eq!(done_types, vec!["response.completed"]);
        match &done[0] {
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
                    refusal: None,
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                },
                logprobs: None,
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
                    refusal: None,
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                },
                logprobs: None,
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
    fn text_item_output_index_follows_the_counter() {
        // A model that streams a tool call *before* text gives the text item an
        // index above 0. Every text event must echo that index: a hardcoded 0
        // corrupts the client's item accounting (Codex drops the item).
        let mut state = initial_stream_state("gpt-4o".to_string());
        translate_stream_chunk(&mut state, &tool_chunk(0, Some("call_1"), Some("shell")));
        let delta_events = translate_stream_chunk(&mut state, &upstream_chunk("Done.", None));

        // The text item was assigned output_index 1 (after the tool call's 0),
        // and the delta must carry it.
        assert!(delta_events.iter().any(|e| matches!(
            e,
            responses::ResponsesStreamEvent::OutputTextDelta {
                output_index: 1,
                ..
            }
        )));

        // The close events must echo the same index.
        let done_events = translate_stream_done(&mut state);
        assert!(done_events.iter().any(|e| matches!(
            e,
            responses::ResponsesStreamEvent::OutputTextDone {
                output_index: 1,
                ..
            }
        )));
        assert!(done_events.iter().any(|e| matches!(
            e,
            responses::ResponsesStreamEvent::OutputItemDone {
                output_index: 1,
                item: responses::OutputItem::Message { .. },
                ..
            }
        )));
        // While the tool call keeps its own index 0.
        assert!(done_events.iter().any(|e| matches!(
            e,
            responses::ResponsesStreamEvent::OutputItemDone {
                output_index: 0,
                item: responses::OutputItem::FunctionCall { .. },
                ..
            }
        )));
    }

    #[test]
    fn failed_stream_does_not_also_emit_completed() {
        // `response.failed` is terminal. Without flagging it, the end-of-stream
        // path appended `response.completed` with empty output after the
        // failure, and clients honouring both closed the turn empty.
        let mut state = initial_stream_state("gpt-4o".to_string());
        let failed = translate_stream_error(&mut state, "boom".to_string());
        assert_eq!(failed.len(), 1);
        assert_eq!(failed[0].event_type(), "response.failed");

        let after = translate_stream_done(&mut state);
        assert!(
            after.is_empty(),
            "nothing may follow response.failed: {after:?}"
        );
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

    /// A tool-call `index` arrives on the upstream frame and used to drive an
    /// unbounded `while len <= index { push }` loop: one frame with a huge
    /// index allocated until the process died (fatal, since release builds use
    /// `panic = "abort"`). Out-of-range indices are now skipped.
    #[test]
    fn a_hostile_tool_call_index_does_not_grow_the_tracker() {
        let mut state = initial_stream_state("m".to_string());
        let chunk: openai::StreamChunk = serde_json::from_str(
            &json!({
                "id": "c1",
                "object": "chat.completion.chunk",
                "created": 0i64,
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": usize::MAX,
                            "id": "call_hostile",
                            "type": "function",
                            "function": { "name": "f", "arguments": "{}" }
                        }]
                    }
                }]
            })
            .to_string(),
        )
        .expect("fixture parses");

        // Must not hang, allocate, or panic.
        translate_stream_chunk(&mut state, &chunk);
        assert!(
            state.active_tool_calls.is_empty(),
            "a tool call above the cap must be ignored, got {}",
            state.active_tool_calls.len()
        );
    }

    /// ...but an ordinary index still works, so the guard did not disable the
    /// feature it protects.
    #[test]
    fn a_normal_tool_call_index_is_still_tracked() {
        let mut state = initial_stream_state("m".to_string());
        let chunk: openai::StreamChunk = serde_json::from_str(
            &json!({
                "id": "c1",
                "object": "chat.completion.chunk",
                "created": 0i64,
                "model": "m",
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call_ok",
                            "type": "function",
                            "function": { "name": "f", "arguments": "{}" }
                        }]
                    }
                }]
            })
            .to_string(),
        )
        .expect("fixture parses");

        translate_stream_chunk(&mut state, &chunk);
        assert_eq!(state.active_tool_calls.len(), 1);
        assert_eq!(state.active_tool_calls[0].call_id, "call_ok");
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

    #[test]
    fn trailing_usage_chunk_is_captured_into_completed() {
        let mut state = initial_stream_state("gpt-4o".to_string());

        // Chunk 1: text delta
        let chunk1 = openai::StreamChunk {
            id: Some("c1".into()),
            object: Some("chat.completion.chunk".into()),
            created: Some(100),
            model: Some("gpt-4o".into()),
            choices: vec![openai::StreamChoice {
                index: 0,
                delta: openai::Delta {
                    role: Some("assistant".into()),
                    content: Some("answer".into()),
                    refusal: None,
                    tool_calls: None,
                    reasoning: None,
                    reasoning_content: None,
                },
                logprobs: None,
                finish_reason: None,
            }],
            usage: None,
        };
        translate_stream_chunk(&mut state, &chunk1);

        // Chunk 2: finish_reason without usage (standard OpenAI behavior)
        let chunk2 = openai::StreamChunk {
            id: Some("c2".into()),
            object: Some("chat.completion.chunk".into()),
            created: Some(101),
            model: Some("gpt-4o".into()),
            choices: vec![openai::StreamChoice {
                index: 0,
                delta: openai::Delta::default(),
                logprobs: None,
                finish_reason: Some("stop".into()),
            }],
            usage: None,
        };
        let events2 = translate_stream_chunk(&mut state, &chunk2);
        let types2: Vec<&str> = events2.iter().map(|e| e.event_type()).collect();
        assert_eq!(
            types2,
            vec!["response.output_text.done", "response.output_item.done"]
        );

        // Chunk 3: trailing usage chunk with empty choices
        let chunk3 = openai::StreamChunk {
            id: Some("c3".into()),
            object: Some("chat.completion.chunk".into()),
            created: Some(102),
            model: Some("gpt-4o".into()),
            choices: vec![],
            usage: Some(openai::Usage {
                prompt_tokens: 15,
                completion_tokens: 7,
                total_tokens: 22,
                ..Default::default()
            }),
        };
        let events3 = translate_stream_chunk(&mut state, &chunk3);
        let types3: Vec<&str> = events3.iter().map(|e| e.event_type()).collect();
        assert_eq!(types3, vec!["response.completed"]);

        match &events3[0] {
            responses::ResponsesStreamEvent::Completed { response } => {
                let u = response.usage.as_ref().expect("usage must be present");
                assert_eq!(u.input_tokens, 15);
                assert_eq!(u.output_tokens, 7);
                assert_eq!(u.total_tokens, 22);
            }
            other => panic!("expected Completed event, got {other:?}"),
        }
    }

    #[test]
    fn tool_call_stream_delays_output_item_added_until_name_is_known() {
        let mut state = initial_stream_state("gpt-4o".to_string());

        // Chunk 1: ID only, function name empty
        let chunk1 = openai::StreamChunk {
            id: Some("c1".into()),
            object: Some("chat.completion.chunk".into()),
            created: Some(100),
            model: Some("gpt-4o".into()),
            choices: vec![openai::StreamChoice {
                index: 0,
                delta: openai::Delta {
                    role: Some("assistant".into()),
                    content: None,
                    refusal: None,
                    tool_calls: Some(vec![openai::DeltaToolCall {
                        index: 0,
                        id: Some("call_abc123".into()),
                        call_type: Some("function".into()),
                        function: None,
                    }]),
                    reasoning: None,
                    reasoning_content: None,
                },
                logprobs: None,
                finish_reason: None,
            }],
            usage: None,
        };
        let events1 = translate_stream_chunk(&mut state, &chunk1);
        let types1: Vec<&str> = events1.iter().map(|e| e.event_type()).collect();
        assert!(
            !types1.contains(&"response.output_item.added"),
            "must not emit output_item.added when name is empty"
        );

        // Chunk 2: Name arrives
        let chunk2 = openai::StreamChunk {
            id: Some("c2".into()),
            object: Some("chat.completion.chunk".into()),
            created: Some(101),
            model: Some("gpt-4o".into()),
            choices: vec![openai::StreamChoice {
                index: 0,
                delta: openai::Delta {
                    role: None,
                    content: None,
                    refusal: None,
                    tool_calls: Some(vec![openai::DeltaToolCall {
                        index: 0,
                        id: None,
                        call_type: None,
                        function: Some(openai::DeltaFunctionCall {
                            name: Some("apply_patch".into()),
                            arguments: None,
                        }),
                    }]),
                    reasoning: None,
                    reasoning_content: None,
                },
                logprobs: None,
                finish_reason: None,
            }],
            usage: None,
        };
        let events2 = translate_stream_chunk(&mut state, &chunk2);
        let types2: Vec<&str> = events2.iter().map(|e| e.event_type()).collect();
        assert!(
            types2.contains(&"response.output_item.added"),
            "must emit output_item.added now that name is known"
        );
        match &events2[0] {
            responses::ResponsesStreamEvent::OutputItemAdded { item, .. } => match item {
                responses::OutputItem::FunctionCall { id, name, .. } => {
                    assert_eq!(id, "call_abc123");
                    assert_eq!(name, "apply_patch");
                }
                other => panic!("expected FunctionCall, got {other:?}"),
            },
            other => panic!("expected OutputItemAdded, got {other:?}"),
        }
    }
}
