//! Config-aware composition of the pure translation atoms.
//!
//! ## File layout
//!
//! Split from one 2,500-line file. The 3.3.0 audit flagged this module as the
//! densest concentration of *tests* in the codebase (1,601 test lines against
//! 920 production lines), which is exactly why the split is by concern and why
//! the test module stayed in one piece: the tests exercise behaviour that spans
//! all four files, and scattering them would make a change harder to verify, not
//! easier.
//!
//! | file | owns |
//! |---|---|
//! | [`policy`] | `TranslationPolicy` and the three translate entry points |
//! | [`routing`] | upstream model selection and the free-tier fallbacks |
//! | [`sanitize`] | fingerprint / harness-identity scrubbing of outbound text |
//! | [`normalize`] | message-sequence normalization |
//!
//! Every historical `crate::translate::pipeline::X` path still resolves: the
//! submodules are re-exported below.

use crate::error::{ProxyError, ProxyResult};
use crate::models::{anthropic, openai};
use crate::translate::core;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::OnceLock;

mod normalize;
mod policy;
mod routing;
mod sanitize;

pub use normalize::*;
pub use policy::*;
pub use routing::*;
pub(crate) use sanitize::*;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use serde_json::json;

    fn policy_from(config: &Config) -> TranslationPolicy {
        TranslationPolicy {
            reasoning_model: config.reasoning_model.clone(),
            completion_model: config.completion_model.clone(),
            model_map: config.model_map.clone(),
            ignore_terms: config.system_prompt_ignore_terms.clone(),
            strip_model_suffix: false,
            sanitize_fingerprints: true,
        }
    }

    fn default_policy() -> TranslationPolicy {
        policy_from(&Config::default())
    }

    // ---- model resolution ---------------------------------------------------

    fn policy_with(
        completion: Option<&str>,
        reasoning: Option<&str>,
        map: &[(&str, &str)],
        strip_suffix: bool,
    ) -> TranslationPolicy {
        TranslationPolicy {
            reasoning_model: reasoning.map(str::to_string),
            completion_model: completion.map(str::to_string),
            model_map: map
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            ignore_terms: Vec::new(),
            strip_model_suffix: strip_suffix,
            sanitize_fingerprints: false,
        }
    }

    #[test]
    fn configured_model_outranks_the_client_model() {
        // The configured model exists to override what the client sends.
        let p = policy_with(Some("gpt-5"), None, &[], false);
        assert_eq!(
            resolve_upstream_model("sonnet", p.completion_model.as_ref(), &p),
            "gpt-5"
        );
    }

    #[test]
    fn the_client_model_is_used_when_nothing_is_configured() {
        let p = policy_with(None, None, &[], false);
        assert_eq!(resolve_upstream_model("sonnet", None, &p), "sonnet");
    }

    #[test]
    fn model_map_applies_to_the_client_model_when_none_is_configured() {
        let p = policy_with(None, None, &[("sonnet", "glm-5.3")], false);
        assert_eq!(resolve_upstream_model("sonnet", None, &p), "glm-5.3");
    }

    #[test]
    fn the_configured_model_is_itself_mapped() {
        // `model_map` is consulted after the override, so a mapping keyed on
        // the configured model still applies.
        let p = policy_with(Some("gpt-5"), None, &[("gpt-5", "glm-5.3")], false);
        assert_eq!(
            resolve_upstream_model("sonnet", p.completion_model.as_ref(), &p),
            "glm-5.3"
        );
    }

    #[test]
    fn a_mapping_keyed_on_the_client_model_does_not_survive_an_override() {
        // Documents the consequence of that order: once a configured model
        // replaces the client's, a mapping named after the client's model is
        // no longer consulted. Pinned because it is easy to read as a bug.
        let p = policy_with(Some("gpt-5"), None, &[("sonnet", "glm-5.3")], false);
        assert_eq!(
            resolve_upstream_model("sonnet", p.completion_model.as_ref(), &p),
            "gpt-5"
        );
    }

    #[test]
    fn the_suffix_is_stripped_only_when_configured() {
        let off = policy_with(None, None, &[], false);
        assert_eq!(resolve_upstream_model("m[1M]", None, &off), "m[1M]");

        let on = policy_with(None, None, &[], true);
        assert_eq!(resolve_upstream_model("m[1M]", None, &on), "m");
    }

    // ---- virtual `free` model -------------------------------------------------

    #[test]
    fn free_model_resolves_to_a_concrete_node_never_itself() {
        // `free` is virtual: it must be rewritten to a real upstream node
        // (hy3 by day, hy4-preview by night) and must never reach upstream
        // verbatim, regardless of any configured override.
        let p = default_policy();
        let resolved = resolve_upstream_model(FREE_MODEL_NAME, Some(&"override".to_string()), &p);
        assert_ne!(resolved, FREE_MODEL_NAME);
        assert_eq!(resolved, resolve_free_model());
    }

    #[test]
    fn night_window_wraps_past_midnight() {
        // Default free schedule: local 23:00–08:00.
        assert!(is_night_hour(23, 23, 8));
        assert!(is_night_hour(0, 23, 8));
        assert!(is_night_hour(7, 23, 8));
        assert!(!is_night_hour(8, 23, 8));
        assert!(!is_night_hour(22, 23, 8));
        assert!(!is_night_hour(12, 23, 8));
    }

    #[test]
    fn non_wrapping_window() {
        assert!(is_night_hour(2, 0, 8));
        assert!(!is_night_hour(8, 0, 8));
        assert!(!is_night_hour(23, 0, 8));
    }

    #[test]
    fn free_node_accessors_default_and_overridable() {
        // Defaults: night node is hy4-preview, degraded (daytime) node is hy3,
        // and the degrade target must differ from the night node so the
        // night→day fallback actually switches models.
        let night = free_night_model();
        let day = free_degraded_model();
        assert_eq!(night, "hy4-preview");
        assert_eq!(day, "hy3");
        assert_ne!(night, day);

        // The degradation helpers read the same env knobs as resolve_free_model,
        // so overriding the day/night nodes stays consistent end-to-end.
        std::env::set_var("PROXY_FREE_NIGHT_MODEL", "hy4");
        std::env::set_var("PROXY_FREE_DAY_MODEL", "hy3-pro");
        assert_eq!(free_night_model(), "hy4");
        assert_eq!(free_degraded_model(), "hy3-pro");
        assert_ne!(free_night_model(), free_degraded_model());
        std::env::remove_var("PROXY_FREE_NIGHT_MODEL");
        std::env::remove_var("PROXY_FREE_DAY_MODEL");
    }

    fn req_with_model(model: &str, extra: Value) -> anthropic::AnthropicRequest {
        anthropic::AnthropicRequest {
            model: model.to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("pong".to_string()),
            }],
            max_tokens: 64,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(false),
            tools: None,
            metadata: None,
            extra,
        }
    }

    #[test]
    fn a_thinking_request_resolves_against_the_reasoning_model() {
        // `select_model` picks which configured model applies before delegating.
        let p = policy_with(Some("completion"), Some("reasoning"), &[], false);
        let thinking = req_with_model("sonnet", json!({ "thinking": { "type": "enabled" } }));
        assert_eq!(select_model(&thinking, &p), "reasoning");

        let plain = req_with_model("sonnet", json!({ "thinking": { "type": "disabled" } }));
        assert_eq!(select_model(&plain, &p), "completion");
    }

    #[test]
    fn every_route_resolves_the_same_client_model_identically() {
        // The regression this guards: the Chat and Responses paths used to map
        // *first* and fall back to `completion_model` second, so the same
        // request reached a different upstream model per endpoint.
        let p = policy_with(Some("gpt-5"), None, &[("sonnet", "glm-5.3")], false);

        // What the Anthropic route produces (via select_model).
        let anthropic_route = select_model(&req_with_model("sonnet", json!({})), &p);

        // What the Chat and Responses routes produce (same shared function).
        let other_routes = resolve_upstream_model("sonnet", p.completion_model.as_ref(), &p);

        assert_eq!(
            anthropic_route, other_routes,
            "one client model must not resolve differently per route"
        );
    }

    // ---- fingerprint sanitization ------------------------------------------

    #[test]
    fn sanitize_rewrites_identity_sentence() {
        let in_ = "You are Claude Code, Anthropic's official CLI for Claude";
        let out = sanitize_text(in_);
        assert!(out.contains("official CLI tool for Claude"));
        assert!(!out.contains("official CLI for Claude"));
    }

    #[test]
    fn sanitize_rewrites_main_branch() {
        let out = sanitize_text("Main branch (you will usually use this for PRs)");
        assert!(out.contains("Default branch"));
        assert!(!out.contains("Main branch"));
    }

    #[test]
    fn sanitize_rewrites_feedback_sentence() {
        let in_ = "To give feedback, users should report the issue at https://github.com/anthropics/claude-code/issues";
        let out = sanitize_text(in_);
        assert!(out.contains("To provide feedback"));
        assert!(!out.contains("To give feedback"));
    }

    #[test]
    fn sanitize_breaks_11128_with_hyphen() {
        let out = sanitize_text("the upstream returned code 11128");
        assert!(out.contains("11-128"));
        assert!(!out.contains("11128"));
    }

    #[test]
    fn sanitize_strips_billing_header_kv() {
        let in_ = "x-anthropic-billing-header: abc123; carry on";
        let out = sanitize_text(in_);
        assert!(!out.contains("x-anthropic-billing-header: abc123"));
        assert!(out.contains("carry on"));
    }

    #[test]
    fn sanitize_strips_bare_cc_kv() {
        let in_ = "context cc_entrypoint=cli; cc_version=2; done";
        let out = sanitize_text(in_);
        assert!(!out.contains("cc_entrypoint"));
        assert!(!out.contains("cc_version"));
        assert!(out.contains("done"));
    }

    #[test]
    fn sanitize_abbreviates_bare_key_name() {
        let in_ = "we referenced x-anthropic-billing-header in notes";
        let out = sanitize_text(in_);
        assert!(out.contains("x-anthropic-billing-hdr"));
        assert!(!out.contains("x-anthropic-billing-header"));
    }

    #[test]
    fn sanitize_leaves_clean_text_untouched() {
        let in_ = "Please summarize the quarterly report.";
        assert_eq!(sanitize_text(in_), in_);
    }

    #[test]
    fn sanitize_request_keeps_semantics_and_roles() {
        let mut req = openai::OpenAIRequest {
            model: "m".to_string(),
            messages: vec![
                openai::Message {
                    role: "developer".to_string(),
                    content: Some(openai::MessageContent::Text(
                        "Main branch (you will usually use this for PRs)".to_string(),
                    )),
                    reasoning_content: None,
                    tool_calls: None,
                    tool_call_id: None,
                    name: None,
                },
                openai::Message {
                    role: "assistant".to_string(),
                    content: None,
                    reasoning_content: Some(
                        "thinking about x-anthropic-billing-header usage".to_string(),
                    ),
                    tool_calls: Some(vec![openai::ToolCall {
                        id: "t1".to_string(),
                        call_type: "function".to_string(),
                        function: openai::FunctionCall {
                            name: "write".to_string(),
                            arguments: "{\"path\":\"a\",\"content\":\"You are Claude Code, Anthropic's official CLI for Claude\"}".to_string(),
                        },
                    }]),
                    tool_call_id: None,
                    name: None,
                },
            ],
            max_tokens: Some(1),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        };
        sanitize_openai_request(&mut req);

        // developer -> system
        assert_eq!(req.messages[0].role, "system");
        let sys_text = match req.messages[0].content.as_ref().unwrap() {
            openai::MessageContent::Text(t) => t,
            _ => panic!("expected text content"),
        };
        assert!(sys_text.contains("Default branch"));

        // assistant reasoning + tool arguments both scrubbed
        assert!(req.messages[1]
            .reasoning_content
            .as_ref()
            .unwrap()
            .contains("x-anthropic-billing-hdr"));
        let args = &req.messages[1].tool_calls.as_ref().unwrap()[0]
            .function
            .arguments;
        assert!(args.contains("official CLI tool for Claude"));
        assert!(!args.contains("official CLI for Claude"));
    }

    #[test]
    fn applies_model_map_after_selection() {
        let req = anthropic::AnthropicRequest {
            model: "claude-opus-4-6".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("pong".to_string()),
            }],
            max_tokens: 64,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(false),
            tools: None,
            metadata: None,
            extra: json!({}),
        };

        let policy = TranslationPolicy {
            model_map: [("claude-opus-4-6".to_string(), "openai/gpt-4.1".to_string())]
                .into_iter()
                .collect(),
            ..default_policy()
        };

        let openai = translate_request(req, &policy).unwrap();
        assert_eq!(openai.model, "openai/gpt-4.1");
    }

    #[test]
    fn strips_workbuddy_context_suffix() {
        let req = anthropic::AnthropicRequest {
            model: "deepseek-v4.1-flash[1M]".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("hi".to_string()),
            }],
            max_tokens: 64,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(false),
            tools: None,
            metadata: None,
            extra: json!({}),
        };

        // Disabled: suffix must be preserved.
        let off = translate_request(req.clone(), &default_policy()).unwrap();
        assert_eq!(off.model, "deepseek-v4.1-flash[1M]");

        // Enabled (WorkBuddy flavor): trailing [1M] dropped.
        let on = TranslationPolicy {
            strip_model_suffix: true,
            ..default_policy()
        };
        let openai = translate_request(req, &on).unwrap();
        assert_eq!(openai.model, "deepseek-v4.1-flash");
    }

    #[test]
    fn sanitizes_configured_system_prompt_terms() {
        let req = anthropic::AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("pong".to_string()),
            }],
            max_tokens: 64,
            system: Some(anthropic::SystemPrompt::Single(
                "Examples of risky actions: rm -rf.".to_string(),
            )),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
            metadata: None,
            extra: json!({}),
        };

        let policy = TranslationPolicy {
            ignore_terms: vec!["rm -rf".to_string()],
            ..default_policy()
        };

        let openai = translate_request(req, &policy).unwrap();

        match &openai.messages[0].content {
            Some(openai::MessageContent::Text(text)) => {
                assert_eq!(text, "Examples of risky actions: .");
            }
            _ => panic!("expected sanitized system prompt"),
        }
    }

    #[test]
    fn streaming_request_includes_usage_stream_options() {
        let req = anthropic::AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("hi".to_string()),
            }],
            max_tokens: 100,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(true),
            tools: None,
            metadata: None,
            extra: json!({}),
        };

        let openai = translate_request(req, &default_policy()).unwrap();

        assert_eq!(
            openai.stream_options.map(|options| options.include_usage),
            Some(true)
        );
    }

    #[test]
    fn non_streaming_request_omits_usage_stream_options() {
        let req = anthropic::AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("hi".to_string()),
            }],
            max_tokens: 100,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: Some(false),
            tools: None,
            metadata: None,
            extra: json!({}),
        };

        let openai = translate_request(req, &default_policy()).unwrap();

        assert!(openai.stream_options.is_none());
    }

    #[test]
    fn converts_tool_definitions() {
        let req = anthropic::AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("use tool".to_string()),
            }],
            max_tokens: 100,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: Some(vec![anthropic::Tool {
                name: "read_file".to_string(),
                description: Some("Read a file".to_string()),
                input_schema: Some(json!({
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                })),
                tool_type: None,
                extra: Default::default(),
            }]),
            metadata: None,
            extra: json!({}),
        };

        let openai = translate_request(req, &default_policy()).unwrap();

        let tools = openai.tools.unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].tool_type, "function");
        assert_eq!(tools[0].function.name, "read_file");
    }

    #[test]
    fn filters_batch_tools() {
        let req = anthropic::AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("hi".to_string()),
            }],
            max_tokens: 100,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: Some(vec![anthropic::Tool {
                name: "batch_tool".to_string(),
                description: None,
                input_schema: Some(json!({})),
                tool_type: Some("BatchTool".to_string()),
                extra: Default::default(),
            }]),
            metadata: None,
            extra: json!({}),
        };

        let openai = translate_request(req, &default_policy()).unwrap();
        assert!(openai.tools.is_none());
    }

    #[test]
    fn converts_image_content() {
        let req = anthropic::AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Blocks(vec![
                    anthropic::ContentBlock::Text {
                        text: "What is this?".to_string(),
                        cache_control: None,
                    },
                    anthropic::ContentBlock::Image {
                        source: anthropic::ImageSource {
                            source_type: "base64".to_string(),
                            media_type: "image/png".to_string(),
                            data: "iVBOR...".to_string(),
                        },
                    },
                ]),
            }],
            max_tokens: 100,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            metadata: None,
            extra: json!({}),
        };

        let openai = translate_request(req, &default_policy()).unwrap();

        match &openai.messages[0].content {
            Some(openai::MessageContent::Parts(parts)) => {
                assert_eq!(parts.len(), 2);
                match &parts[1] {
                    openai::ContentPart::ImageUrl { image_url } => {
                        assert!(image_url.url.starts_with("data:image/png;base64,"));
                    }
                    _ => panic!("expected image_url part"),
                }
            }
            _ => panic!("expected multi-part content"),
        }
    }

    #[test]
    fn converts_tool_use_and_tool_result() {
        let req = anthropic::AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![
                anthropic::Message {
                    role: "assistant".to_string(),
                    content: anthropic::MessageContent::Blocks(vec![
                        anthropic::ContentBlock::ToolUse {
                            id: "tool_1".to_string(),
                            name: "read_file".to_string(),
                            input: json!({"path": "/tmp"}),
                        },
                    ]),
                },
                anthropic::Message {
                    role: "user".to_string(),
                    content: anthropic::MessageContent::Blocks(vec![
                        anthropic::ContentBlock::ToolResult {
                            tool_use_id: "tool_1".to_string(),
                            content: anthropic::ToolResultContent::Text(
                                "file contents".to_string(),
                            ),
                            is_error: None,
                        },
                    ]),
                },
            ],
            max_tokens: 100,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            metadata: None,
            extra: json!({}),
        };

        let openai = translate_request(req, &default_policy()).unwrap();

        let tool_calls = openai.messages[0].tool_calls.as_ref().unwrap();
        assert_eq!(tool_calls[0].id, "tool_1");
        assert_eq!(tool_calls[0].function.name, "read_file");

        assert_eq!(openai.messages[1].role, "tool");
        assert_eq!(openai.messages[1].tool_call_id, Some("tool_1".to_string()));
    }

    #[test]
    fn deserializes_tool_result_with_nested_content_blocks() {
        let body = json!({
            "model": "gpt-4o",
            "max_tokens": 100,
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "tool_42",
                    "content": [
                        {"type": "text", "text": "first chunk"},
                        {"type": "text", "text": "second chunk"}
                    ]
                }]
            }]
        });

        let req: anthropic::AnthropicRequest = serde_json::from_value(body).unwrap();
        let openai = translate_request(req, &default_policy()).unwrap();

        let tool_msg = openai
            .messages
            .iter()
            .find(|m| m.role == "tool")
            .expect("expected a tool message");
        assert_eq!(tool_msg.tool_call_id, Some("tool_42".to_string()));
        match &tool_msg.content {
            Some(openai::MessageContent::Text(text)) => {
                assert_eq!(text, "first chunk\nsecond chunk");
            }
            other => panic!("expected flattened text content, got {:?}", other),
        }
    }

    #[test]
    fn converts_multiple_system_prompts() {
        let req = anthropic::AnthropicRequest {
            model: "gpt-4o".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("hi".to_string()),
            }],
            max_tokens: 100,
            system: Some(anthropic::SystemPrompt::Multiple(vec![
                anthropic::SystemMessage {
                    message_type: "text".to_string(),
                    text: "You are helpful.".to_string(),
                    cache_control: None,
                },
                anthropic::SystemMessage {
                    message_type: "text".to_string(),
                    text: "Be concise.".to_string(),
                    cache_control: None,
                },
            ])),
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            metadata: None,
            extra: json!({}),
        };

        let openai = translate_request(req, &default_policy()).unwrap();

        let system_msgs: Vec<_> = openai
            .messages
            .iter()
            .filter(|m| m.role == "system")
            .collect();
        assert_eq!(system_msgs.len(), 2);
    }

    #[test]
    fn uses_reasoning_model_when_thinking_enabled() {
        let req = anthropic::AnthropicRequest {
            model: "claude-opus-4-6".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("think hard".to_string()),
            }],
            max_tokens: 100,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            metadata: None,
            extra: json!({"thinking": {"type": "enabled", "budget_tokens": 1000}}),
        };

        let policy = TranslationPolicy {
            reasoning_model: Some("gpt-4o-reasoning".to_string()),
            completion_model: Some("gpt-4o-mini".to_string()),
            ..default_policy()
        };

        let openai = translate_request(req, &policy).unwrap();
        assert_eq!(openai.model, "gpt-4o-reasoning");
    }

    #[test]
    fn uses_completion_model_without_thinking() {
        let req = anthropic::AnthropicRequest {
            model: "claude-opus-4-6".to_string(),
            messages: vec![anthropic::Message {
                role: "user".to_string(),
                content: anthropic::MessageContent::Text("quick".to_string()),
            }],
            max_tokens: 100,
            system: None,
            temperature: None,
            top_p: None,
            top_k: None,
            stop_sequences: None,
            stream: None,
            tools: None,
            metadata: None,
            extra: json!({}),
        };

        let policy = TranslationPolicy {
            reasoning_model: Some("gpt-4o-reasoning".to_string()),
            completion_model: Some("gpt-4o-mini".to_string()),
            ..default_policy()
        };

        let openai = translate_request(req, &policy).unwrap();
        assert_eq!(openai.model, "gpt-4o-mini");
    }

    #[test]
    fn response_with_all_fields_present() {
        let response = openai::OpenAIResponse {
            id: Some("chatcmpl-abc123".to_string()),
            object: Some("chat.completion".to_string()),
            created: Some(1700000000),
            model: Some("gpt-4o".to_string()),
            choices: vec![openai::Choice {
                index: 0,
                message: openai::ChoiceMessage {
                    role: "assistant".to_string(),
                    content: Some("hello".to_string()),
                    reasoning_content: None,
                    refusal: None,
                    tool_calls: None,
                },
                logprobs: None,
                finish_reason: Some("stop".to_string()),
            }],
            usage: openai::Usage {
                prompt_tokens: 5,
                completion_tokens: 1,
                total_tokens: 6,
                prompt_tokens_details: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                ..Default::default()
            },
            system_fingerprint: None,
        };

        let anthropic = translate_response(response, "fallback-model").unwrap();
        assert_eq!(anthropic.id, "chatcmpl-abc123");
        assert_eq!(anthropic.model, "gpt-4o");
    }

    #[test]
    fn response_allows_missing_metadata() {
        let response = openai::OpenAIResponse {
            id: None,
            object: None,
            created: None,
            model: None,
            choices: vec![openai::Choice {
                index: 0,
                message: openai::ChoiceMessage {
                    role: "assistant".to_string(),
                    content: Some("pong".to_string()),
                    reasoning_content: None,
                    refusal: None,
                    tool_calls: None,
                },
                logprobs: None,
                finish_reason: Some("stop".to_string()),
            }],
            usage: openai::Usage {
                prompt_tokens: 10,
                completion_tokens: 2,
                total_tokens: 12,
                prompt_tokens_details: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                ..Default::default()
            },
            system_fingerprint: None,
        };

        let anthropic = translate_response(response, "openai/gpt-4o-mini").unwrap();
        assert_eq!(anthropic.id, "msg_proxy");
        assert_eq!(anthropic.model, "openai/gpt-4o-mini");
    }

    #[test]
    fn response_converts_tool_calls() {
        let response = openai::OpenAIResponse {
            id: Some("chatcmpl-1".to_string()),
            object: None,
            created: None,
            model: Some("gpt-4o".to_string()),
            choices: vec![openai::Choice {
                index: 0,
                message: openai::ChoiceMessage {
                    role: "assistant".to_string(),
                    content: None,
                    reasoning_content: None,
                    refusal: None,
                    tool_calls: Some(vec![openai::ToolCall {
                        id: "call_abc".to_string(),
                        call_type: "function".to_string(),
                        function: openai::FunctionCall {
                            name: "read_file".to_string(),
                            arguments: "{\"path\":\"/tmp\"}".to_string(),
                        },
                    }]),
                },
                logprobs: None,
                finish_reason: Some("tool_calls".to_string()),
            }],
            usage: openai::Usage {
                prompt_tokens: 10,
                completion_tokens: 5,
                total_tokens: 15,
                prompt_tokens_details: None,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                ..Default::default()
            },
            system_fingerprint: None,
        };

        let anthropic = translate_response(response, "fallback").unwrap();
        assert_eq!(anthropic.stop_reason, Some("tool_use".to_string()));
        assert!(!anthropic.content.is_empty());
    }

    #[test]
    fn models_list_translation() {
        let response = openai::ModelsListResponse {
            object: Some("list".to_string()),
            data: vec![
                openai::ModelInfo {
                    id: "gpt-4o-mini".to_string(),
                    object: Some("model".to_string()),
                    created: None,
                    owned_by: Some("azure".to_string()),
                },
                openai::ModelInfo {
                    id: "gpt-5-chat".to_string(),
                    object: Some("model".to_string()),
                    created: None,
                    owned_by: Some("azure".to_string()),
                },
            ],
        };

        let result = translate_models_list(response);
        assert_eq!(result.first_id.as_deref(), Some("gpt-4o-mini"));
        // The virtual `free` model is appended after the upstream catalog.
        assert_eq!(result.last_id.as_deref(), Some(FREE_MODEL_NAME));
        assert!(!result.has_more);
    }

    #[test]
    fn empty_models_list() {
        let response = openai::ModelsListResponse {
            object: Some("list".to_string()),
            data: vec![],
        };
        let result = translate_models_list(response);
        // Even an empty upstream catalog advertises the virtual `free` model.
        assert_eq!(result.data.len(), 1);
        assert_eq!(result.data[0].id, FREE_MODEL_NAME);
        assert_eq!(result.first_id.as_deref(), Some(FREE_MODEL_NAME));
    }
    // ---- DeepSeek Harness identity neutralization --------------------------
    //
    // Each test pins one of the facts established against the live gateway on
    // 2026-10-25 (see the block comment beside `neutralize_harness_identity`), so
    // a future "simplification" cannot silently reintroduce a failed round-trip
    // per request or the user-data corruption of the reverted attempts.

    const DSH_OPENER: &str = "You are an AI agent powered by DeepSeek Harness.";
    const DSH_WEB_GUI: &str = "You are interacting with the user through the DeepSeek Harness Web GUI whose URL the runtime context names.";
    /// The `@`-reference pair exactly as DSH 0.2.2-alpha.2 renders it. The
    /// rewrite touches only the second sentence; the opener is byte-identical
    /// afterwards, which the test below pins.
    const DSH_REFERENCE_PAIR: &str = "Tokens prefixed with @ are paths the user explicitly referenced. Relative paths resolve from the workspace root; absolute paths identify files or directories on the host.";

    fn system_msg(text: &str) -> openai::Message {
        openai::Message {
            role: "system".to_string(),
            content: Some(openai::MessageContent::Text(text.to_string())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    /// A system message whose content is the parts array an OpenAI SDK sends for
    /// the same text. The gateway inspects the concatenated text, so this must
    /// be sanitized exactly like the plain-string form.
    fn parts_system_msg(text: &str) -> openai::Message {
        openai::Message {
            role: "system".to_string(),
            content: Some(openai::MessageContent::Parts(vec![
                openai::ContentPart::Text {
                    text: text.to_string(),
                },
            ])),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    fn text_msg(role: &str, text: &str) -> openai::Message {
        openai::Message {
            role: role.to_string(),
            content: Some(openai::MessageContent::Text(text.to_string())),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }
    }

    fn req_with(messages: Vec<openai::Message>) -> openai::OpenAIRequest {
        openai::OpenAIRequest {
            model: "deepseek-v4.1-flash".to_string(),
            messages,
            max_tokens: Some(64),
            max_completion_tokens: None,
            temperature: None,
            top_p: None,
            stop: None,
            stream: Some(false),
            stream_options: None,
            tools: None,
            tool_choice: None,
            extra: serde_json::Map::new(),
        }
    }

    /// The gateway rejects the identity opener; it must not survive verbatim.
    #[test]
    fn rewrites_harness_identity_opener() {
        let out = neutralize_harness_identity(DSH_OPENER).expect("opener must be rewritten");
        assert!(!out.contains("DeepSeek Harness"));
        assert_eq!(out, "You are an AI agent.");
    }

    /// The gateway also rejects the web-surface phrase. Fixing only one of the
    /// two literals still returned 400 live, so both must be covered.
    #[test]
    fn rewrites_harness_web_gui_phrase() {
        let out =
            neutralize_harness_identity(DSH_WEB_GUI).expect("web GUI phrase must be rewritten");
        assert!(!out.contains("DeepSeek Harness"));
        // The surrounding guidance is what actually shapes behaviour; it stays.
        assert!(out.contains("You are interacting with the user through the web GUI"));
        assert!(out.contains("whose URL the runtime context names."));
    }

    /// Both literals in one system prompt: one pass neutralizes both, which is
    /// the condition the real capture needed.
    #[test]
    fn rewrites_both_literals_in_one_prompt() {
        let prompt = format!("{DSH_OPENER}\n\n{DSH_WEB_GUI}");
        let out = neutralize_harness_identity(&prompt).expect("both literals present");
        assert!(
            !out.contains("DeepSeek Harness"),
            "no literal may survive: {out}"
        );
    }

    /// Bare product-name mentions are served by the gateway, so rewriting them
    /// would only be gratuitous prompt damage.
    #[test]
    fn leaves_bare_product_name_alone() {
        let text =
            "The DeepSeek Harness implementation checkout is /Users/denis/Sites/deepseek-harness.";
        assert_eq!(neutralize_harness_identity(text), None);
    }

    // ---- the `@`-reference fingerprint (v2.4.1) ---------------------------
    //
    // Measured live against the real gateway on 2026-11-05 with DSH
    // 0.2.2-alpha.2, after the two identity literals above had stopped blocking
    // on their own. Each test below pins one of those measurements so a future
    // "tidy-up" cannot drop the row and silently restore a failed round-trip on
    // every single request.

    /// The pair is what blocks; rewriting the second sentence breaks it.
    #[test]
    fn rewrites_reference_pair() {
        let out = neutralize_harness_identity(DSH_REFERENCE_PAIR).expect("pair must be rewritten");
        assert!(!out.contains("Relative paths resolve from the workspace root"));
        assert!(out.contains("Relative paths resolve against the workspace root"));
        // The opener is the model-facing rule; it must survive byte-identically.
        assert!(out.contains("Tokens prefixed with @ are paths the user explicitly referenced."));
    }

    /// Rewriting the second sentence alone is enough: one row, one edit, and the
    /// rest of the guidance (trailing-slash directories, the quoted spelling)
    /// still reaches the model.
    #[test]
    fn reference_rewrite_keeps_the_rest_of_the_guidance() {
        let out = neutralize_harness_identity(DSH_REFERENCE_PAIR).expect("rewritten");
        assert!(out.contains("absolute paths identify files or directories on the host."));
    }

    /// The whole v2.4.1 regression, in one assertion: the real system prompt as
    /// DSH 0.2.2-alpha.2 assembled it must come out with **no** rejected literal
    /// left, and must keep the identity opener's replacement rather than losing
    /// the sentence entirely.
    #[test]
    fn real_dsh_prompt_loses_every_rejected_literal() {
        // Abridged to the sections that carry literals plus a neutral filler, so
        // the test states which real sentences matter instead of pinning a
        // 7 KB fixture that changes with every DSH release.
        let prompt = [
            DSH_OPENER,
            "You are a coding agent powered by the deepseek-v4.1-flash model.",
            "Use the read tool — not shell commands like cat — to inspect text files.",
            DSH_REFERENCE_PAIR,
            "You are interacting with the user through the DeepSeek Harness Web GUI whose URL the runtime context names. When the user refers to \"this page\", they mean this GUI.",
        ]
        .join("\n\n");

        let out = neutralize_harness_identity(&prompt).expect("literals present");
        for (from, _) in HARNESS_REWRITES {
            assert!(
                !out.contains(from),
                "rejected literal survived: {from:?} in {out:?}"
            );
        }
        assert!(out.contains("You are an AI agent."));
        assert!(out.contains("You are a coding agent powered by the deepseek-v4.1-flash model."));
    }

    /// The gateway checks the *concatenated* system text, so a parts array is
    /// rejected exactly like a plain string. The pre-v2.4.1 code matched only
    /// `MessageContent::Text` and skipped parts silently — sanitizer "ran", the
    /// request was blocked anyway. This is the regression test for that.
    #[test]
    fn rewrites_parts_shaped_system_content() {
        let mut req = req_with(vec![
            parts_system_msg(DSH_REFERENCE_PAIR),
            text_msg("user", "read the file"),
        ]);
        assert_eq!(
            neutralize_harness_system_messages(&mut req),
            1,
            "parts-shaped system content must be rewritten, not skipped"
        );
        let Some(openai::MessageContent::Parts(parts)) = &req.messages[0].content else {
            panic!(
                "content shape must be preserved: {:?}",
                req.messages[0].content
            );
        };
        assert!(
            !format!("{parts:?}").contains("Relative paths resolve from the workspace root"),
            "literal survived inside parts: {parts:?}"
        );
        // Shaping is the client's choice: a parts array must not become a string.
        assert_eq!(parts.len(), 1);
    }

    /// A multi-part system message where only one part carries a literal: that
    /// part is rewritten, the sibling text and any image part are untouched.
    #[test]
    fn rewrites_only_the_offending_part() {
        let mut req = req_with(vec![openai::Message {
            role: "system".to_string(),
            content: Some(openai::MessageContent::Parts(vec![
                openai::ContentPart::Text {
                    text: "You are a helpful assistant.".to_string(),
                },
                openai::ContentPart::Text {
                    text: DSH_OPENER.to_string(),
                },
            ])),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }]);
        assert_eq!(neutralize_harness_system_messages(&mut req), 1);
        let Some(openai::MessageContent::Parts(parts)) = &req.messages[0].content else {
            panic!("expected parts");
        };
        match &parts[0] {
            openai::ContentPart::Text { text } => assert_eq!(text, "You are a helpful assistant."),
            other => panic!("unexpected part: {other:?}"),
        }
        match &parts[1] {
            openai::ContentPart::Text { text } => {
                assert!(
                    !text.contains("DeepSeek Harness"),
                    "part not rewritten: {text}"
                )
            }
            other => panic!("unexpected part: {other:?}"),
        }
    }

    /// A clean parts-shaped prompt changes nothing, so the caller does not log a
    /// rewrite that never happened.
    #[test]
    fn leaves_clean_parts_system_content_alone() {
        let mut req = req_with(vec![parts_system_msg("You are a helpful assistant.")]);
        assert_eq!(neutralize_harness_system_messages(&mut req), 0);
    }

    /// Every row of [`HARNESS_REWRITES`] must actually change its literal.
    /// Measured: a pair whose `from` and `to` are identical ships silently
    /// (v2.1.5 did exactly that), and the request stays blocked while the
    /// sanitizer reports success.
    #[test]
    fn every_harness_rewrite_pair_actually_rewrites() {
        for (from, to) in HARNESS_REWRITES {
            assert_ne!(from, to, "no-op rewrite pair: {from:?}");
            assert!(
                neutralize_harness_identity(from).is_some_and(|out| out != *from),
                "HARNESS_REWRITES row {from:?} is not applied by neutralize_harness_identity"
            );
        }
    }

    /// Non-text parts must be copied through untouched.
    #[test]
    fn preserves_image_parts_in_system_content() {
        let image = openai::ContentPart::ImageUrl {
            image_url: openai::ImageUrl {
                url: "data:image/png;base64,AAAA".to_string(),
            },
        };
        let mut req = req_with(vec![openai::Message {
            role: "system".to_string(),
            content: Some(openai::MessageContent::Parts(vec![
                image.clone(),
                openai::ContentPart::Text {
                    text: DSH_REFERENCE_PAIR.to_string(),
                },
            ])),
            reasoning_content: None,
            tool_calls: None,
            tool_call_id: None,
            name: None,
        }]);
        assert_eq!(neutralize_harness_system_messages(&mut req), 1);
        let Some(openai::MessageContent::Parts(parts)) = &req.messages[0].content else {
            panic!("expected parts");
        };
        assert_eq!(parts[0], image, "image part must be byte-identical");
    }

    /// Clean prompts must not be rewritten at all (no allocation, no drift).
    #[test]
    fn leaves_clean_system_prompt_alone() {
        let text = "You are a helpful assistant. Be concise.";
        assert_eq!(neutralize_harness_identity(text), None);
    }

    /// The gateway inspects the system channel only: the same literal inside a
    /// tool result is served. Rewriting there corrupted the user's own data
    /// (project paths, source code, `git log` output) for no benefit.
    #[test]
    fn does_not_touch_non_system_messages() {
        let mut req = req_with(vec![
            system_msg(DSH_OPENER),
            text_msg("user", "read the file"),
            text_msg("tool", &format!("content:\n{DSH_OPENER}\n{DSH_WEB_GUI}")),
            text_msg("assistant", DSH_OPENER),
        ]);
        // Keep a byte-exact copy of everything that is not a system message.
        let before: Vec<String> = req
            .messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| format!("{:?}", m.content))
            .collect();

        let changed = neutralize_harness_system_messages(&mut req);
        assert_eq!(changed, 1, "exactly the one system message should change");

        let after: Vec<String> = req
            .messages
            .iter()
            .filter(|m| m.role != "system")
            .map(|m| format!("{:?}", m.content))
            .collect();
        assert_eq!(before, after, "non-system messages must be byte-identical");

        // And the system message really did get repaired.
        match &req.messages[0].content {
            Some(openai::MessageContent::Text(t)) => {
                assert!(!t.contains("DeepSeek Harness"));
            }
            other => panic!("unexpected system content: {other:?}"),
        }
    }

    /// A prompt needing no rewrite reports zero changes, so the caller can skip
    /// logging work on ordinary traffic.
    #[test]
    fn reports_zero_changes_for_clean_system_prompt() {
        let mut req = req_with(vec![system_msg("You are a helpful assistant.")]);
        assert_eq!(neutralize_harness_system_messages(&mut req), 0);
    }

    /// Guard against the silent no-op that shipped in an earlier attempt: a
    /// rewrite pair whose `from` and `to` were identical, so the pass "ran" and
    /// changed nothing. Asserting the output differs from the input catches it.
    #[test]
    fn rewrite_actually_changes_the_text() {
        let out = neutralize_harness_identity(DSH_OPENER).expect("rewritten");
        assert_ne!(out, DSH_OPENER, "rewrite must not be an identity mapping");
    }

    /// Every entry of `rewrite_table` must actually change the text.
    ///
    /// A pair whose `from` and `to` are identical — including the case where a
    /// zero-width character used to break a literal match is lost on write — is a
    /// silent no-op: `sanitize_text` appears to run and returns the original
    /// string, so the request is still rejected while tests pass. That exact
    /// failure shipped once (v2.1.5, per its own commit message), which is why
    /// this asserts on the whole table rather than one hand-picked phrase.
    #[test]
    fn every_rewrite_pair_actually_rewrites() {
        for (from, to) in rewrite_table() {
            assert_ne!(
                from, to,
                "no-op rewrite pair (from == to): {from:?} — it would silently match nothing"
            );
        }
    }

    /// The rewrite must be observable through the public path, not just the table:
    /// `sanitize_text` on the source phrasing has to yield the target phrasing.
    #[test]
    fn every_rewrite_pair_is_applied_by_sanitize_text() {
        for (from, to) in rewrite_table() {
            // `sanitize_text` gates on `has_fingerprint` first, so a pair whose
            // source is not a feature substring would never run. Asserting the
            // output pins both the gate and the replacement together.
            let out = sanitize_text(from);
            assert_ne!(
                out, *from,
                "sanitize_text left {from:?} untouched — has_fingerprint may not cover it"
            );
            assert!(
                out.contains(to) || *to == out.trim(),
                "sanitize_text({from:?}) = {out:?} does not contain the target {to:?}"
            );
        }
    }

    /// The upstream normalizes zero-width characters away, so a zero-width-based
    /// "break the literal" trick is invisible to it. This pass must not rely on
    /// invisible characters to work.
    #[test]
    fn replacement_uses_visible_text_only() {
        let out = neutralize_harness_identity(DSH_OPENER).expect("rewritten");
        assert!(
            !out.chars().any(|c| c == '\u{200B}' || c == '\u{FEFF}'),
            "replacement must not smuggle zero-width characters: {out:?}"
        );
    }

    #[test]
    fn normalize_message_sequence_merges_adjacent_assistants_with_text_and_tools() {
        let input = vec![
            openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("hello".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "assistant".to_string(),
                content: Some(openai::MessageContent::Text("I will run bash:".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![openai::ToolCall {
                    id: "call_1".to_string(),
                    call_type: "function".to_string(),
                    function: openai::FunctionCall {
                        name: "Bash".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![openai::ToolCall {
                    id: "call_2".to_string(),
                    call_type: "function".to_string(),
                    function: openai::FunctionCall {
                        name: "Read".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "tool".to_string(),
                content: Some(openai::MessageContent::Text("result 2".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some("call_2".to_string()),
                name: None,
            },
            openai::Message {
                role: "tool".to_string(),
                content: Some(openai::MessageContent::Text("result 1".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some("call_1".to_string()),
                name: None,
            },
        ];

        let normalized = normalize_message_sequence(input);
        assert_eq!(normalized.len(), 4); // user, assistant, tool(call_1), tool(call_2)

        assert_eq!(normalized[0].role, "user");
        assert_eq!(normalized[1].role, "assistant");
        assert_eq!(
            normalized[1].content,
            Some(openai::MessageContent::Text("I will run bash:".to_string()))
        );
        let calls = normalized[1].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_1");
        assert_eq!(calls[1].id, "call_2");

        // Tool messages must be reordered strictly to match call_1, then call_2
        assert_eq!(normalized[2].role, "tool");
        assert_eq!(normalized[2].tool_call_id.as_deref(), Some("call_1"));
        assert_eq!(
            normalized[2].content,
            Some(openai::MessageContent::Text("result 1".to_string()))
        );

        assert_eq!(normalized[3].role, "tool");
        assert_eq!(normalized[3].tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(
            normalized[3].content,
            Some(openai::MessageContent::Text("result 2".to_string()))
        );
    }

    #[test]
    fn normalize_message_sequence_adds_stubs_for_missing_tool_results_and_drops_orphans() {
        let input = vec![
            openai::Message {
                role: "assistant".to_string(),
                content: None,
                reasoning_content: None,
                tool_calls: Some(vec![
                    openai::ToolCall {
                        id: "call_a".to_string(),
                        call_type: "function".to_string(),
                        function: openai::FunctionCall {
                            name: "Bash".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                    openai::ToolCall {
                        id: "call_b".to_string(),
                        call_type: "function".to_string(),
                        function: openai::FunctionCall {
                            name: "Read".to_string(),
                            arguments: "{}".to_string(),
                        },
                    },
                ]),
                tool_call_id: None,
                name: None,
            },
            // Only call_a has a tool result, plus an orphan call_x
            openai::Message {
                role: "tool".to_string(),
                content: Some(openai::MessageContent::Text("res a".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some("call_a".to_string()),
                name: None,
            },
            openai::Message {
                role: "tool".to_string(),
                content: Some(openai::MessageContent::Text("orphan".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some("call_x".to_string()),
                name: None,
            },
        ];

        let normalized = normalize_message_sequence(input);
        assert_eq!(normalized.len(), 3); // assistant, tool(call_a), tool(call_b stub)

        assert_eq!(normalized[1].tool_call_id.as_deref(), Some("call_a"));
        assert_eq!(
            normalized[1].content,
            Some(openai::MessageContent::Text("res a".to_string()))
        );

        // call_b was missing, must have a stub
        assert_eq!(normalized[2].tool_call_id.as_deref(), Some("call_b"));
        assert_eq!(
            normalized[2].content,
            Some(openai::MessageContent::Text("[result omitted]".to_string()))
        );
    }

    #[test]
    fn normalize_message_sequence_prunes_empty_tool_calls_and_merges_reasoning() {
        let input = vec![
            openai::Message {
                role: "assistant".to_string(),
                content: Some(openai::MessageContent::Text("First".to_string())),
                reasoning_content: Some("think 1".to_string()),
                tool_calls: Some(vec![openai::ToolCall {
                    id: "".to_string(),
                    call_type: "function".to_string(),
                    function: openai::FunctionCall {
                        name: "".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "assistant".to_string(),
                content: Some(openai::MessageContent::Text("Second".to_string())),
                reasoning_content: Some("think 2".to_string()),
                tool_calls: Some(vec![openai::ToolCall {
                    id: "valid_id".to_string(),
                    call_type: "function".to_string(),
                    function: openai::FunctionCall {
                        name: "valid_tool".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "tool".to_string(),
                content: Some(openai::MessageContent::Text("valid res".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some("valid_id".to_string()),
                name: None,
            },
        ];

        let normalized = normalize_message_sequence(input);
        assert_eq!(normalized.len(), 2); // assistant, tool(valid_id)

        assert_eq!(
            normalized[0].content,
            Some(openai::MessageContent::Text("First\n\nSecond".to_string()))
        );
        assert_eq!(
            normalized[0].reasoning_content,
            Some("think 1\n\nthink 2".to_string())
        );
        let calls = normalized[0].tool_calls.as_ref().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "valid_id");
        assert_eq!(calls[0].function.name, "valid_tool");
    }

    #[test]
    fn normalize_message_sequence_is_idempotent() {
        let input = vec![
            openai::Message {
                role: "user".to_string(),
                content: Some(openai::MessageContent::Text("go".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "assistant".to_string(),
                content: Some(openai::MessageContent::Text("ok".to_string())),
                reasoning_content: None,
                tool_calls: Some(vec![openai::ToolCall {
                    id: "c1".to_string(),
                    call_type: "function".to_string(),
                    function: openai::FunctionCall {
                        name: "Bash".to_string(),
                        arguments: "{}".to_string(),
                    },
                }]),
                tool_call_id: None,
                name: None,
            },
            openai::Message {
                role: "tool".to_string(),
                content: Some(openai::MessageContent::Text("done".to_string())),
                reasoning_content: None,
                tool_calls: None,
                tool_call_id: Some("c1".to_string()),
                name: None,
            },
        ];

        let once = normalize_message_sequence(input);
        let twice = normalize_message_sequence(once.clone());

        assert_eq!(format!("{:?}", once), format!("{:?}", twice));
    }
}
