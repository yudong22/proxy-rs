//! Session identification for the three IDE/CLI clients this gateway serves.
//!
//! Every request carries some notion of "which conversation is this?", but each
//! vendor names and encodes it differently. Putting one conversation's requests
//! under a single `session id` in the logs is what makes "show me everything
//! from this chat" possible instead of scrolling through an interleaved stream.
//!
//! This module is the single place that knows the three dialects. It is a pure
//! mapping (no I/O, no async, no logging) so it can be unit-tested directly and
//! consumed from Layer 3 (`proxy.rs`) without inverting the layer hierarchy.
//!
//! ## Dialects
//!
//! | Client      | Primary source                                     | Secondary |
//! |-------------|----------------------------------------------------|-----------|
//! | Codex       | `x-codex-turn-metadata.session_id` JSON              | `session-id` / `thread-id` headers, `prompt_cache_key` body field |
//! | Claude Code | `x-claude-code-session-id` header                    | `session_id` cookie, `metadata.user_id` body field |
//! | DSH (大守护) | `x-deepseek-harness-session-id` header              | `deepseek-harness/…` user-agent (client-level), `dsh-auth-*` cookie (per-install) |
//!
//! DSH is the one client that identifies at *client* rather than conversation
//! granularity in practice: the shipped build omits the session header and puts
//! no session field in the body, so the user-agent is the best available key.
//! See [`session_from_dsh_identity`] for the evidence.
//!
//! ## Why ids are namespaced (`codex:<uuid>`, `claude:<uuid>`, `dsh:<key>`)
//!
//! Two reasons, and both matter for a filter that has to be trustworthy:
//!
//! 1. **No silent collisions.** The clients' id spaces overlap in *shape*: all
//!    three emit UUID-ish strings, and Codex's 4-segment UUIDv7 lookalikes
//!    differ from Claude Code's only in the third segment. Namespacing keeps a
//!    `codex:` id from ever being confused with a `claude:` one, so a session
//!    filter cannot merge two conversations by accident.
//! 2. **Grep-ability.** `grep 'session_id=codex:01a0cc30'` isolates one chat
//!    with no tooling at all. A bare UUID would need a second lookup to know
//!    which app sent it, which is exactly the ambiguity being removed.
//!
//! Generic `x-session-id`/`session-id` headers are only trusted when the value
//! passes [`looks_like_session_id`]; short values such as `sec-ch-ua-mobile`'s
//! `?0` must never be mistaken for an id.

use axum::http::HeaderMap;
use serde_json::Value;

/// A resolved session identity for one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    /// Which client dialect produced `session_id`.
    pub client: ClientKind,
    /// Namespaced id (`codex:<uuid>`), already unique across dialects.
    pub session_id: String,
    /// Per-turn id when the client exposes one, useful for counting turns in a
    /// session (Codex `turn_id`; empty for the other dialects).
    pub turn_id: String,
    /// The client-facing model name the client itself reported, when the
    /// header form carries one (Codex's `x-codex-turn-metadata.model`). `None`
    /// for body dialects, where the body's own `model` field is authoritative.
    pub model_hint: Option<String>,
}

/// The client dialect a request was identified as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ClientKind {
    /// OpenAI Codex CLI / Desktop.
    Codex,
    /// Anthropic Claude Code CLI.
    ClaudeCode,
    /// DSH (大守护) — the DeepSeek Harness app.
    Dsh,
    /// A client that matched none of the known dialects.
    #[default]
    Unknown,
}

impl ClientKind {
    /// Short, stable tag used as the log prefix and as the namespaced session-id
    /// component, so `codex:` ids never collide with `claude:`/`dsh:` ids.
    pub fn tag(self) -> &'static str {
        match self {
            ClientKind::Codex => "codex",
            ClientKind::ClaudeCode => "claude",
            ClientKind::Dsh => "dsh",
            ClientKind::Unknown => "unknown",
        }
    }

    /// Human-readable name for log lines.
    pub fn label(self) -> &'static str {
        match self {
            ClientKind::Codex => "Codex",
            ClientKind::ClaudeCode => "Claude Code",
            ClientKind::Dsh => "DSH",
            ClientKind::Unknown => "Unknown",
        }
    }
}

impl SessionInfo {
    /// The fallback identity for a request that matched no dialect.
    pub fn unknown() -> Self {
        SessionInfo {
            client: ClientKind::Unknown,
            session_id: String::new(),
            turn_id: String::new(),
            model_hint: None,
        }
    }

    /// Whether a real session id was resolved.
    pub fn is_known(&self) -> bool {
        !self.session_id.is_empty()
    }

    /// `client=codex session_id=codex:<uuid> turn=<uuid>` for the file log.
    ///
    /// Formatted so a plain `grep 'session_id=codex:01a0cc30'` (or a
    /// `grep 'client=claude'`) isolates one conversation without any tooling.
    pub fn log_tag(&self) -> String {
        if !self.is_known() {
            return "client=unknown".to_string();
        }
        let mut tag = format!(
            "client={} session_id={}",
            self.client.tag(),
            self.session_id
        );
        if !self.turn_id.is_empty() {
            tag.push_str(&format!(" turn={}", self.turn_id));
        }
        tag
    }

    /// `[codex:01a0cc30 / turn 01a0cc31]` for the human-facing GUI console.
    ///
    /// Ids are shortened to their first UUID group: the full value is 36
    /// characters and a console line already runs 170+.
    pub fn display_tag(&self) -> String {
        if !self.is_known() {
            return "[unknown]".to_string();
        }
        let short = |s: &str| {
            s.split('-')
                .next()
                .filter(|head| !head.is_empty() && head.len() < s.len())
                .unwrap_or(s)
                .to_string()
        };
        // `session_id` already carries the `codex:`/`claude:`/`dsh:` prefix.
        let id = self
            .session_id
            .split_once(':')
            .map(|(_, rest)| rest)
            .unwrap_or(&self.session_id);
        if self.turn_id.is_empty() {
            format!("[{}:{}]", self.client.tag(), id)
        } else {
            format!(
                "[{}:{} / turn {}]",
                self.client.tag(),
                short(id),
                short(&self.turn_id)
            )
        }
    }
}

/// Split a raw `Cookie` header into `(name, value)` pairs.
///
/// Values may contain `=` (DSH's `dsh-auth-…` cookie is `<base64url>` plus a
/// signature), so only the *first* `=` of a pair is treated as the separator.
fn parse_cookie_pairs(raw: &str) -> Vec<(String, String)> {
    raw.split(';')
        .filter_map(|pair| {
            let pair = pair.trim();
            let (name, value) = pair.split_once('=')?;
            let name = name.trim().to_string();
            if name.is_empty() {
                return None;
            }
            Some((name, value.trim().to_string()))
        })
        .collect()
}

/// Read the session id from Claude Code's dedicated request header.
///
/// `x-claude-code-session-id` is the CLI's authoritative source: it names the
/// conversation, is stable across every turn of that conversation, and is sent
/// on every request. The cookie and body forms below are *not* reliable for this
/// client — the CLI sends no `session_id` cookie, and `metadata.user_id` is not
/// guaranteed to be present (it identifies the user, not the conversation).
///
/// A generic `x-session-id` is accepted as a fallback only when the user-agent
/// already looks like Claude Code, so an unrelated client's header can never be
/// relabelled `claude:`.
fn session_from_claude_header(headers: &HeaderMap) -> Option<SessionInfo> {
    let raw = header(headers, "x-claude-code-session-id").or_else(|| {
        let looks_claude = header(headers, "user-agent")
            .map(|a| a.to_ascii_lowercase().contains("claude"))
            .unwrap_or(false);
        looks_claude
            .then(|| header(headers, "x-session-id"))
            .flatten()
    })?;

    looks_like_session_id(&raw).then(|| SessionInfo {
        client: ClientKind::ClaudeCode,
        session_id: format!("claude:{}", raw.trim()),
        turn_id: String::new(),
        model_hint: None,
    })
}

/// Read DSH's conversation id, falling back to a client-level id.
///
/// The harness source *declares* `x-deepseek-harness-session-id` on both its
/// Chat Completions and Messages adapters, carrying the harness session id
/// (`session-<uuid>`). But that header is sent **conditionally** — the adapter
/// omits it entirely when `options.sessionId` is undefined — and the shipped
/// 0.1.6-alpha.2 build does not send it at all: a captured request carried
/// neither the header nor any session-shaped body field (body keys were exactly
/// `max_completion_tokens, messages, model, store, stream, stream_options,
/// tools`). So the conversation-level id simply does not reach this hop.
///
/// That header remains the preferred source, so this picks it up automatically
/// if a future build starts sending it. Until then the request is keyed by the
/// **user-agent**, which identifies the client rather than the conversation:
/// every chat from one installed version shares an id. That is a deliberate
/// trade — a client-level grouping that works beats a conversation-level one
/// that never matches.
fn session_from_dsh_identity(headers: &HeaderMap) -> Option<SessionInfo> {
    let raw = header(headers, "x-deepseek-harness-session-id").or_else(|| {
        let ua = header(headers, "user-agent")?;
        ua.to_ascii_lowercase()
            .contains("deepseek-harness")
            .then(|| harness_client_id(&ua))
    })?;

    looks_like_session_id(&raw).then(|| SessionInfo {
        client: ClientKind::Dsh,
        session_id: format!("dsh:{}", raw.trim()),
        turn_id: String::new(),
        model_hint: None,
    })
}

/// Condense a `deepseek-harness/…` user-agent into a stable client id.
///
/// The full user-agent is long and full of punctuation (it embeds a GitHub
/// URL), and the GUI shortens long ids for display by cutting at the first
/// hyphen — which would render the whole thing as a bare `dsh:deepseek`. So
/// only the **version** is kept (`0.1.6-alpha.2`), since that is the part that
/// actually distinguishes one install from another, and it stays readable in
/// both the log line and the session dropdown.
fn harness_client_id(user_agent: &str) -> String {
    user_agent
        .split_whitespace()
        .next()
        .and_then(|product| product.split_once('/'))
        .map(|(_, version)| version.to_string())
        .unwrap_or_else(|| user_agent.trim().to_string())
}

/// Look for a session-id cookie in the raw cookie string.
///
/// Claude Code sends `session_id=<uuid>`. DSH identifies itself with a
/// `dsh-auth-<key>=<value>` cookie, whose *name* is the stable per-install
/// identity (the value is a rotating signed token).
///
/// Note that the DSH console is also reachable from a plain browser, which
/// brings its own unrelated cookies (`wmda_uuid`, `wmda_report_times`, …) to the
/// same request. Those are browser state, not DSH conversation state, so they
/// are deliberately not used as an id.
fn session_from_cookies(raw: &str) -> Option<SessionInfo> {
    let pairs = parse_cookie_pairs(raw);

    if let Some((_, value)) = pairs.iter().find(|(name, _)| name == "session_id") {
        if looks_like_session_id(value) {
            return Some(SessionInfo {
                client: ClientKind::ClaudeCode,
                session_id: format!("claude:{}", value),
                turn_id: String::new(),
                model_hint: None,
            });
        }
    }

    if let Some((name, _)) = pairs.iter().find(|(name, _)| name.starts_with("dsh-auth")) {
        return Some(SessionInfo {
            client: ClientKind::Dsh,
            session_id: format!("dsh:{}", name),
            turn_id: String::new(),
            model_hint: None,
        });
    }

    None
}

/// Accept only values that can plausibly be a session id: long enough to carry
/// entropy, no sentinels, and not a CSS-ish token such as `*/*` or `?0`.
fn looks_like_session_id(value: &str) -> bool {
    let value = value.trim();
    (8..=128).contains(&value.len())
        && value.chars().any(|c| c.is_ascii_alphanumeric())
        && !value.eq_ignore_ascii_case("null")
        && !value.eq_ignore_ascii_case("none")
}

/// Pull `session_id` (and friends) out of a Codex `x-codex-turn-metadata` JSON
/// blob, which arrives as a single header value.
fn session_from_turn_metadata(raw: &str) -> Option<SessionInfo> {
    let meta: Value = serde_json::from_str(raw).ok()?;
    let pick = |key: &str| {
        meta.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| looks_like_session_id(s))
    };

    let session = pick("session_id").or_else(|| pick("thread_id"))?;
    Some(SessionInfo {
        client: ClientKind::Codex,
        session_id: format!("codex:{}", session),
        turn_id: pick("turn_id").unwrap_or_default().to_string(),
        model_hint: pick("model").map(str::to_string),
    })
}

/// Generic `x-session-id` / `session-id` header fallback for clients we do not
/// otherwise recognise.
///
/// The header name is common (Vercel AI SDK, OpenWebUI, …), but so is the value
/// *shape* in unrelated headers, so a *known* dialect must be allowed to win
/// first — Codex sends `session-id` too.
fn session_from_generic_headers(headers: &HeaderMap) -> Option<SessionInfo> {
    for name in ["x-session-id", "session-id"] {
        if let Some(raw) = header(headers, name) {
            if looks_like_session_id(&raw) {
                let client = if header(headers, "originator").as_deref() == Some("Codex")
                    || header(headers, "x-codex-window-id").is_some()
                {
                    ClientKind::Codex
                } else {
                    ClientKind::Unknown
                };
                let client = match client {
                    // Resolve the catch-all: a generic `session-id` header
                    // with no dialect marker is still a usable id.
                    ClientKind::Unknown => detect_client(headers),
                    known => known,
                };
                return Some(SessionInfo {
                    client,
                    session_id: format!("{}:{}", client.tag(), raw.trim()),
                    turn_id: String::new(),
                    model_hint: None,
                });
            }
        }
    }
    None
}

/// Read a header, trimmed and non-empty.
fn header(headers: &HeaderMap, name: &str) -> Option<String> {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

/// Which client dialect a header map belongs to, before any id is resolved.
///
/// Split out from [`detect`] because the body-derived dialects (Claude Code's
/// `metadata.user_id`) need to know who is asking *before* they have a body.
pub fn detect_client(headers: &HeaderMap) -> ClientKind {
    if header(headers, "originator").as_deref() == Some("Codex")
        || header(headers, "x-codex-window-id").is_some()
        || header(headers, "x-codex-turn-metadata").is_some()
        || header(headers, "thread-id").is_some()
    {
        return ClientKind::Codex;
    }
    // DSH's own header is a dialect marker in its own right, so a harness
    // request is attributed even when it carries no `dsh-auth-*` cookie.
    if header(headers, "x-deepseek-harness-session-id").is_some()
        || header(headers, "x-deepseek-harness-user-id").is_some()
    {
        return ClientKind::Dsh;
    }
    if let Some(cookie) = header(headers, "cookie") {
        if parse_cookie_pairs(&cookie)
            .iter()
            .any(|(name, _)| name.starts_with("dsh-auth"))
        {
            return ClientKind::Dsh;
        }
    }
    if let Some(agent) = header(headers, "user-agent") {
        let agent = agent.to_ascii_lowercase();
        if agent.contains("claude") {
            return ClientKind::ClaudeCode;
        }
        // The harness calls itself `deepseek-harness/…`, which does *not*
        // contain the substring `dsh` — matching on `dsh` alone would miss
        // every request the actual app makes.
        if agent.contains("deepseek-harness") || agent.contains("dsh") {
            return ClientKind::Dsh;
        }
    }
    ClientKind::Unknown
}

/// Resolve the session identity for an incoming request from its headers.
///
/// Cheap, allocation-light and infallible: this runs on every proxied request,
/// so it must not be able to fail a request that would otherwise succeed. When
/// nothing matches it returns `SessionInfo::unknown()`, which callers log as
/// `client=unknown` rather than inventing an id.
pub fn detect(headers: &HeaderMap) -> SessionInfo {
    // 1. Codex: the dedicated turn metadata is the richest single source
    //    (session id, per-turn id and the model in one header).
    if let Some(raw) = header(headers, "x-codex-turn-metadata") {
        if let Some(info) = session_from_turn_metadata(&raw) {
            return info;
        }
    }

    // 2. DSH's dedicated header names the *conversation* and is sent on every
    //    request regardless of how the harness was launched. It must be read
    //    before the cookie step below, whose `dsh-auth-*` fallback is only
    //    per-install and would otherwise merge every chat in one install.
    if let Some(info) = session_from_dsh_identity(headers) {
        return info;
    }

    // 3. Cookie dialects come before generic headers: reading `session_id` from
    //    the cookie is what lets `/v1/models` (no body at all) be attributed.
    if let Some(cookie) = header(headers, "cookie") {
        if let Some(info) = session_from_cookies(&cookie) {
            return info;
        }
    }

    // 4. Claude Code's dedicated header, which must be read *before* the generic
    //    `x-session-id`/`session-id` fallback below: a known dialect has to win
    //    over a header name several unrelated clients also use.
    if let Some(info) = session_from_claude_header(headers) {
        return info;
    }

    if let Some(info) = session_from_generic_headers(headers) {
        if info.client != ClientKind::Unknown {
            return info;
        }
        // An anonymous client with a session-shaped header: keep the id but
        // report the dialect so a caller can still ask for a body-derived id.
        return SessionInfo {
            client: detect_client(headers),
            ..info
        };
    }

    SessionInfo {
        client: detect_client(headers),
        ..SessionInfo::unknown()
    }
}

/// Body-derived session id for the dialects that put it in the payload.
///
/// Returns `None` when the request matches neither shape, which is the common
/// case for `/v1/chat/completions` (a passthrough with no session concept).
pub fn detect_from_body(client: ClientKind, body: &Value) -> Option<SessionInfo> {
    match client {
        // Codex's Responses payload carries the id as `prompt_cache_key`; it is
        // the same value as the `session-id` header, so this is only reached on
        // the rare header-less Codex request.
        ClientKind::Codex => {
            let key = body.get("prompt_cache_key")?.as_str()?.trim();
            looks_like_session_id(key).then(|| SessionInfo {
                client: ClientKind::Codex,
                session_id: format!("codex:{}", key),
                turn_id: String::new(),
                model_hint: None,
            })
        }
        // Claude Code has no session header of its own, so the Anthropic
        // `metadata.user_id` field is authoritative here.
        ClientKind::ClaudeCode => {
            let user = body.get("metadata")?.get("user_id")?.as_str()?.trim();
            looks_like_session_id(user).then(|| SessionInfo {
                client: ClientKind::ClaudeCode,
                session_id: format!("claude:{}", user),
                turn_id: String::new(),
                model_hint: None,
            })
        }
        ClientKind::Dsh | ClientKind::Unknown => None,
    }
}

/// Merge a header-resolved identity with a body-derived one.
///
/// The header wins: it is the only source usable on body-less routes, and the
/// only one that names the client with certainty. The body fills a gap only
/// when the header produced nothing, so a Claude Code body can never relabel a
/// Codex request.
pub fn merge(header_info: SessionInfo, body_info: Option<SessionInfo>) -> SessionInfo {
    match body_info {
        Some(info) if !header_info.is_known() => info,
        _ => header_info,
    }
}

/// Extract the client-facing model name from a request body, for log lines that
/// report which model the client asked for.
pub fn model_from_body(body: &Value) -> Option<String> {
    body.get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (k, v) in pairs {
            map.insert(
                k.parse::<axum::http::HeaderName>().unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        map
    }

    const CODEX_SESSION: &str = "01a0cc30-3318-74d2-b045-650a0b0c2e1c";
    const CODEX_TURN: &str = "01a0cc31-bc18-77c3-85d8-de3c452f781c";

    #[test]
    fn codex_is_detected_from_originator_and_session_id_header() {
        let h = headers(&[
            ("originator", "Codex"),
            ("session-id", CODEX_SESSION),
            ("thread-id", CODEX_SESSION),
        ]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::Codex);
        assert_eq!(info.session_id, format!("codex:{}", CODEX_SESSION));
        assert_eq!(info.turn_id, "");
    }

    #[test]
    fn codex_turn_metadata_wins_and_supplies_turn_and_model() {
        let meta = format!(
            r#"{{"installation_id":"4017c924","session_id":"{CODEX_SESSION}","thread_id":"{CODEX_SESSION}","turn_id":"{CODEX_TURN}","model":"gpt-5.6-luna"}}"#
        );
        let h = headers(&[
            ("originator", "Codex"),
            ("x-codex-turn-metadata", &meta),
            ("session-id", CODEX_SESSION),
        ]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::Codex);
        assert_eq!(info.session_id, format!("codex:{}", CODEX_SESSION));
        assert_eq!(info.turn_id, CODEX_TURN);
        assert_eq!(info.model_hint.as_deref(), Some("gpt-5.6-luna"));
    }

    #[test]
    fn codex_detected_even_without_originator() {
        // A `session-id` header alone is ambiguous, but `thread-id` marks Codex.
        let h = headers(&[("thread-id", CODEX_SESSION), ("session-id", CODEX_SESSION)]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::Codex);
        assert_eq!(info.session_id, format!("codex:{}", CODEX_SESSION));
    }

    #[test]
    fn claude_code_is_detected_from_session_cookie() {
        let h = headers(&[(
            "cookie",
            &format!("other=1; session_id={CODEX_SESSION}; x=y"),
        )]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::ClaudeCode);
        assert_eq!(info.session_id, format!("claude:{}", CODEX_SESSION));
    }

    #[test]
    fn claude_code_falls_back_to_metadata_user_id_in_body() {
        // The header is the primary source, but a client that omits it can still
        // be identified by the Anthropic body's `metadata.user_id`.
        let h = headers(&[("user-agent", "claude-cli/2.0.0 (external, cli)")]);
        let header_info = detect(&h);
        assert_eq!(header_info.client, ClientKind::ClaudeCode);
        assert!(!header_info.is_known());

        let body = serde_json::json!({
            "model": "sonnet",
            "metadata": { "user_id": "user_8f3a1c2e-11b4-4d0a-92f0-6c1d2e3f4a5b" }
        });
        let merged = merge(header_info, detect_from_body(ClientKind::ClaudeCode, &body));
        assert_eq!(merged.client, ClientKind::ClaudeCode);
        assert_eq!(
            merged.session_id,
            "claude:user_8f3a1c2e-11b4-4d0a-92f0-6c1d2e3f4a5b"
        );
    }

    #[test]
    fn claude_code_is_detected_from_its_session_header() {
        // The exact header set the real CLI sends (captured in
        // `docs/troubleshooting-claude-code.md`). Before this was parsed, every
        // Claude Code request degraded to `client=unknown`, so the conversation
        // filter had nothing to key on.
        let h = headers(&[
            ("accept", "application/json"),
            ("content-type", "application/json"),
            ("user-agent", "claude-cli/2.1.228 (external, cli)"),
            (
                "x-claude-code-session-id",
                "89be8f22-e51d-4922-bd35-dd8e254872e9",
            ),
            ("x-app", "cli"),
            ("x-stainless-lang", "js"),
            ("anthropic-version", "2023-06-01"),
        ]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::ClaudeCode);
        assert_eq!(
            info.session_id,
            "claude:89be8f22-e51d-4922-bd35-dd8e254872e9"
        );
        assert!(info.is_known());
        assert_eq!(
            info.log_tag(),
            "client=claude session_id=claude:89be8f22-e51d-4922-bd35-dd8e254872e9"
        );
    }

    #[test]
    fn claude_header_beats_a_generic_session_header() {
        // A generic `x-session-id` must not shadow the dialect-specific header:
        // otherwise the request loses its `claude:` name and can be mislabelled.
        let h = headers(&[
            ("user-agent", "claude-cli/2.1.228 (external, cli)"),
            ("x-session-id", "generic-ssssion-value"),
            (
                "x-claude-code-session-id",
                "89be8f22-e51d-4922-bd35-dd8e254872e9",
            ),
        ]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::ClaudeCode);
        assert_eq!(
            info.session_id,
            "claude:89be8f22-e51d-4922-bd35-dd8e254872e9"
        );
    }

    #[test]
    fn claude_header_is_rejected_when_it_looks_like_junk() {
        // Same guard as every other dialect: a too-short value is not an id.
        let h = headers(&[
            ("user-agent", "claude-cli/2.1.228 (external, cli)"),
            ("x-claude-code-session-id", "?0"),
        ]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::ClaudeCode);
        assert!(!info.is_known());
    }

    #[test]
    fn dsh_is_unaffected_by_browser_cookies_it_shares_a_request_with() {
        // The DSH console is also reachable from a browser, so a DSH request can
        // carry the browser's own `wmda_*` cookies. Those are not DSH state and
        // must not become the session id.
        let h = headers(&[(
            "cookie",
            "wmda_uuid=9cca2d0b5213b80bc5700de7aa6d8f34; wmda_new_uuid=1; \
             dsh-auth-KEY=v1.sig",
        )]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::Dsh);
        assert_eq!(info.session_id, "dsh:dsh-auth-KEY");
        assert!(!info.session_id.contains("9cca2d0b"));
    }

    #[test]
    fn claude_metadata_user_id_is_rejected_when_it_looks_like_junk() {
        let body = serde_json::json!({ "metadata": { "user_id": "null" } });
        assert!(detect_from_body(ClientKind::ClaudeCode, &body).is_none());
    }

    #[test]
    fn dsh_is_detected_from_its_auth_cookie() {
        // `wmda_uuid` is browser state (the DSH console is also opened in a
        // browser); only the `dsh-auth-*` cookie name is DSH's own identity.
        let h = headers(&[(
            "cookie",
            "wmda_uuid=9cca2d0b5213b80bc5700de7aa6d8f34; \
             dsh-auth-VPhEEcLKeqRDBoBalzN2Nm7CnfxKhLE00pKIDWxt1sw=v1.eyJ2ZXJzaW9uIjoxfQ.AbwTOi",
        )]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::Dsh);
        assert_eq!(
            info.session_id,
            "dsh:dsh-auth-VPhEEcLKeqRDBoBalzN2Nm7CnfxKhLE00pKIDWxt1sw"
        );
    }

    #[test]
    fn dsh_session_id_is_stable_across_requests() {
        // The `dsh-auth-*` cookie name is the long-lived identity; its value is
        // a rotating signed token, so two requests from the same browser must
        // still land under the same session.
        let a = detect(&headers(&[("cookie", "dsh-auth-KEY=v1.aaa.AbwTOi_Nks0")]));
        let b = detect(&headers(&[("cookie", "dsh-auth-KEY=v1.bbb.OtherSig")]));
        assert_eq!(a.session_id, b.session_id);
    }

    #[test]
    fn browser_requests_do_not_invent_a_session() {
        // A plain `curl`/browser probe carries none of the dialect markers.
        let info = detect(&headers(&[("user-agent", "curl/8.7.1")]));
        assert_eq!(info.client, ClientKind::Unknown);
        assert!(!info.is_known());
        assert_eq!(info.log_tag(), "client=unknown");
        assert_eq!(info.display_tag(), "[unknown]");
    }

    #[test]
    fn short_header_values_are_not_mistaken_for_session_ids() {
        // `sec-ch-ua-mobile: ?0` style values must never become a session id.
        assert!(!detect(&headers(&[("x-session-id", "1")])).is_known());
        assert!(!detect(&headers(&[("session-id", "?0")])).is_known());
    }

    #[test]
    fn dsh_harness_session_header_wins_when_the_client_sends_one() {
        // Preferred source, kept for a future harness build that starts sending
        // it: the declared header names the conversation, not just the client.
        let h = headers(&[
            ("content-type", "application/json"),
            (
                "user-agent",
                "deepseek-harness/0.1.6-alpha.2 (+https://github.com/deepseek-ai/deepseek-harness)",
            ),
            (
                "x-deepseek-harness-session-id",
                "session-7d3f2a10-9c4b-4e21-8f77-1a2b3c4d5e6f",
            ),
        ]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::Dsh);
        assert_eq!(
            info.session_id,
            "dsh:session-7d3f2a10-9c4b-4e21-8f77-1a2b3c4d5e6f"
        );
    }

    #[test]
    fn dsh_falls_back_to_the_user_agent_as_a_client_level_id() {
        // The shipped 0.1.6-alpha.2 build sends no session header and no
        // session-shaped body field, so the client is keyed by its user-agent.
        // Every chat from that build therefore shares one id — a deliberate
        // trade, since a client-level grouping that works beats a
        // conversation-level one that never matches.
        let h = headers(&[
            ("content-type", "application/json"),
            ("authorization", "Bearer ck_example"),
            (
                "user-agent",
                "deepseek-harness/0.1.6-alpha.2 (+https://github.com/deepseek-ai/deepseek-harness)",
            ),
            ("sec-fetch-mode", "cors"),
        ]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::Dsh);
        assert_eq!(info.session_id, "dsh:0.1.6-alpha.2");
        assert_eq!(info.log_tag(), "client=dsh session_id=dsh:0.1.6-alpha.2");
    }

    #[test]
    fn dsh_client_level_id_beats_a_per_install_cookie() {
        // The cookie is only per-install, so keying on it would merge every
        // install behind one id; the user-agent is at least per-version.
        let h = headers(&[
            ("cookie", "dsh-auth-KEY=v1.sig"),
            (
                "user-agent",
                "deepseek-harness/0.1.6-alpha.2 (+https://github.com/deepseek-ai/deepseek-harness)",
            ),
        ]);
        let info = detect(&h);
        assert_eq!(info.client, ClientKind::Dsh);
        assert_eq!(info.session_id, "dsh:0.1.6-alpha.2");
    }

    #[test]
    fn dsh_user_id_header_alone_identifies_the_client() {
        // The user-id header marks the dialect even when no user-agent or
        // session id is present.
        let h = headers(&[(
            "x-deepseek-harness-user-id",
            "anon-2f8a1c4e-11b4-4d0a-92f0-6c1d2e3f4a5b",
        )]);
        assert_eq!(detect_client(&h), ClientKind::Dsh);
    }

    #[test]
    fn a_junk_session_header_is_rejected_rather_than_used() {
        // A too-short value is not an id; with no user-agent to fall back to,
        // the request stays unidentified instead of inventing one.
        assert!(!detect(&headers(&[("x-deepseek-harness-session-id", "?0")])).is_known());
    }

    #[test]
    fn dsh_is_detected_from_its_real_user_agent() {
        // The app calls itself `deepseek-harness/…`, which does not contain the
        // substring `dsh`; matching on `dsh` alone missed every real request.
        let ua =
            "deepseek-harness/0.1.6-alpha.2 (+https://github.com/deepseek-ai/deepseek-harness)";
        assert_eq!(
            detect_client(&headers(&[("user-agent", ua)])),
            ClientKind::Dsh
        );
    }

    #[test]
    fn browser_client_with_a_dsh_cookie_is_the_dsh_app() {
        // The real log shows `GET /v1/models` from Chrome *carrying* the DSH
        // cookie. The cookie is the strongest signal available, so attribute it
        // to DSH rather than to an anonymous browser.
        let h = headers(&[
            (
                "user-agent",
                "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7)",
            ),
            ("cookie", "wmda_uuid=9cca2d0b; dsh-auth-KEY=v1.sig"),
        ]);
        assert_eq!(detect(&h).client, ClientKind::Dsh);
    }

    #[test]
    fn header_session_id_beats_a_body_session_id() {
        let h = headers(&[("originator", "Codex"), ("session-id", CODEX_SESSION)]);
        let body =
            serde_json::json!({ "prompt_cache_key": "01a0cc99-9999-7999-8999-999999999999" });
        let merged = merge(detect(&h), detect_from_body(ClientKind::Codex, &body));
        assert_eq!(merged.session_id, format!("codex:{}", CODEX_SESSION));
    }

    #[test]
    fn codex_prompt_cache_key_is_the_body_fallback() {
        let body =
            serde_json::json!({ "prompt_cache_key": "01a0cc99-9999-7999-8999-999999999999" });
        let info = detect_from_body(ClientKind::Codex, &body).unwrap();
        assert_eq!(
            info.session_id,
            "codex:01a0cc99-9999-7999-8999-999999999999"
        );
    }

    #[test]
    fn turn_metadata_without_session_id_falls_through_to_headers() {
        let h = headers(&[
            ("originator", "Codex"),
            (
                "x-codex-turn-metadata",
                &format!(r#"{{"turn_id":"{CODEX_TURN}"}}"#),
            ),
            ("session-id", CODEX_SESSION),
        ]);
        assert_eq!(detect(&h).session_id, format!("codex:{}", CODEX_SESSION));
    }

    #[test]
    fn cookie_values_containing_equals_are_parsed_whole() {
        let h = headers(&[("cookie", &format!("session_id={CODEX_SESSION}; jwt=a=b=c"))]);
        assert_eq!(detect(&h).session_id, format!("claude:{}", CODEX_SESSION));
    }

    #[test]
    fn log_tag_carries_the_greppable_session_id() {
        let info = SessionInfo {
            client: ClientKind::Codex,
            session_id: format!("codex:{}", CODEX_SESSION),
            turn_id: CODEX_TURN.to_string(),
            model_hint: None,
        };
        assert_eq!(
            info.log_tag(),
            format!(
                "client=codex session_id=codex:{} turn={}",
                CODEX_SESSION, CODEX_TURN
            )
        );
    }

    #[test]
    fn display_tag_shortens_a_long_session_id() {
        let info = SessionInfo {
            client: ClientKind::Codex,
            session_id: format!("codex:{}", CODEX_SESSION),
            turn_id: CODEX_TURN.to_string(),
            model_hint: None,
        };
        assert_eq!(info.display_tag(), "[codex:01a0cc30 / turn 01a0cc31]");
    }

    #[test]
    fn display_tag_for_a_turn_less_session_keeps_the_full_id() {
        let info = SessionInfo {
            client: ClientKind::ClaudeCode,
            session_id: "claude:user_8f3a1c2e".to_string(),
            turn_id: String::new(),
            model_hint: None,
        };
        assert_eq!(info.display_tag(), "[claude:user_8f3a1c2e]");
    }

    #[test]
    fn model_from_body_is_trimmed_and_ignores_blanks() {
        assert_eq!(
            model_from_body(&serde_json::json!({ "model": " claude-sonnet-4-5 " })),
            Some("claude-sonnet-4-5".to_string())
        );
        assert_eq!(model_from_body(&serde_json::json!({ "model": "  " })), None);
        assert_eq!(model_from_body(&serde_json::json!({})), None);
    }
}
