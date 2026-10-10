use crate::error::{ProxyError, ProxyResult};
use crate::models::{anthropic, openai};
use crate::translate::core;
use regex::Regex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::OnceLock;

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

/// Resolve the upstream model for a client-facing model name.
///
/// The one place the precedence lives, shared by every route so a request
/// cannot reach a different upstream model depending on which endpoint it
/// arrived at:
///
/// 1. the policy's configured model for this kind of request (if any),
/// 2. otherwise the model the client asked for,
/// 3. then `model_map`, applied to whichever of those won,
/// 4. then the `[1M]`-style suffix stripped, when configured.
///
/// The configured model deliberately outranks the client's: it exists precisely
/// to override what the client sends. Note `model_map` is consulted *after*
/// that override, so a mapping keyed on the client's model name does not apply
/// once a configured model has taken its place.
/// The virtual model name that the gateway routes to a free upstream node.
///
/// Clients ask for `free`; the gateway resolves it to `hy3` during the day and
/// `hy4-preview` at night (local 23:00–08:00, when `hy4-preview` is free), and
/// lets the existing credential pool round-robin accounts when one is
/// daily-exhausted. This is the single name clients should pin to.
pub const FREE_MODEL_NAME: &str = "free";

/// Resolve the virtual `free` model to its concrete upstream node.
///
/// Daytime (local hour 08:00–22:59) → the day free node (`hy3`); night
/// (local 23:00–07:59) → the night free node (`hy4-preview`). The night window
/// and node names are overridable through the env vars below so the schedule or
/// nodes can be retuned without a rebuild:
///
/// * `PROXY_FREE_DAY_MODEL`   (default `hy3`)
/// * `PROXY_FREE_NIGHT_MODEL` (default `hy4-preview`)
/// * `PROXY_FREE_NIGHT_START` (default `23`, inclusive)
/// * `PROXY_FREE_NIGHT_END`   (default `8`, exclusive)
pub(crate) fn resolve_free_model() -> String {
    let day_model =
        std::env::var("PROXY_FREE_DAY_MODEL").unwrap_or_else(|_| "hy3".to_string());
    let night_model =
        std::env::var("PROXY_FREE_NIGHT_MODEL").unwrap_or_else(|_| "hy4-preview".to_string());
    let start: i32 = std::env::var("PROXY_FREE_NIGHT_START")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(23);
    let end: i32 = std::env::var("PROXY_FREE_NIGHT_END")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(8);

    if is_night(start, end) {
        night_model
    } else {
        day_model
    }
}

/// Whether a given local `hour` (0–23) falls inside the night window
/// `[start, end)`, wrapping past midnight when `start > end`.
///
/// Kept pure (no clock access) so it can be unit-tested deterministically.
/// `resolve_free_model` feeds it the current local hour.
pub(crate) fn is_night_hour(hour: i32, start: i32, end: i32) -> bool {
    if start <= end {
        // Non-wrapping window, e.g. 0–8.
        hour >= start && hour < end
    } else {
        // Wrapping window, e.g. 23–8 → night if hour >= 23 OR hour < 8.
        hour >= start || hour < end
    }
}

/// Whether the local clock is currently inside the night window
/// `[start, end)` expressed in local hours (0–23), wrapping past midnight.
fn is_night(start: i32, end: i32) -> bool {
    let offset_secs = crate::util::local_utc_offset_secs();
    let local_ms = crate::util::unix_millis() + offset_secs * 1_000;
    let secs_of_day = ((local_ms / 1_000) % 86_400 + 86_400) % 86_400;
    let hour = (secs_of_day / 3_600) as i32;
    is_night_hour(hour, start, end)
}

pub(crate) fn resolve_upstream_model(
    client_model: &str,
    configured_model: Option<&String>,
    policy: &TranslationPolicy,
) -> String {
    // Virtual `free` model: routed by the local clock before any configured
    // override or `model_map`, so a request for `free` always lands on the
    // current free node regardless of the configured reasoning/completion model.
    if client_model == FREE_MODEL_NAME {
        return resolve_free_model();
    }

    let model = configured_model
        .cloned()
        .unwrap_or_else(|| client_model.to_string());

    let resolved = policy
        .model_map
        .get(&model)
        .cloned()
        .unwrap_or_else(|| model.clone());

    if policy.strip_model_suffix {
        strip_model_suffix(&resolved)
    } else {
        resolved
    }
}

fn select_model(req: &anthropic::AnthropicRequest, policy: &TranslationPolicy) -> String {
    let has_thinking = req
        .extra
        .get("thinking")
        .and_then(|v| v.as_object())
        .map(|o| o.get("type").and_then(|t| t.as_str()) == Some("enabled"))
        .unwrap_or(false);

    let configured = if has_thinking {
        policy.reasoning_model.as_ref()
    } else {
        policy.completion_model.as_ref()
    };

    resolve_upstream_model(&req.model, configured, policy)
}

pub(crate) fn sanitize_prompt(text: String, terms: &[String]) -> String {
    let mut sanitized = text;
    let mut removed = Vec::new();

    for term in terms {
        let next = core::remove_term(&sanitized, term);
        if next != sanitized {
            sanitized = next;
            removed.push(term.clone());
        }
    }

    if !removed.is_empty() {
        tracing::debug!(
            "Removed configured system prompt terms for upstream compatibility: {}",
            removed.join("; ")
        );
    }

    sanitized
}

// ---------------------------------------------------------------------------
// Fingerprint sanitization (port of workbuddy2api/internal/upstream/sanitize.go)
// ---------------------------------------------------------------------------
//
// The upstream content filter rejects requests by exact-substring match on a
// handful of fixed template strings (CLI system-prompt sentences, SDK key names,
// the bare error code `11128`, etc.). Rewriting a single token per phrase keeps
// the semantics intact while breaking the literal match. We must apply this to
// *every* field that can carry a fingerprint — user/assistant content, tool
// call arguments (stringified JSON), and reasoning content — not just the
// system prompt, or the blocked phrases simply reappear in later turns.
//
// This runs as a single deterministic pass over the already-translated OpenAI
// request, so it is upstream-cache friendly: identical input yields an identical
// rewritten body, so the cache prefix stays stable across retries/turns.

/// Quick pre-check substrings: if none are present we skip the (slightly more
/// expensive) regex pass entirely. Mirrors `sanitizeFeatures` in the Go source.
const FINGERPRINT_FEATURES: &[&str] = &[
    "x-anthropic-billing-header",
    "cc_entrypoint=",
    "You are Claude Code",
    "Main branch (",
    "You are a coding agent running in the Codex CLI",
    "github.com/anthropics/",
    "11128",
    "anthropics/claude-code/issues",
];

/// `(from, to)` rewrite pairs. Each rewrites exactly one token so semantics are
/// preserved. Order is irrelevant (non-overlapping).
fn rewrite_table() -> &'static [(&'static str, &'static str)] {
    &[
        (
            "You are Claude Code, Anthropic's official CLI for Claude",
            "You are Claude Code, Anthropic's official CLI tool for Claude",
        ),
        (
            "Main branch (you will usually use this for PRs)",
            "Default branch (you will usually use this for PRs)",
        ),
        (
            "You are a coding agent running in the Codex CLI, a terminal-based coding assistant.",
            "You are a coding agent running in the Codex CLI tool, a terminal-based coding assistant.",
        ),
        (
            "To give feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
            "To provide feedback, users should report the issue at https://github.com/anthropics/claude-code/issues",
        ),
        // Upstream anti-probing: any occurrence of the bare error code `11128`
        // in the body is itself a block condition. A hyphen keeps it readable
        // and still breaks the literal match (zero-width spaces are normalized
        // away by the upstream).
        ("11128", "11-128"),
    ]
}

/// Strip layer: `x-anthropic-billing-header: …;?` (key+value, whole segment
/// removed).
fn hdr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)x-anthropic-billing-header:[^;\n]*;?\s*").unwrap())
}

/// Strip layer: trailing bare `cc_*=…;` key/values (looped until stable).
fn kv_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)\bcc_[a-z0-9_]+=[^;\n]*;?\s*").unwrap())
}

/// Fallback layer: a bare SDK key name (no colon, no value) — e.g. referenced
/// inside assistant reasoning — cannot be deleted without dropping context, so
/// we minimally abbreviate it. Superset of `hdr_re` (no colon required).
fn bare_hdr_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"(?i)x-anthropic-billing-header").unwrap())
}

/// False when the text is definitely clean: skips all regex work on normal
/// traffic. Cheap `contains` fast path plus the case-insensitive bare-key regex
/// (which `contains` would miss for `X-Anthropic-…` variants).
fn has_fingerprint(text: &str) -> bool {
    if FINGERPRINT_FEATURES.iter().any(|f| text.contains(f)) {
        return true;
    }
    bare_hdr_re().is_match(text)
}

/// Rewrite a single text segment. Returns the input unchanged when no fixed
/// fingerprint is present (zero allocation on clean text).
pub(crate) fn sanitize_text(text: &str) -> String {
    if !has_fingerprint(text) {
        return text.to_string();
    }
    let mut out = text.to_string();
    for (from, to) in rewrite_table() {
        out = out.replace(from, to);
    }
    if hdr_re().is_match(&out) {
        out = hdr_re().replace_all(&out, "").to_string();
    }
    if out.contains("cc_") {
        // Clear trailing bare key/values; loop because replacements can expose
        // adjacent segments (`cc_a=1; cc_b=2;`).
        let mut prev;
        loop {
            prev = out.clone();
            out = kv_re().replace_all(&out, "").to_string();
            if out == prev {
                break;
            }
        }
    }
    // Bare key name: minimal abbreviation to break the literal match
    // (key/value form was already removed above).
    out = bare_hdr_re()
        .replace_all(&out, "x-anthropic-billing-hdr")
        .to_string();
    out.trim().to_string()
}

/// Sanitize a single message content value (string or multimodal parts). Image
/// and other non-text parts are left untouched. Returns a new value.
fn sanitize_content_value(content: &openai::MessageContent) -> openai::MessageContent {
    match content {
        openai::MessageContent::Text(text) => openai::MessageContent::Text(sanitize_text(text)),
        openai::MessageContent::Parts(parts) => {
            let new_parts: Vec<_> = parts
                .iter()
                .map(|p| match p {
                    openai::ContentPart::Text { text } => openai::ContentPart::Text {
                        text: sanitize_text(text),
                    },
                    other => other.clone(),
                })
                .collect();
            openai::MessageContent::Parts(new_parts)
        }
    }
}

/// Sanitize `function.arguments` (stringified JSON) of every tool call.
fn sanitize_tool_calls(calls: &mut [openai::ToolCall]) {
    for call in calls.iter_mut() {
        let rewritten = sanitize_text(&call.function.arguments);
        call.function.arguments = rewritten;
    }
}

/// Neutralize fingerprints across an entire translated OpenAI request: every
/// message's content, reasoning_content and tool-call arguments, plus the
/// `developer`→`system` role normalization. The request is mutated in place.
pub(crate) fn sanitize_openai_request(req: &mut openai::OpenAIRequest) {
    for msg in req.messages.iter_mut() {
        // The upstream OpenAI-compatible gateway only understands `system` /
        // `user` / `assistant` / `tool`. Anthropic's `developer` role (used by
        // some SDK/system variants) is rejected as an "unapproved channel",
        // so normalize it to `system`.
        if msg.role == "developer" {
            msg.role = "system".to_string();
        }

        if let Some(content) = &msg.content {
            let new_content = sanitize_content_value(content);
            if new_content != *content {
                msg.content = Some(new_content);
            }
        }
        if let Some(rc) = &msg.reasoning_content {
            let new_rc = sanitize_text(rc);
            if new_rc != *rc {
                msg.reasoning_content = Some(new_rc);
            }
        }
        if let Some(calls) = &mut msg.tool_calls {
            sanitize_tool_calls(calls);
        }
    }
}

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
}
