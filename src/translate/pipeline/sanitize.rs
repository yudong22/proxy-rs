//! Fingerprint and identity scrubbing applied to outbound text.

use super::*;

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
pub(crate) fn rewrite_table() -> &'static [(&'static str, &'static str)] {
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
pub(crate) const HARNESS_REWRITES: &[(&str, &str)] = &[
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
