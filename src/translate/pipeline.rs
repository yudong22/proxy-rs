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
    let day_model = std::env::var("PROXY_FREE_DAY_MODEL").unwrap_or_else(|_| "hy3".to_string());
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

/// The daytime free node (`hy3` by default). When the night free node
/// (`hy4-preview`) is exhausted across every account and key, the virtual `free`
/// model degrades to this node instead of erroring — it is the daytime free
/// node, so it is still "free" to the client.
pub(crate) fn free_degraded_model() -> String {
    std::env::var("PROXY_FREE_DAY_MODEL").unwrap_or_else(|_| "hy3".to_string())
}

/// The night free node (`hy4-preview` by default). A `free` request is only
/// eligible for the night→day degradation when its resolved upstream model is
/// exactly this node; a daytime `free` request already resolves to the day node
/// and has nothing further to degrade to.
pub(crate) fn free_night_model() -> String {
    std::env::var("PROXY_FREE_NIGHT_MODEL").unwrap_or_else(|_| "hy4-preview".to_string())
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

// ---------------------------------------------------------------------------
// DeepSeek Harness identity neutralization
// ---------------------------------------------------------------------------
//
// The WorkBuddy/CodeBuddy gateway rejects a request as an "unapproved channel"
// (business code 11-128) when a **system message** contains one of the literals
// in [`HARNESS_REWRITES`]. All of them ship verbatim in DSH's own system prompt:
//
//   * `You are an AI agent powered by DeepSeek Harness.`
//     — `packages/core/system-prompt/src/index.ts` (`harness:identity` section)
//   * `DeepSeek Harness Web GUI`
//     — `packages/context/harness-dev-context/src/index.ts` (`WEB_SURFACE_PROMPT`)
//   * `Tokens prefixed with @ … Relative paths resolve from the workspace root`
//     — `packages/context/file-reference/src/index.ts` (`FILE_REFERENCE_PROMPT`)
//
// Verified live against the real upstream, twice, because the rule set moved:
//
//   2026-10-25 (DSH 0.2.1-rc.3, real 653 KB / 14-message / 42-tool capture):
//     · unmodified capture                     -> 400 / 11-128
//     · both identity literals rewritten       -> 200
//     · only one of the two rewritten          -> 400  (both were needed)
//     · a literal placed in a user, tool or
//       reasoning message                      -> 200  (system-only check)
//
//   2026-11-05 (DSH 0.2.2-alpha.2), the state that produced the v2.4.1 fix:
//     · unmodified capture                     -> 400 / 11-128
//     · the two identity literals rewritten    -> 400  (upstream had retuned;
//                                                      they no longer block)
//     · the `@`-reference pair rewritten       -> 200
//     · whole prompt in a parts array          -> the same 400 as a plain
//                                                 string, i.e. shape-agnostic
//
// Three consequences shape the implementation:
//
//   1. Every entry must be rewritten. A partially covered prompt looks exactly
//      like "the rewrite does not work", which is how v2.1.4 (identity opener
//      only) and v2.1.5 (Web GUI phrase only) were both reported as failures.
//   2. The match is on the *system channel*, not on request content. Earlier
//      attempts replaced the phrase everywhere in the body, which corrupted the
//      user's own data (~40 hits lived in tool results: project paths, source
//      code, `git log` output). Restricting this pass to `system` messages
//      fixes the rejection while leaving every other message byte-identical.
//   3. It must cover **both content shapes** (`Text` and `Parts`): the gateway
//      checks the concatenated system text, not the JSON encoding.

/// DSH identity opener, without the trailing period so the form lacking it
/// (also rejected) is covered by the same entry.
const DSH_IDENTITY_LITERAL: &str = "You are an AI agent powered by DeepSeek Harness";
/// Equally neutral self-description: keeps the opener's role in the prompt (it
/// opens by telling the model what it is) but drops the product name, which is
/// the part the gateway keys on.
const DSH_IDENTITY_REPLACEMENT: &str = "You are an AI agent";
/// DSH web-surface phrase. Only the product name inside it is rewritten; the
/// sentence keeps its meaning, since the guidance that follows is what actually
/// tells the model how to behave in the GUI.
const DSH_WEB_GUI_LITERAL: &str = "DeepSeek Harness Web GUI";
const DSH_WEB_GUI_REPLACEMENT: &str = "web GUI";

/// DSH `@`-reference guidance (`packages/context/file-reference/src/index.ts`,
/// `FILE_REFERENCE_PROMPT`), third rejected literal — found live on 2026-11-05
/// with DSH 0.2.2-alpha.2, after the gateway had *stopped* rejecting the two
/// literals above on their own.
///
/// What the gateway keys on is the **pair**: the opener sentence
/// `Tokens prefixed with @ are paths the user explicitly referenced.` followed
/// immediately by `Relative paths resolve from the workspace root`. Measured
/// live (see the bisect notes below):
///
///   · opener alone, this sentence alone                -> 200
///   · the two adjacent (period or semicolon between)   -> 400 / 11-128
///   · the same two lines separated by a newline        -> 200
///   · either sentence reworded at all (#533/#537/#539
///     style: upstream re-tunes the exact phrasing)     -> 200
///
/// Only the second sentence is rewritten here — one word, same meaning — which
/// is the smallest edit that breaks the pair. The opener stays byte-identical so
/// the model keeps the `@`-mention rule spelled out exactly as DSH wrote it.
const DSH_REFERENCE_LITERAL: &str = "Relative paths resolve from the workspace root";
const DSH_REFERENCE_REPLACEMENT: &str = "Relative paths resolve against the workspace root";

/// Every `(literal, replacement)` pair the gateway rejects inside a **system**
/// message. Each rewrite is deliberately the smallest visible edit that breaks
/// the literal match, so the prompt keeps its meaning and its behaviour.
///
/// This is a table, not three hand-written calls, because upstream adds a rule
/// without warning: v2.1.4 covered one of two needed literals, v2.1.5 the other,
/// and each looked like "the fix does not work" until the third appeared. A new
/// fingerprint is one more row.
const HARNESS_REWRITES: &[(&str, &str)] = &[
    (DSH_IDENTITY_LITERAL, DSH_IDENTITY_REPLACEMENT),
    (DSH_WEB_GUI_LITERAL, DSH_WEB_GUI_REPLACEMENT),
    (DSH_REFERENCE_LITERAL, DSH_REFERENCE_REPLACEMENT),
];

/// Whether `text` carries any rejected DSH literal. Cheap `contains` checks so
/// the common (already-clean) path allocates nothing.
fn has_harness_identity(text: &str) -> bool {
    HARNESS_REWRITES.iter().any(|(from, _)| text.contains(from))
}

/// Rewrite every gateway-rejected DSH literal in `text`. Returns `None` when
/// none is present, so the caller can skip replacing the message content.
///
/// Applied to `system` messages only — see the block comment above for why the
/// scope matters.
pub(crate) fn neutralize_harness_identity(text: &str) -> Option<String> {
    if !has_harness_identity(text) {
        return None;
    }
    let mut out = text.to_string();
    for (from, to) in HARNESS_REWRITES {
        out = out.replace(from, to);
    }
    Some(out)
}

/// Rewrite a system message's content whatever shape it arrived in.
///
/// DSH's own adapter sends the system prompt as one plain string, which is why
/// v2.1.7 only handled [`openai::MessageContent::Text`] and still worked. A
/// **parts array** (`[{"type":"text","text":…}]`) is equally accepted by the
/// client SDKs — OpenAI's own SDK sends the system prompt that way — and it hits
/// the very same gateway check. The old code matched `Text` with `let … else`
/// and silently skipped parts, so such a request was blocked on every attempt
/// while the log showed the sanitizer "running". Both shapes are rewritten here.
///
/// Non-text parts (images) are cloned untouched, and an unchanged message is
/// reported as unchanged so the caller's counter stays a real "we changed this".
fn neutralize_harness_content(content: &openai::MessageContent) -> Option<openai::MessageContent> {
    match content {
        openai::MessageContent::Text(text) => {
            let rewritten = neutralize_harness_identity(text)?;
            (rewritten != *text).then_some(openai::MessageContent::Text(rewritten))
        }
        openai::MessageContent::Parts(parts) => {
            let mut changed = false;
            let rewritten: Vec<_> = parts
                .iter()
                .map(|part| match part {
                    openai::ContentPart::Text { text } => match neutralize_harness_identity(text) {
                        Some(new_text) if new_text != *text => {
                            changed = true;
                            openai::ContentPart::Text { text: new_text }
                        }
                        _ => part.clone(),
                    },
                    other => other.clone(),
                })
                .collect();
            changed.then_some(openai::MessageContent::Parts(rewritten))
        }
    }
}

/// Rewrite DSH's rejected literals across every `system` message of `req`,
/// leaving all other messages untouched. Returns how many messages changed.
///
/// Deliberately independent of the `sanitize_fingerprints` setting: those rules
/// neutralize the *client's* identity (Claude Code / Codex), while this one
/// unblocks a gateway check that is not the user's to opt out of.
pub(crate) fn neutralize_harness_system_messages(req: &mut openai::OpenAIRequest) -> usize {
    let mut changed = 0;
    for msg in req.messages.iter_mut() {
        if msg.role != "system" {
            continue;
        }
        let Some(content) = &msg.content else {
            continue;
        };
        if let Some(rewritten) = neutralize_harness_content(content) {
            msg.content = Some(rewritten);
            changed += 1;
        }
    }
    changed
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
                let mut tool_results_by_id: std::collections::HashMap<String, Vec<openai::Message>> =
                    std::collections::HashMap::new();
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
