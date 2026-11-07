//! Request-log wire shapes and the row/record conversions.

use super::*;

/// Token breakdown for a single request.
#[derive(Debug, Clone, Default)]
pub struct TokenRecord {
    /// Uncached prompt tokens
    pub input: i64,
    /// Cache-read prompt tokens (served from cache)
    pub cache_read: i64,
    /// Cache-write tokens (new cache entries written)
    pub cache_write: i64,
    /// Output / completion tokens
    pub output: i64,
}

/// Upstream-side timings for one request, all relative to the moment the
/// winning upstream attempt was sent.
///
/// These are what make TTFT / TPS / tool-wait measurable. Everything defaults
/// to zero, which means "not observable on this path" — a plain non-streamed
/// JSON response never exposes a first-token instant, so its TTFT stays 0 and
/// the UI renders `—` rather than a fabricated number.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct UpstreamTiming {
    /// Milliseconds from sending the upstream request to the first output
    /// delta (first content token, or the first tool-call delta on a turn that
    /// opens with a tool call). 0 when it could not be observed.
    pub ttft_ms: i64,
    /// Milliseconds from sending the upstream request to the *last* output
    /// delta: the model's own generation span.
    ///
    /// Deliberately not the request duration. A streaming generator is pulled
    /// by the client, so the request also contains however long the client sat
    /// on the final chunk; measuring to the last delta is what keeps this a
    /// property of the model rather than of the client.
    pub model_ms: i64,
    /// Whether the turn ended by asking for a tool call.
    ///
    /// The proxy never runs tools — the client does, and reports back in a
    /// later request on the same session. This flag is what lets the session
    /// query attribute the *gap* between those two requests to tool execution.
    pub ended_with_tool_call: bool,
}

impl UpstreamTiming {
    /// Tokens per second over the model's own generation span, or `None` when
    /// either half is missing (no tokens, or no observable span).
    pub fn tps(&self, output_tokens: i64) -> Option<f64> {
        if output_tokens <= 0 || self.model_ms <= 0 {
            return None;
        }
        Some(output_tokens as f64 * 1000.0 / self.model_ms as f64)
    }
}

/// Everything captured about one completed request for the stats DB.
#[derive(Debug, Clone)]
pub struct RequestOutcome<'a> {
    /// Served upstream model name (after virtual `free` resolution and any
    /// `model_map`). This is the model that actually generated the tokens, so
    /// usage accounting groups by it — a `free` request lands under `hy3` or
    /// `hy4-preview`, never under `free`.
    pub model: &'a str,
    /// Client-facing model name as requested (before upstream mapping). For a
    /// virtual `free` request this is `free`, while `model` is the resolved
    /// `hy3`/`hy4-preview`; the two differ only on the virtual node, which is
    /// exactly why both are kept.
    pub client_model: &'a str,
    /// Route that served the request, e.g. `/v1/messages`.
    pub route: &'a str,
    pub tokens: &'a TokenRecord,
    pub duration_ms: i64,
    /// Upstream TTFT / model span / tool-terminated flag (see [`UpstreamTiming`]).
    pub timing: UpstreamTiming,
    pub streamed: bool,
    /// HTTP status returned to the client.
    pub status: u16,
    /// Error message when the request failed.
    pub error: Option<&'a str>,
    /// Namespaced session id resolved by `session::detect`, e.g.
    /// `codex:01a0cc30-….` Empty when the client could not be identified.
    pub session_id: &'a str,
    /// Namespaced id of the session that *spawned* this one, or empty for a
    /// top-level conversation.
    ///
    /// Only a client that knows its own session lineage can report this, and
    /// only DSH does (`x-deepseek-harness-parent-session-id`, sent for subagent
    /// children alone). Everything else stores empty — which is the same value
    /// historical rows carry, because they were recorded before the header
    /// existed and cannot be backfilled.
    pub parent_session_id: &'a str,
    /// Client dialect tag (`codex` / `claude` / `dsh` / `unknown`).
    pub client: &'a str,
    /// Credential short id actually used after an exception override (failover
    /// session switch, degraded retry, token refresh, …). Empty when the
    /// request completed on its originally selected credential.
    pub override_key: &'a str,
    /// Upstream model name selected by an exception override (e.g. the degraded
    /// retry's model). Empty when no override changed the model.
    pub override_model: &'a str,
    /// Why the override(s) fired, e.g. `429 from wb-a1; content_blocked 400`.
    /// Empty when the request ran exactly as configured.
    pub override_reason: &'a str,
}

impl RequestOutcome<'_> {
    /// Freeze into an owned row that can cross the writer-thread channel.
    pub(crate) fn to_row(&self, date: &str, created_at: &str) -> RequestRow {
        RequestRow {
            date: date.to_string(),
            created_at: created_at.to_string(),
            model: self.model.to_string(),
            client_model: self.client_model.to_string(),
            route: self.route.to_string(),
            input_tokens: self.tokens.input,
            output_tokens: self.tokens.output,
            cache_read_tokens: self.tokens.cache_read,
            cache_write_tokens: self.tokens.cache_write,
            duration_ms: self.duration_ms,
            ttft_ms: self.timing.ttft_ms,
            model_ms: self.timing.model_ms,
            ended_with_tool_call: self.timing.ended_with_tool_call,
            streamed: self.streamed,
            status: self.status,
            error: self.error.map(|e| e.to_string()),
            session_id: self.session_id.to_string(),
            parent_session_id: self.parent_session_id.to_string(),
            client: self.client.to_string(),
            override_key: self.override_key.to_string(),
            override_model: self.override_model.to_string(),
            override_reason: self.override_reason.to_string(),
        }
    }
}

/// Owned counterpart of [`RequestOutcome`], sent to the writer thread.
#[derive(Debug, Clone)]
pub(crate) struct RequestRow {
    date: String,
    created_at: String,
    model: String,
    client_model: String,
    route: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    duration_ms: i64,
    /// Milliseconds to first output delta (0 = not observable on this path).
    ttft_ms: i64,
    /// Milliseconds from send to last output delta (0 = not observable).
    model_ms: i64,
    /// Whether the turn finished by requesting a tool call.
    ended_with_tool_call: bool,
    streamed: bool,
    status: u16,
    error: Option<String>,
    session_id: String,
    /// Parent session for a subagent child, else empty (see
    /// [`RequestOutcome::parent_session_id`]).
    parent_session_id: String,
    client: String,
    override_key: String,
    override_model: String,
    override_reason: String,
}

impl RequestRow {
    pub(crate) fn insert_sql() -> &'static str {
        "INSERT INTO request_logs (
            date, created_at, model, client_model, route,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            duration_ms, ttft_ms, model_ms, ended_with_tool_call,
            streamed, status, error, session_id, parent_session_id, client,
            override_key, override_model, override_reason
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22)"
    }

    pub(crate) fn bind_to(&self, stmt: &mut rusqlite::Statement<'_>) -> rusqlite::Result<()> {
        stmt.execute(params![
            self.date,
            self.created_at,
            self.model,
            self.client_model,
            self.route,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            self.duration_ms,
            self.ttft_ms,
            self.model_ms,
            if self.ended_with_tool_call { 1 } else { 0 },
            if self.streamed { 1 } else { 0 },
            self.status as i64,
            self.error,
            self.session_id,
            self.parent_session_id,
            self.client,
            self.override_key,
            self.override_model,
            self.override_reason,
        ])?;
        Ok(())
    }
}

/// A single request log entry stored in the database.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLogItem {
    pub id: i64,
    pub created_at: String,
    pub model: String,
    pub client_model: String,
    pub route: String,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_write_tokens: i64,
    pub duration_ms: i64,
    /// Milliseconds from upstream send to the first output delta; 0 when the
    /// path could not observe one (a plain non-streamed JSON response).
    pub ttft_ms: i64,
    /// Milliseconds from upstream send to the last output delta: the model's own
    /// generation span, excluding how long the client then sat on the stream.
    pub model_ms: i64,
    /// Whether the turn finished by requesting a tool call.
    pub ended_with_tool_call: bool,
    pub streamed: bool,
    pub status: u16,
    pub error: Option<String>,
    /// Namespaced session id (`codex:…` / `claude:…` / `dsh:…`), or empty.
    pub session_id: String,
    /// Namespaced parent session id for a subagent child, or empty for a
    /// top-level conversation (and for every row recorded before the column
    /// existed — an absence that is permanent, not a measurement of "no parent").
    pub parent_session_id: String,
    /// Client dialect tag, or `unknown`.
    pub client: String,
    /// Credential short id used after an exception override, or empty when the
    /// request ran on its originally selected credential.
    pub override_key: String,
    /// Upstream model chosen by an exception override, or empty.
    pub override_model: String,
    /// Why the override fired, or empty when nothing was overridden.
    pub override_reason: String,
}

/// Filter criteria for querying request logs.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestLogFilter {
    pub model: Option<String>,
    /// Restrict to the model the client asked for (before virtual `free`
    /// resolution). Lets the GUI filter `free` requests even though their
    /// `model` column holds the resolved `hy3`/`hy4-preview`.
    pub client_model: Option<String>,
    /// Restrict to one client dialect: `codex` / `claude` / `dsh` / `unknown`.
    pub client: Option<String>,
    /// Restrict to one exact session id (`codex:01a0cc30-…`).
    pub session_id: Option<String>,
    pub status_group: Option<String>,
    pub streamed: Option<bool>,
    pub search: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}

/// One distinct conversation, for the session filter dropdown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLogSession {
    /// Namespaced id as stored (`codex:<uuid>`, `claude:<uuid>`, `dsh:<key>`).
    pub session_id: String,
    /// Owning dialect, shown beside the id so two clients' ids stay tellable apart.
    pub client: String,
}

/// Query result containing matched items, total count and model list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestLogsResult {
    pub items: Vec<RequestLogItem>,
    pub total: i64,
    pub models: Vec<String>,
    /// Distinct *requested* models (before virtual `free` resolution), for the
    /// filter UI. Lets the GUI offer `free` even though its `model` column holds
    /// the resolved `hy3`/`hy4-preview`.
    pub client_models: Vec<String>,
    /// Distinct client dialects present in the database, for the filter UI.
    pub clients: Vec<String>,
    /// Distinct conversations, newest first. Queried over the whole table rather
    /// than derived from `items`, so a chat whose latest request is on an older
    /// page can still be selected.
    pub sessions: Vec<RequestLogSession>,
}
