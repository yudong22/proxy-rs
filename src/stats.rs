use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::settings::data_dir;

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
    /// Client-facing model name (before upstream mapping).
    pub model: &'a str,
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
    fn to_row(&self, date: &str, created_at: &str) -> RequestRow {
        RequestRow {
            date: date.to_string(),
            created_at: created_at.to_string(),
            model: self.model.to_string(),
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
            client: self.client.to_string(),
            override_key: self.override_key.to_string(),
            override_model: self.override_model.to_string(),
            override_reason: self.override_reason.to_string(),
        }
    }
}

/// Owned counterpart of [`RequestOutcome`], sent to the writer thread.
#[derive(Debug, Clone)]
struct RequestRow {
    date: String,
    created_at: String,
    model: String,
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
    client: String,
    override_key: String,
    override_model: String,
    override_reason: String,
}

impl RequestRow {
    fn insert_sql() -> &'static str {
        "INSERT INTO request_logs (
            date, created_at, model, route,
            input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
            duration_ms, ttft_ms, model_ms, ended_with_tool_call,
            streamed, status, error, session_id, client,
            override_key, override_model, override_reason
        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20)"
    }

    fn bind_to(&self, stmt: &mut rusqlite::Statement<'_>) -> rusqlite::Result<()> {
        stmt.execute(params![
            self.date,
            self.created_at,
            self.model,
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
    /// Distinct client dialects present in the database, for the filter UI.
    pub clients: Vec<String>,
    /// Distinct conversations, newest first. Queried over the whole table rather
    /// than derived from `items`, so a chat whose latest request is on an older
    /// page can still be selected.
    pub sessions: Vec<RequestLogSession>,
}

/// One completed upstream turn within a session, as the speed card needs it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionTurn {
    pub id: i64,
    pub created_at: String,
    pub model: String,
    /// Prompt tokens billed as fresh input.
    pub input_tokens: i64,
    pub output_tokens: i64,
    /// Prompt tokens served from the upstream's cache.
    pub cache_read_tokens: i64,
    /// End-to-end request duration (includes the client's read tail).
    pub duration_ms: i64,
    pub ttft_ms: i64,
    /// The model's own generation span (send → last output delta).
    pub model_ms: i64,
    pub ended_with_tool_call: bool,
}

impl SessionTurn {
    /// Whether this turn contributed a real generation span.
    ///
    /// An unmeasured row (a plain non-streamed reply, or one served before
    /// timing existed) has `model_ms == 0` and must not be counted as a zero-
    /// length generation — that would drag the session's speed to zero.
    pub fn is_measured(&self) -> bool {
        self.model_ms > 0
    }

    /// Output speed over the model's generation span. `None` when unmeasurable.
    pub fn tps(&self) -> Option<f64> {
        if self.output_tokens <= 0 || self.model_ms <= 0 {
            return None;
        }
        Some(self.output_tokens as f64 * 1000.0 / self.model_ms as f64)
    }
}

/// Hard cap on how many recent turns one session aggregate folds in.
///
/// The aggregate is a cumulative rate, so it is deliberately windowed rather
/// than unbounded: an all-time figure would stop responding to the session
/// getting faster or slower, while a single-turn figure jitters wildly. 3000
/// turns is far beyond any real conversation's useful memory while keeping the
/// query's work bounded.
pub const SESSION_METRICS_MAX_TURNS: i64 = 3000;

/// Session-level speed metrics, backing the overview's 输出速度 card.
///
/// **Aggregated over the session's recent turns, not the latest one.** A single
/// turn's speed swings by an order of magnitude (a short reply spends most of
/// its span in the first-token gap), so the card used to change on every poll.
/// Aggregating fixes that the correct way for a *rate*: sum the output tokens and
/// sum the generation time, then divide once.
///
/// ```text
///   tps = Σ output_tokens / Σ model_ms
/// ```
///
/// That is a time-weighted mean. Averaging the per-turn `tok/s` values instead
/// would be wrong: it would let a 3-token reply and a 3000-token reply count
/// equally.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionMetrics {
    pub session_id: String,
    /// Turns folded in (≤ [`SESSION_METRICS_MAX_TURNS`]).
    pub turns: i64,
    /// How many of those carried a real generation span.
    pub measured_turns: i64,
    /// Model of the most recent turn, for the panel.
    pub model: Option<String>,
    /// When the most recent turn finished.
    pub last_at: Option<String>,
    /// Σ output tokens, over measured turns only.
    pub output_tokens: i64,
    /// Σ model generation span (ms), over measured turns only.
    pub model_ms: i64,
    /// Σ measured TTFTs (ms) and how many were measured, for the average.
    pub ttft_sum_ms: i64,
    pub ttft_samples: i64,
    /// Σ tool wait (ms) attributed within this window, and how many gaps.
    pub tool_wait_ms: i64,
    pub tool_waits: i64,
    /// Σ fresh input tokens across the window (all turns, measured or not —
    /// this is a token count, not a rate, so an unmeasured turn still counts).
    pub input_tokens: i64,
    /// Σ prompt tokens served from cache across the same window.
    pub cache_read_tokens: i64,
}

impl SessionMetrics {
    /// Session output speed: total tokens over total generation time.
    /// `None` when nothing in the window was measurable.
    pub fn tps(&self) -> Option<f64> {
        if self.output_tokens <= 0 || self.model_ms <= 0 {
            return None;
        }
        Some(self.output_tokens as f64 * 1000.0 / self.model_ms as f64)
    }

    /// Mean TTFT across the measured turns, or `None` when none was measured.
    pub fn avg_ttft_ms(&self) -> Option<i64> {
        if self.ttft_samples <= 0 {
            return None;
        }
        Some(self.ttft_sum_ms / self.ttft_samples)
    }

    /// Total tool time attributed in the window, or `None` when no tool gap was
    /// observable (so the panel shows "—" rather than a bare 0).
    pub fn tool_wait_ms(&self) -> Option<i64> {
        if self.tool_waits <= 0 {
            return None;
        }
        Some(self.tool_wait_ms)
    }

    /// Cache-hit percentage over the window: cache_read / (input + cache_read).
    ///
    /// Same formula as [`DayStats::cache_hit_pct`], so a session's figure and
    /// the overview's daily figure mean the same thing. `None` when the window
    /// carried no prompt traffic at all: a session with no requests has no hit
    /// rate, and showing "0%" would read as "nothing was cached" rather than
    /// "nothing was measured" — the same distinction the speed card draws
    /// between a real 0 and an unmeasured value.
    pub fn cache_hit_pct(&self) -> Option<i64> {
        let denom = self.input_tokens + self.cache_read_tokens;
        if denom <= 0 {
            return None;
        }
        Some(self.cache_read_tokens * 100 / denom)
    }
}

/// Fold a session's turns (oldest first) into one aggregate.
///
/// Pure and separate from the query so the weighting rules are unit-testable
/// without a database.
///
/// Tool time is accumulated only where a tool-requesting turn is immediately
/// followed by another turn in the same session — see [`tool_wait_ms`] for why
/// the arithmetic subtracts the follower's own duration, and why an implausible
/// or negative gap counts as no measurement rather than as zero.
fn fold_session_turns(session_id: &str, turns: &[SessionTurn]) -> SessionMetrics {
    let mut metrics = SessionMetrics {
        session_id: session_id.to_string(),
        turns: turns.len() as i64,
        ..Default::default()
    };

    let mut previous: Option<&SessionTurn> = None;
    for turn in turns {
        if turn.is_measured() {
            metrics.measured_turns += 1;
            metrics.output_tokens += turn.output_tokens;
            metrics.model_ms += turn.model_ms;
        }
        // Token totals count every turn: they are quantities, not rates, so a
        // turn with no timing still contributed prompt traffic.
        metrics.input_tokens += turn.input_tokens;
        metrics.cache_read_tokens += turn.cache_read_tokens;
        if turn.ttft_ms > 0 {
            metrics.ttft_samples += 1;
            metrics.ttft_sum_ms += turn.ttft_ms;
        }
        if let Some(prev) = previous {
            if prev.ended_with_tool_call {
                if let Some(gap) =
                    tool_wait_ms(&prev.created_at, &turn.created_at, turn.duration_ms)
                {
                    metrics.tool_wait_ms += gap;
                    metrics.tool_waits += 1;
                }
            }
        }
        previous = Some(turn);
    }

    if let Some(last) = turns.last() {
        metrics.model = Some(last.model.clone());
        metrics.last_at = Some(last.created_at.clone());
    }

    metrics
}

/// Milliseconds the client spent running tools, derived from the gap between a
/// tool-requesting turn and its successor.
///
/// Both `created_at` values are stamped when the row is *written* — i.e. at the
/// **end** of each request, not its start (verified against the running app: a
/// request that took 1.3 s was stamped as it completed). So the raw difference
/// between the two stamps is *not* the tool wait; it also contains the current
/// turn's own duration. Subtracting that duration backs out the moment this turn
/// began, which is what puts the gap on the client:
///
/// ```text
///   tool wait = (cur_end - prev_end) - cur_duration
///             = cur_start - prev_end
/// ```
///
/// Timestamps are stored at second granularity, so the result is accurate to
/// about a second — fine for a tool that takes seconds to minutes, and the UI
/// shows one decimal.
///
/// Returns `None` when either timestamp is unparseable or the result is negative
/// or implausibly large: a session resumed hours later must not present as hours
/// of tool execution, which would be a worse lie than showing nothing.
fn tool_wait_ms(prev_at: &str, cur_at: &str, cur_duration_ms: i64) -> Option<i64> {
    /// Above this, a "tool call" is really a paused/resumed conversation.
    const MAX_PLAUSIBLE_TOOL_WAIT_MS: i64 = 10 * 60 * 1000;

    let prev_end = parse_local_datetime_secs(prev_at)?;
    let cur_end = parse_local_datetime_secs(cur_at)?;
    let wait = (cur_end - prev_end) * 1000 - cur_duration_ms.max(0);
    if !(0..=MAX_PLAUSIBLE_TOOL_WAIT_MS).contains(&wait) {
        return None;
    }
    Some(wait)
}

/// Parse the stats DB's local `YYYY-MM-DD HH:MM:SS` stamp into epoch seconds.
///
/// Deliberately hand-rolled: the column is written by this module in one fixed
/// local-time format, and pulling in a date-time crate (or re-deriving the UTC
/// offset, which can have changed between the two rows across a DST boundary)
/// would add a dependency for one subtraction. Two timestamps in the same local
/// format differ by the same amount as the instants they denote.
fn parse_local_datetime_secs(value: &str) -> Option<i64> {
    let bytes = value.as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let year: i64 = value.get(0..4)?.parse().ok()?;
    let month: i64 = value.get(5..7)?.parse().ok()?;
    let day: i64 = value.get(8..10)?.parse().ok()?;
    let hour: i64 = value.get(11..13)?.parse().ok()?;
    let min: i64 = value.get(14..16)?.parse().ok()?;
    let sec: i64 = value.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || min > 59 || sec > 60 {
        return None;
    }
    // Days since the Unix epoch, proleptic Gregorian — enough for the 1970+ and
    // 2100- ranges this table can hold.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86_400 + hour * 3600 + min * 60 + sec)
}

/// Aggregated statistics for a single calendar day.
/// Derived on demand from `request_logs` — there is no separate daily table, so
/// the counters can never drift out of sync with the rows they summarize.
#[derive(Debug, Clone, Default)]
pub struct DayStats {
    pub date: String,
    pub requests_total: i64,
    pub requests_success: i64,
    pub requests_failed: i64,
    pub tokens_input: i64,
    pub tokens_cache_read: i64,
    pub tokens_cache_write: i64,
    pub tokens_output: i64,
    /// Requests whose credential/model was changed by an exception override
    /// (failover, degraded retry, token refresh). Empty `override_key` rows do
    /// not count: a request served exactly as configured is not an override.
    pub overrides_total: i64,
}

impl DayStats {
    /// Total tokens consumed (all input categories + output).
    pub fn tokens_total(&self) -> i64 {
        self.tokens_input + self.tokens_cache_read + self.tokens_cache_write + self.tokens_output
    }

    /// Cache-hit percentage: cache_read / (input + cache_read) × 100.
    /// Returns 0 when there is no input traffic.
    pub fn cache_hit_pct(&self) -> i64 {
        let denom = self.tokens_input + self.tokens_cache_read;
        if denom == 0 {
            0
        } else {
            self.tokens_cache_read * 100 / denom
        }
    }
}

/// Persistent request-log store backed by a local SQLite database.
///
/// The database lives at `~/.proxy-rs/stats.db` and holds a single
/// `request_logs` table — one row per finished request. Daily aggregates are
/// computed from it with `GROUP BY date`.
///
/// Writes never run on a request handler: `record_request_log` pushes the row
/// onto an unbounded queue and returns, and a dedicated writer thread drains
/// that queue in batches inside a transaction. So a slow disk or a lock
/// contention with the GUI's read queries can never stall a proxied request.
///
/// Queries use their own connection; WAL mode lets them read while the writer
/// thread holds the write lock.
///
/// For a file-backed database, reads draw from a small pool instead of sharing
/// one connection. WAL already permits concurrent readers at the SQLite level,
/// so the bottleneck was the single `Mutex` serializing them: the GUI polls
/// both the stats line and the request-log table every two seconds, and one
/// shared lock made those queries queue behind each other.
///
/// An in-memory database cannot be shared across connections — a second
/// connection would be a *different*, empty database — so those fall back to
/// the single shared handle, which is correct for tests and the non-persistent
/// fallback the GUI uses when the real file cannot be opened.
pub struct StatsDb {
    /// Read connections, checked out for the duration of a query. Only used
    /// when `reader_source` is set; otherwise `conn` serves every read.
    readers: Mutex<Vec<Connection>>,
    /// Path to open additional read connections from. `None` for in-memory
    /// databases, which cannot be reopened.
    reader_source: Option<PathBuf>,
    /// The connection used for writes in inline mode (no writer thread) and for
    /// every read when the database is in-memory.
    conn: Arc<Mutex<Connection>>,
    /// Queue feeding the background writer thread. `None` in inline mode,
    /// where writes go straight through `conn`.
    queue: Option<Sender<RequestRow>>,
}

/// Upper bound on pooled read connections.
///
/// The GUI issues two pollers; a handful of spare connections covers that plus
/// an ad-hoc query, without opening unbounded handles on a desktop app.
const MAX_READ_CONNS: usize = 4;

/// Where a read should come from: a pooled connection (file-backed) or the
/// single shared handle (in-memory).
enum ReadHandle<'a> {
    Pooled(ReadConn<'a>),
    Shared(std::sync::MutexGuard<'a, Connection>),
}

impl ReadHandle<'_> {
    fn get(&self) -> &Connection {
        match self {
            ReadHandle::Pooled(r) => r.get(),
            ReadHandle::Shared(g) => g,
        }
    }
}

/// A read connection borrowed from the pool, returned on drop.
struct ReadConn<'a> {
    pool: &'a Mutex<Vec<Connection>>,
    conn: Option<Connection>,
}

impl ReadConn<'_> {
    fn get(&self) -> &Connection {
        // `Some` for the guard's whole life; `take` only happens in `Drop`.
        self.conn.as_ref().expect("read connection checked out")
    }
}

impl Drop for ReadConn<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if let Ok(mut pool) = self.pool.lock() {
                pool.push(conn);
            }
        }
    }
}

impl StatsDb {
    /// Open (or create) the statistics database, ensure the schema exists, and
    /// spawn the background writer thread.
    pub fn open() -> Result<Arc<Self>> {
        let path = match data_dir() {
            Some(dir) => dir.join("stats.db"),
            None => {
                return Err(anyhow::anyhow!(
                    "Cannot determine data directory for stats.db"
                ))
            }
        };
        Self::open_at(&path)
    }

    /// Open a file-backed database at an explicit path.
    ///
    /// Split from [`open`] so tests can use a temp file instead of the user's
    /// real `~/.proxy-rs/stats.db`.
    pub fn open_at(path: &std::path::Path) -> Result<Arc<Self>> {
        let seed_conn = Self::connect(path)?;
        Self::init_schema(&seed_conn)?;
        let write_conn = Self::connect(path)?;

        let (tx, rx) = channel::<RequestRow>();
        std::thread::Builder::new()
            .name("stats-writer".to_string())
            .spawn(move || writer_loop(write_conn, rx))?;

        Ok(Arc::new(Self {
            readers: Mutex::new(vec![seed_conn]),
            reader_source: Some(path.to_path_buf()),
            conn: Arc::new(Mutex::new(Connection::open_in_memory()?)),
            queue: Some(tx),
        }))
    }

    /// Construct from an existing connection (e.g. an in-memory DB for tests).
    ///
    /// Inline mode: no writer thread, so writes are synchronous and immediately
    /// visible to queries.
    pub fn from_conn(conn: Connection) -> Self {
        Self {
            readers: Mutex::new(Vec::new()),
            reader_source: None,
            conn: Arc::new(Mutex::new(conn)),
            queue: None,
        }
    }

    /// Open a fresh in-memory database with the schema applied. Useful as a
    /// non-persistent fallback and for tests.
    pub fn in_memory() -> Result<Arc<Self>> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Arc::new(Self {
            readers: Mutex::new(Vec::new()),
            reader_source: None,
            conn: Arc::new(Mutex::new(conn)),
            queue: None,
        }))
    }

    /// Borrow a connection for a read query.
    ///
    /// A file-backed database hands out a pooled connection so concurrent
    /// readers do not serialize on one lock. In-memory databases share the
    /// single handle: a second connection would be a different, empty database.
    fn read_conn(&self) -> ReadHandle<'_> {
        let Some(path) = self.reader_source.as_ref() else {
            return ReadHandle::Shared(self.conn.lock().unwrap_or_else(|p| p.into_inner()));
        };

        if let Some(conn) = self.readers.lock().unwrap_or_else(|p| p.into_inner()).pop() {
            return ReadHandle::Pooled(ReadConn {
                pool: &self.readers,
                conn: Some(conn),
            });
        }

        // Pool empty: open another up to the cap, else share the single handle
        // rather than making the reader wait.
        let room = self
            .readers
            .lock()
            .map(|p| p.len() < MAX_READ_CONNS)
            .unwrap_or(false);
        match room.then(|| Self::connect(path).ok()).flatten() {
            Some(conn) => ReadHandle::Pooled(ReadConn {
                pool: &self.readers,
                conn: Some(conn),
            }),
            None => ReadHandle::Shared(self.conn.lock().unwrap_or_else(|p| p.into_inner())),
        }
    }

    fn connect(path: &std::path::Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
        // Enable WAL mode so the GUI can read while the writer thread writes.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        // Wait rather than fail outright if the write lock is momentarily held.
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS request_logs (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                date                TEXT NOT NULL DEFAULT '',
                created_at          TEXT NOT NULL,
                model               TEXT NOT NULL,
                route               TEXT NOT NULL,
                input_tokens        INTEGER NOT NULL DEFAULT 0,
                output_tokens       INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
                duration_ms         INTEGER NOT NULL DEFAULT 0,
                ttft_ms             INTEGER NOT NULL DEFAULT 0,
                model_ms            INTEGER NOT NULL DEFAULT 0,
                ended_with_tool_call INTEGER NOT NULL DEFAULT 0,
                streamed            INTEGER NOT NULL DEFAULT 0,
                status              INTEGER NOT NULL DEFAULT 0,
                error               TEXT,
                session_id          TEXT NOT NULL DEFAULT '',
                client              TEXT NOT NULL DEFAULT '',
                override_key        TEXT NOT NULL DEFAULT '',
                override_model      TEXT NOT NULL DEFAULT '',
                override_reason     TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_request_logs_id_desc ON request_logs(id DESC);
            CREATE INDEX IF NOT EXISTS idx_request_logs_created_at ON request_logs(created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_request_logs_model ON request_logs(model);
            CREATE INDEX IF NOT EXISTS idx_request_logs_status ON request_logs(status);",
        )?;
        // Must run before the `date` index is created, since the column only
        // exists after migration on databases from older builds.
        Self::migrate(conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_date ON request_logs(date);
             CREATE INDEX IF NOT EXISTS idx_request_logs_session_id
                 ON request_logs(session_id);",
        )?;
        Ok(())
    }

    /// Bring a database written by an older version up to the current schema.
    ///
    /// Older builds kept a separate `daily_stats` table and had no `date`
    /// column; both are reconciled here so existing installs keep their history.
    fn migrate(conn: &Connection) -> Result<()> {
        let has_date = conn
            .prepare("PRAGMA table_info(request_logs)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .flatten()
            .any(|col| col == "date");

        if !has_date {
            conn.execute("ALTER TABLE request_logs ADD COLUMN date TEXT", [])?;
        }

        // `session_id`/`client` arrived with session-aware logging. SQLite has
        // no `ADD COLUMN IF NOT EXISTS`, so each is probed first and old rows
        // keep the empty-string default.
        for column in [
            "session_id",
            "client",
            "override_key",
            "override_model",
            "override_reason",
        ] {
            let exists = conn
                .prepare("PRAGMA table_info(request_logs)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .flatten()
                .any(|col| col == column);
            if !exists {
                conn.execute(
                    &format!(
                        "ALTER TABLE request_logs ADD COLUMN {column} TEXT NOT NULL DEFAULT ''"
                    ),
                    [],
                )?;
            }
        }

        // Backfill rows written before the column existed.
        conn.execute(
            "UPDATE request_logs
                SET date = substr(created_at, 1, 10)
              WHERE date IS NULL OR date = ''",
            [],
        )?;

        // Upstream timings arrived with the overview's output-speed card. The
        // loop above only handles TEXT columns, so these get their own numeric
        // pass; old rows keep 0, which the UI renders as "unknown" rather than
        // inventing a speed for requests we never measured.
        for column in ["ttft_ms", "model_ms", "ended_with_tool_call"] {
            let exists = conn
                .prepare("PRAGMA table_info(request_logs)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .flatten()
                .any(|col| col == column);
            if !exists {
                conn.execute(
                    &format!(
                        "ALTER TABLE request_logs ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"
                    ),
                    [],
                )?;
            }
        }

        // daily_stats is now derived from request_logs on demand.
        conn.execute_batch("DROP TABLE IF EXISTS daily_stats;")?;

        Ok(())
    }

    /// Record the outcome of one finished request.
    ///
    /// Non-blocking in persistent mode: the row is queued for the writer thread.
    /// Call this exactly once per request, after the response has completed —
    /// never per streamed chunk.
    pub fn record_request_log(&self, outcome: RequestOutcome<'_>) -> Result<()> {
        let row = outcome.to_row(&local_date_string(), &local_datetime_string());
        match &self.queue {
            Some(tx) => tx
                .send(row)
                .map_err(|_| anyhow::anyhow!("stats writer thread stopped")),
            None => {
                let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
                let mut stmt = conn.prepare(RequestRow::insert_sql())?;
                row.bind_to(&mut stmt)?;
                Ok(())
            }
        }
    }

    /// Query request logs with optional filtering and pagination.
    pub fn query_request_logs(&self, filter: &RequestLogFilter) -> Result<RequestLogsResult> {
        // Pooled: this runs five queries in a row, so holding the one shared
        // lock would block the GUI's other poller for that whole span.
        let handle = self.read_conn();
        let conn = handle.get();

        // 1. Fetch distinct models for filter dropdown
        let mut model_stmt =
            conn.prepare("SELECT DISTINCT model FROM request_logs ORDER BY model ASC")?;
        let models_iter = model_stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut models = Vec::new();
        for m in models_iter.flatten() {
            if !m.is_empty() {
                models.push(m);
            }
        }

        // 1b. Data-driven client list, so the dropdown only offers dialects
        //     that actually appear in this database.
        let mut client_stmt = conn.prepare(
            "SELECT DISTINCT client FROM request_logs WHERE client != '' ORDER BY client ASC",
        )?;
        let clients_iter = client_stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut clients = Vec::new();
        for c in clients_iter.flatten() {
            if !c.is_empty() {
                clients.push(c);
            }
        }

        // 1c. Conversations for the session filter, newest first. Capped so a
        //     long-lived database cannot make the dropdown itself the slow part;
        //     the cap is generous enough to cover any realistic recent history.
        let mut session_stmt = conn.prepare(
            "SELECT session_id, MAX(client) FROM request_logs
              WHERE session_id != ''
              GROUP BY session_id
              ORDER BY MAX(id) DESC
              LIMIT 200",
        )?;
        let sessions_iter = session_stmt.query_map([], |row| {
            Ok(RequestLogSession {
                session_id: row.get::<_, String>(0)?,
                client: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            })
        })?;
        let mut sessions: Vec<RequestLogSession> = sessions_iter.flatten().collect();
        sessions.retain(|s| !s.session_id.is_empty());

        // 2. Build WHERE clause
        let mut where_clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(ref m) = filter.model {
            let m_trimmed = m.trim();
            if !m_trimmed.is_empty() && m_trimmed != "all" {
                where_clauses.push("model = ?".to_string());
                params.push(Box::new(m_trimmed.to_string()));
            }
        }

        if let Some(ref s) = filter.status_group {
            match s.as_str() {
                "2xx" | "success" => {
                    where_clauses
                        .push("status >= 200 AND status < 300 AND error IS NULL".to_string());
                }
                "4xx" | "client_error" => {
                    where_clauses.push("status >= 400 AND status < 500".to_string());
                }
                "5xx" | "server_error" => {
                    where_clauses.push("status >= 500".to_string());
                }
                "error" => {
                    where_clauses.push("(status >= 400 OR error IS NOT NULL)".to_string());
                }
                _ => {}
            }
        }

        for (value, column) in [
            (&filter.client, "client"),
            (&filter.session_id, "session_id"),
        ] {
            if let Some(ref v) = value {
                let v = v.trim();
                if !v.is_empty() && v != "all" {
                    where_clauses.push(format!("{column} = ?"));
                    params.push(Box::new(v.to_string()));
                }
            }
        }

        if let Some(streamed) = filter.streamed {
            where_clauses.push("streamed = ?".to_string());
            params.push(Box::new(if streamed { 1 } else { 0 }));
        }

        if let Some(ref q) = filter.search {
            let q_trimmed = q.trim();
            if !q_trimmed.is_empty() {
                where_clauses.push(
                    "(model LIKE ? OR route LIKE ? OR error LIKE ? \
                     OR session_id LIKE ? OR client LIKE ?)"
                        .to_string(),
                );
                let like_pat = format!("%{}%", q_trimmed);
                for _ in 0..5 {
                    params.push(Box::new(like_pat.clone()));
                }
            }
        }

        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        // 3. Count total matching rows
        let count_sql = format!("SELECT COUNT(*) FROM request_logs {}", where_sql);
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: i64 = conn.query_row(
            &count_sql,
            rusqlite::params_from_iter(params_refs.iter().copied()),
            |row| row.get(0),
        )?;

        // 4. Query page items
        let limit = filter.limit.unwrap_or(50).clamp(1, 500);
        let offset = filter.offset.unwrap_or(0);
        let query_sql = format!(
            "SELECT id, created_at, model, route,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    duration_ms, ttft_ms, model_ms, ended_with_tool_call,
                    streamed, status, error, session_id, client,
                    override_key, override_model, override_reason
             FROM request_logs
             {}
             ORDER BY id DESC
             LIMIT ? OFFSET ?",
            where_sql
        );

        let mut query_params: Vec<&dyn rusqlite::ToSql> = params_refs;
        let limit_i64 = limit as i64;
        let offset_i64 = offset as i64;
        query_params.push(&limit_i64);
        query_params.push(&offset_i64);

        let mut stmt = conn.prepare(&query_sql)?;
        let items_iter = stmt.query_map(rusqlite::params_from_iter(query_params), |row| {
            let streamed_int: i64 = row.get(12)?;
            let status_int: i64 = row.get(13)?;
            let tool_int: i64 = row.get(11)?;
            Ok(RequestLogItem {
                id: row.get(0)?,
                created_at: row.get(1)?,
                model: row.get(2)?,
                route: row.get(3)?,
                input_tokens: row.get(4)?,
                output_tokens: row.get(5)?,
                cache_read_tokens: row.get(6)?,
                cache_write_tokens: row.get(7)?,
                duration_ms: row.get(8)?,
                ttft_ms: row.get(9)?,
                model_ms: row.get(10)?,
                ended_with_tool_call: tool_int != 0,
                streamed: streamed_int != 0,
                status: status_int as u16,
                error: row.get(14)?,
                session_id: row.get(15)?,
                client: row.get(16)?,
                override_key: row.get(17)?,
                override_model: row.get(18)?,
                override_reason: row.get(19)?,
            })
        })?;

        let mut items = Vec::new();
        for item in items_iter {
            items.push(item?);
        }

        Ok(RequestLogsResult {
            items,
            total,
            models,
            clients,
            sessions,
        })
    }

    /// Clear all request logs from the database.
    pub fn clear_request_logs(&self) -> Result<()> {
        // A write, but it needs a connection that sees the same database; for
        // in-memory that is the shared handle, so this borrows through the same
        // path the readers use.
        let handle = self.read_conn();
        handle.get().execute("DELETE FROM request_logs", [])?;
        Ok(())
    }

    /// The conversation the overview's speed card should describe: the one whose
    /// newest request is the most recent **today**.
    ///
    /// A session with no id (unidentified client) is not a conversation, so it
    /// never wins here — the card would otherwise describe an anonymous request
    /// that cannot be followed up.
    pub fn latest_session_id(&self) -> Result<Option<String>> {
        self.latest_session_id_on(&local_date_string())
    }

    /// [`Self::latest_session_id`] restricted to one local day.
    ///
    /// The card describes today's traffic, so a conversation whose last request
    /// was yesterday must not win the slot — it would show a stale speed while
    /// today's sessions were still running.
    pub fn latest_session_id_on(&self, date: &str) -> Result<Option<String>> {
        let handle = self.read_conn();
        let id = handle
            .get()
            .query_row(
                "SELECT session_id FROM request_logs
                  WHERE session_id != '' AND date = ?1
                  ORDER BY id DESC
                  LIMIT 1",
                params![date],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Metrics for the most recently active conversation, **as of today**.
    ///
    /// Convenience wrapper over [`Self::latest_session_id`] +
    /// [`Self::query_session_metrics`] so the GUI makes one round-trip.
    pub fn query_latest_session_metrics(&self) -> Result<SessionMetrics> {
        self.query_latest_session_metrics_on(&local_date_string())
    }

    /// [`Self::query_latest_session_metrics`] against an explicit date.
    ///
    /// Takes the date rather than reading the clock so the "day" boundary is
    /// testable without freezing time.
    pub fn query_latest_session_metrics_on(&self, date: &str) -> Result<SessionMetrics> {
        match self.latest_session_id_on(date)? {
            Some(id) => self.query_session_metrics_on(&id, date),
            None => Ok(SessionMetrics::default()),
        }
    }

    /// Per-session aggregates for the `limit` most recently active conversations
    /// **that were active today**, newest first — the overview's speed panel
    /// compares them side by side.
    ///
    /// Each row is folded exactly like [`Self::query_session_metrics`] (same
    /// window cap, same weighting, same day), so a session's figure does not
    /// change depending on which query produced it.
    ///
    /// Only identified sessions appear: an empty `session_id` is not a
    /// conversation and cannot be compared with one.
    pub fn query_recent_session_metrics(&self, limit: usize) -> Result<Vec<SessionMetrics>> {
        self.query_recent_session_metrics_on(limit, &local_date_string())
    }

    /// [`Self::query_recent_session_metrics`] against an explicit date.
    pub fn query_recent_session_metrics_on(
        &self,
        limit: usize,
        date: &str,
    ) -> Result<Vec<SessionMetrics>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // One handle for the whole method. Calling `query_session_metrics` per id
        // would acquire a *second* handle inside the first — an in-memory
        // database shares a single `Mutex<Connection>`, so that self-deadlocks
        // (and it is the GUI's fallback database when the real file cannot be
        // opened, so this is not a test-only hazard).
        let handle = self.read_conn();
        let conn = handle.get();

        // Restricted to today, for the same reason the current session's figure
        // is: the panel exists to compare *today's* conversations, and a session
        // that ran only yesterday would otherwise sit in the comparison with a
        // full-window figure beside today's partial ones.
        let mut stmt = conn.prepare(
            "SELECT session_id FROM request_logs
              WHERE session_id != '' AND date = ?1
              GROUP BY session_id
              ORDER BY MAX(id) DESC
              LIMIT ?2",
        )?;
        let ids: Vec<String> = stmt
            .query_map(params![date, limit as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(Self::fold_session_on(conn, &id, Some(date))?);
        }
        Ok(out)
    }

    /// Session-level speed metrics for the overview's 输出速度 card, as of today.
    pub fn query_session_metrics(&self, session_id: &str) -> Result<SessionMetrics> {
        self.query_session_metrics_on(session_id, &local_date_string())
    }

    /// [`Self::query_session_metrics`] against an explicit date.
    ///
    /// Folds the session's most recent [`SESSION_METRICS_MAX_TURNS`] turns
    /// **from that day** into one time-weighted aggregate (see
    /// [`SessionMetrics`]) so the figure is stable instead of changing with
    /// every turn.
    ///
    /// The window is taken **newest-first** by `id` and then reversed, so the cap
    /// always keeps the *recent* turns — the conversation's live behaviour —
    /// rather than the oldest ones.
    ///
    /// Tool time is summed across the window: the proxy never executes tools (the
    /// client does, then reports back in a later request), so the gap between a
    /// tool-requesting turn and its successor is the only observable tool time.
    pub fn query_session_metrics_on(&self, session_id: &str, date: &str) -> Result<SessionMetrics> {
        if session_id.is_empty() {
            return Ok(SessionMetrics::default());
        }
        let handle = self.read_conn();
        Self::fold_session_on(handle.get(), session_id, Some(date))
    }

    /// Read one session's window from `conn` and fold it.
    ///
    /// Takes the connection rather than acquiring its own so callers can reuse a
    /// single handle for many sessions (see `query_recent_session_metrics`).
    ///
    /// `date` restricts the window to one local day; `None` folds the session's
    /// whole history. The panel always passes a date — "today" — because an
    /// unbounded figure would mix yesterday's throughput into today's card.
    fn fold_session_on(
        conn: &Connection,
        session_id: &str,
        date: Option<&str>,
    ) -> Result<SessionMetrics> {
        const COLUMNS: &str = "id, created_at, model, input_tokens, output_tokens,
                               cache_read_tokens, duration_ms, ttft_ms, model_ms,
                               ended_with_tool_call";
        let sql = match date {
            Some(_) => format!(
                "SELECT {COLUMNS}
                   FROM request_logs
                  WHERE session_id = ?1 AND date = ?2
                  ORDER BY id DESC
                  LIMIT ?3"
            ),
            None => format!(
                "SELECT {COLUMNS}
                   FROM request_logs
                  WHERE session_id = ?1
                  ORDER BY id DESC
                  LIMIT ?2"
            ),
        };

        let mut stmt = conn.prepare(&sql)?;
        let read_row = |row: &rusqlite::Row<'_>| {
            let tool_int: i64 = row.get(9)?;
            Ok(SessionTurn {
                id: row.get(0)?,
                created_at: row.get(1)?,
                model: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                duration_ms: row.get(6)?,
                ttft_ms: row.get(7)?,
                model_ms: row.get(8)?,
                ended_with_tool_call: tool_int != 0,
            })
        };

        let rows = match date {
            Some(d) => stmt
                .query_map(params![session_id, d, SESSION_METRICS_MAX_TURNS], read_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map(params![session_id, SESSION_METRICS_MAX_TURNS], read_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };

        // Newest-first from SQL (so LIMIT keeps the recent end); the fold reads
        // chronologically, which is what makes "previous turn" meaningful.
        let mut turns: Vec<SessionTurn> = rows;
        turns.reverse();

        Ok(fold_session_turns(session_id, &turns))
    }

    /// Return statistics for today (local date). Returns a zeroed `DayStats`
    /// with today's date if no requests have been recorded yet.
    pub fn query_today(&self) -> Result<DayStats> {
        let date = local_date_string();
        self.query_date(&date)
    }

    /// Return statistics for an arbitrary date (`YYYY-MM-DD`), aggregated from
    /// the request logs for that day.
    pub fn query_date(&self, date: &str) -> Result<DayStats> {
        let handle = self.read_conn();
        let result = handle.get().query_row(
            "SELECT
                COUNT(*),
                COALESCE(SUM(CASE WHEN status >= 200 AND status < 300 AND error IS NULL
                                  THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN status >= 200 AND status < 300 AND error IS NULL
                                  THEN 0 ELSE 1 END), 0),
                COALESCE(SUM(input_tokens), 0),
                COALESCE(SUM(cache_read_tokens), 0),
                COALESCE(SUM(cache_write_tokens), 0),
                COALESCE(SUM(output_tokens), 0),
                COALESCE(SUM(CASE WHEN override_key != '' THEN 1 ELSE 0 END), 0)
             FROM request_logs WHERE date = ?1",
            params![date],
            |row| {
                Ok(DayStats {
                    date: date.to_string(),
                    requests_total: row.get(0)?,
                    requests_success: row.get(1)?,
                    requests_failed: row.get(2)?,
                    tokens_input: row.get(3)?,
                    tokens_cache_read: row.get(4)?,
                    tokens_cache_write: row.get(5)?,
                    tokens_output: row.get(6)?,
                    overrides_total: row.get(7)?,
                })
            },
        );

        match result {
            Ok(stats) => Ok(stats),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(DayStats {
                date: date.to_string(),
                ..Default::default()
            }),
            Err(e) => Err(e.into()),
        }
    }
}

/// Drains the request queue and commits rows in batches.
///
/// Runs on its own OS thread for the lifetime of the `StatsDb`. The first
/// `recv` blocks until work arrives; everything already queued is then drained
/// into a single transaction, so a burst of requests costs one commit.
fn writer_loop(conn: Connection, rx: Receiver<RequestRow>) {
    let mut stmt = match conn.prepare(RequestRow::insert_sql()) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("stats writer failed to prepare insert: {}", e);
            return;
        }
    };

    // recv() returns Err only when every sender has been dropped, i.e. the
    // StatsDb is gone — that is the signal to exit.
    //
    // Each batch is unwrapped individually. This is the only consumer of the
    // queue: if it panicked, every later `record_request_log` would fail
    // forever while the proxy kept serving requests perfectly — the 请求日志 and
    // 用量统计 panels would silently freeze with nothing anywhere to explain
    // why. Containing the panic keeps one bad row from ending the thread's
    // life, and logs it instead.
    while let Ok(first) = rx.recv() {
        let mut batch = vec![first];
        while let Ok(next) = rx.try_recv() {
            batch.push(next);
        }

        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            insert_batch(&conn, &mut stmt, &batch)
        }));
        match outcome {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                tracing::warn!("stats writer dropped {} row(s): {}", batch.len(), e);
            }
            Err(_) => {
                // The statement may be left in a broken state; rebuild it so
                // the next batch starts clean instead of failing the same way.
                tracing::error!(
                    "stats writer panicked on {} row(s); recovering",
                    batch.len()
                );
                match conn.prepare(RequestRow::insert_sql()) {
                    Ok(fresh) => stmt = fresh,
                    Err(e) => {
                        tracing::error!("stats writer could not rebuild its statement: {}", e);
                        return;
                    }
                }
            }
        }
    }
}

fn insert_batch(
    conn: &Connection,
    stmt: &mut rusqlite::Statement<'_>,
    batch: &[RequestRow],
) -> rusqlite::Result<()> {
    let tx = conn.unchecked_transaction()?;
    for row in batch {
        row.bind_to(stmt)?;
    }
    tx.commit()
}

// ── Helpers ───────────────────────────────────────────────────────────────

/// Return today's date as a `YYYY-MM-DD` string in the machine's local time.
///
/// Uses the same offset as log timestamps, so a day's stats cover exactly the
/// range the log file shows for that day.
fn local_date_string() -> String {
    let offset_secs = crate::util::local_utc_offset_secs();
    let local_secs = (now_secs() + offset_secs).max(0) as u64;
    let (y, m, d) = crate::util::civil_from_days((local_secs / 86400) as i64);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Return current local datetime as `YYYY-MM-DD HH:MM:SS` string, in the same
/// zone the log file uses.
fn local_datetime_string() -> String {
    let offset_secs = crate::util::local_utc_offset_secs();
    let local_secs = (now_secs() + offset_secs).max(0) as u64;
    // `format_epoch_secs` renders an epoch-seconds value; feeding it the
    // already-shifted local seconds yields local wall-clock fields.
    crate::util::format_epoch_secs(local_secs)
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome<'a>(
        model: &'a str,
        route: &'a str,
        tokens: &'a TokenRecord,
        status: u16,
        error: Option<&'a str>,
        streamed: bool,
    ) -> RequestOutcome<'a> {
        RequestOutcome {
            model,
            route,
            tokens,
            duration_ms: 1250,
            timing: UpstreamTiming::default(),
            streamed,
            status,
            error,
            session_id: "",
            client: "",
            override_key: "",
            override_model: "",
            override_reason: "",
        }
    }

    /// The tool wait is measured on the client, not swallowed by this turn's own
    /// duration.
    ///
    /// Both stamps are row-*write* times (end of each turn), so the raw gap also
    /// contains the current turn's duration and must be backed out. Getting this
    /// wrong inflates every tool wait by the length of the following request.
    #[test]
    fn tool_wait_excludes_the_current_turns_own_duration() {
        // Prev turn ended at 10:00:00. This turn ended at 10:00:20 having taken
        // 5 s, so it began at 10:00:15 → the client spent 15 s running the tool.
        let wait = tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:20", 5_000);
        assert_eq!(wait, Some(15_000), "20s gap minus this turn's own 5s");

        // Same stamps, a turn that took the whole 20 s: the client did not wait.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:20", 20_000),
            Some(0)
        );

        // A gap shorter than the turn itself is a clock/estimation artifact, not
        // a negative tool wait: report nothing rather than a nonsense value.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:01", 30_000),
            None
        );

        // Resumed much later: not tool execution.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 12:00:00", 1_000),
            None
        );

        // Unparseable stamps yield nothing instead of a fabricated number.
        assert_eq!(tool_wait_ms("nonsense", "2026-09-28 10:00:20", 0), None);
        assert_eq!(tool_wait_ms("2026-09-28 10:00:00", "", 0), None);

        // Timestamps are whole seconds, so a sub-second turn inside the same
        // second backs out to a real (small) wait rather than a negative one.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:02", 1_500),
            Some(500)
        );
        // ...and a turn that fills its own second lands on exactly zero.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:02", 2_000),
            Some(0)
        );
    }

    /// The timestamps in the DB are local wall-clock, and a plain subtraction of
    /// two of them is the elapsed time between them regardless of the UTC offset.
    #[test]
    fn local_datetime_parsing_is_monotonic_across_a_day_boundary() {
        let a = parse_local_datetime_secs("2026-09-28 23:59:59").unwrap();
        let b = parse_local_datetime_secs("2026-09-29 00:00:01").unwrap();
        assert_eq!(b - a, 2, "crossing midnight adds two seconds");

        // A leap day parses, so the day-count arithmetic is not off by one.
        let feb28 = parse_local_datetime_secs("2028-02-28 00:00:00").unwrap();
        let mar1 = parse_local_datetime_secs("2028-03-01 00:00:00").unwrap();
        assert_eq!(mar1 - feb28, 2 * 86_400, "2028 is a leap year");
    }

    #[test]
    fn upstream_timings_round_trip_and_yield_tps() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord {
            output: 600,
            ..Default::default()
        };
        let mut o = outcome("hy3", "/v1/messages", &tokens, 200, None, true);
        o.duration_ms = 9_000;
        o.timing = UpstreamTiming {
            ttft_ms: 320,
            model_ms: 3_000,
            ended_with_tool_call: true,
        };
        let _ = db.record_request_log(o);

        let res = db
            .query_request_logs(&RequestLogFilter {
                limit: Some(10),
                ..Default::default()
            })
            .unwrap();
        let item = &res.items[0];
        assert_eq!(item.ttft_ms, 320);
        assert_eq!(item.model_ms, 3_000);
        assert!(item.ended_with_tool_call);

        // 600 tokens over the 3 s model span, NOT over the 9 s request.
        let tps = UpstreamTiming {
            ttft_ms: item.ttft_ms,
            model_ms: item.model_ms,
            ended_with_tool_call: item.ended_with_tool_call,
        }
        .tps(item.output_tokens)
        .expect("measurable");
        assert!((tps - 200.0).abs() < 0.001, "got {tps}");

        // An unmeasured span yields no speed rather than a fabricated zero.
        assert_eq!(
            UpstreamTiming {
                ttft_ms: 0,
                model_ms: 0,
                ended_with_tool_call: false,
            }
            .tps(600),
            None
        );
        assert_eq!(
            UpstreamTiming {
                ttft_ms: 0,
                model_ms: 1_000,
                ended_with_tool_call: false,
            }
            .tps(0),
            None,
            "no output tokens means no speed"
        );
    }

    /// An older database gains the numeric timing columns instead of failing to
    /// open — and old rows read back as 0 ("unknown"), never as a real speed.
    #[test]
    fn migration_adds_the_timing_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL,
                model TEXT NOT NULL,
                route TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 60,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                streamed INTEGER NOT NULL DEFAULT 0,
                status INTEGER NOT NULL DEFAULT 0,
                error TEXT
             );
             INSERT INTO request_logs (created_at, model, route, output_tokens, status)
             VALUES ('2026-09-22 10:00:00', 'hy3', '/v1/messages', 60, 200);",
        )
        .unwrap();

        StatsDb::init_schema(&conn).unwrap();

        let db = StatsDb::from_conn(conn);
        let item = db
            .query_request_logs(&RequestLogFilter::default())
            .unwrap()
            .items
            .into_iter()
            .next()
            .expect("the legacy row survives the migration");
        assert_eq!(item.ttft_ms, 0, "legacy row has no measurable TTFT");
        assert_eq!(item.model_ms, 0, "legacy row has no measurable span");
        assert!(!item.ended_with_tool_call);
    }

    /// The session aggregate sums tokens and time, and attributes tool time
    /// across the window.
    ///
    /// This is the property that makes the card stable: two turns at very
    /// different speeds must combine into one time-weighted rate, not the
    /// latest turn's (jittery) figure.
    #[test]
    fn session_metrics_aggregate_tokens_over_time() {
        let db = StatsDb::in_memory().unwrap();

        // Turn 1: 500 tokens over 1 s (fast), ends 10:00:00, asks for a tool.
        insert_turn(
            &db,
            "claude:s1",
            "2026-09-28 10:00:00",
            3_000,
            200,
            1_000,
            500,
            true,
        );
        // Turn 2: 100 tokens over 3 s (slow), ends 10:00:20 having taken 5 s.
        insert_turn(
            &db,
            "claude:s1",
            "2026-09-28 10:00:20",
            5_000,
            400,
            3_000,
            100,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s1", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, 2);
        assert_eq!(m.measured_turns, 2);
        assert_eq!(m.output_tokens, 600, "tokens sum");
        assert_eq!(m.model_ms, 4_000, "generation time sums");

        // Time-weighted: 600 / 4 s = 150 tok/s. Averaging the two per-turn rates
        // would give (500 + 33.3)/2 ≈ 267 — wrong, because it would let a
        // 100-token reply count as much as a 500-token one.
        let tps = m.tps().expect("measurable");
        assert!((tps - 150.0).abs() < 0.001, "600 tokens / 4s, got {tps}");

        // Mean TTFT across both measured turns: (200 + 400) / 2.
        assert_eq!(m.avg_ttft_ms(), Some(300));

        // Turn 1 asked for a tool; the gap to turn 2's start is 20 s minus
        // turn 2's own 5 s = 15 s.
        assert_eq!(m.tool_wait_ms(), Some(15_000));
        assert_eq!(m.tool_waits, 1);

        // The panel's context fields describe the newest turn.
        assert_eq!(m.model.as_deref(), Some("hy3"));
        assert_eq!(m.last_at.as_deref(), Some("2026-09-28 10:00:20"));

        // An unknown session yields an empty result rather than an error.
        let empty = db.query_session_metrics_on("nobody", "2026-09-28").unwrap();
        assert_eq!(empty.turns, 0);
        assert_eq!(empty.tps(), None);
        assert_eq!(empty.avg_ttft_ms(), None);
        assert_eq!(empty.tool_wait_ms(), None);
        assert_eq!(
            db.query_session_metrics_on("", "2026-09-28").unwrap().turns,
            0
        );
    }

    /// Unmeasured turns do not drag the aggregate down.
    ///
    /// A legacy row or a plain non-streamed reply has `model_ms == 0`. Counting
    /// it would add zero time *and* whatever tokens it reports, wildly inflating
    /// the rate; it must be excluded from both sums while still being counted as
    /// a turn.
    #[test]
    fn session_metrics_exclude_unmeasured_turns_from_the_rate() {
        let db = StatsDb::in_memory().unwrap();

        // Measured: 400 tokens over 2 s.
        insert_turn(
            &db,
            "claude:s4",
            "2026-09-28 11:00:00",
            2_500,
            100,
            2_000,
            400,
            false,
        );
        // Unmeasured: no span at all, but a large token count that must not count.
        insert_turn(
            &db,
            "claude:s4",
            "2026-09-28 11:00:10",
            500,
            0,
            0,
            9_999,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s4", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, 2, "both rows are turns");
        assert_eq!(m.measured_turns, 1, "only one carried a span");
        assert_eq!(
            m.output_tokens, 400,
            "the unmeasured row's tokens are excluded"
        );
        assert_eq!(m.model_ms, 2_000);
        let tps = m.tps().expect("measurable");
        assert!((tps - 200.0).abs() < 0.001, "400 / 2s, got {tps}");
        // Its zero TTFT is not a sample either.
        assert_eq!(m.avg_ttft_ms(), Some(100));
    }

    /// A window with nothing measurable reports no speed at all, so the UI shows
    /// "—" instead of a fabricated number.
    #[test]
    fn session_metrics_report_nothing_when_unmeasured() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:s5",
            "2026-09-28 12:00:00",
            1_000,
            0,
            0,
            50,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s5", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, 1);
        assert_eq!(m.measured_turns, 0);
        assert_eq!(m.tps(), None);
        assert_eq!(m.avg_ttft_ms(), None);
        assert_eq!(m.tool_wait_ms(), None);
        // ...but the panel can still name the conversation.
        assert_eq!(m.model.as_deref(), Some("hy3"));
    }

    /// The window keeps the *recent* turns when a session exceeds the cap.
    ///
    /// `LIMIT` runs newest-first, so getting the order wrong would aggregate the
    /// oldest turns of a long conversation instead of its current behaviour.
    #[test]
    fn session_metrics_window_keeps_the_recent_turns() {
        let db = StatsDb::in_memory().unwrap();

        // One ancient turn that must fall out of the window...
        insert_turn(
            &db,
            "claude:s6",
            "2026-09-28 09:00:00",
            1_000,
            50,
            1_000,
            1_000_000,
            false,
        );
        // ...then enough recent turns to exceed the cap.
        let extra = 5;
        for i in 0..(SESSION_METRICS_MAX_TURNS + extra) {
            let secs = i % 60;
            let mins = (i / 60) % 60;
            let created = format!("2026-09-28 13:{mins:02}:{secs:02}");
            insert_turn(&db, "claude:s6", &created, 2_000, 100, 1_000, 10, false);
        }

        let m = db
            .query_session_metrics_on("claude:s6", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, SESSION_METRICS_MAX_TURNS, "window is capped");
        // The huge ancient row would dominate the sum if it were included.
        assert_eq!(
            m.output_tokens,
            10 * SESSION_METRICS_MAX_TURNS,
            "only the recent turns are summed"
        );
        assert_eq!(m.model_ms, 1_000 * SESSION_METRICS_MAX_TURNS);
        let tps = m.tps().expect("measurable");
        assert!((tps - 10.0).abs() < 0.001, "10 tokens per 1s, got {tps}");
    }

    /// The recent-sessions list returns the most recently *active* conversations,
    /// newest first, and excludes unidentified requests.
    ///
    /// Ordering is by each session's latest activity, not by any single row's id:
    /// a long-running conversation must outrank one that merely started later.
    #[test]
    fn recent_sessions_are_ordered_by_latest_activity() {
        let db = StatsDb::in_memory().unwrap();

        // "old" starts first and keeps receiving turns, so its last row is the
        // newest overall — it must come first.
        insert_turn(
            &db,
            "claude:old",
            "2026-09-28 10:00:00",
            1_000,
            100,
            1_000,
            10,
            false,
        );
        insert_turn(
            &db,
            "claude:mid",
            "2026-09-28 10:00:05",
            1_000,
            100,
            1_000,
            10,
            false,
        );
        insert_turn(
            &db,
            "claude:old",
            "2026-09-28 10:00:10",
            1_000,
            100,
            1_000,
            10,
            false,
        );

        // An unidentified request must never appear: it is not a conversation.
        let no_tokens = TokenRecord::default();
        let mut anon = outcome("hy3", "/v1/messages", &no_tokens, 200, None, true);
        anon.session_id = "";
        let _ = db.record_request_log(anon);

        let recent = db.query_recent_session_metrics_on(3, "2026-09-28").unwrap();
        let ids: Vec<&str> = recent.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["claude:old", "claude:mid"],
            "newest activity first"
        );

        // `turns` counts only that session's rows.
        assert_eq!(recent[0].turns, 2, "old has two turns");
        assert_eq!(recent[1].turns, 1);

        // The limit is honoured, and 0 asks for nothing.
        let only_one = db.query_recent_session_metrics_on(1, "2026-09-28").unwrap();
        assert_eq!(only_one.len(), 1);
        assert_eq!(only_one[0].session_id, "claude:old");
        assert!(db
            .query_recent_session_metrics_on(0, "2026-09-28")
            .unwrap()
            .is_empty());

        // A database with no identified sessions yields an empty list, not an
        // error or a nameless row.
        let empty = StatsDb::in_memory().unwrap();
        assert!(empty
            .query_recent_session_metrics_on(3, "2026-09-28")
            .unwrap()
            .is_empty());
    }

    /// A session in the `recent` list reports exactly the same aggregate as the
    /// dedicated query — the panel compares them side by side, so a divergence
    /// would make the highlighted row disagree with the list.
    #[test]
    fn recent_sessions_agree_with_the_single_session_query() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:a",
            "2026-09-28 10:00:00",
            2_000,
            100,
            1_000,
            400,
            true,
        );
        insert_turn(
            &db,
            "claude:a",
            "2026-09-28 10:00:20",
            3_000,
            300,
            2_000,
            200,
            false,
        );

        let single = db
            .query_session_metrics_on("claude:a", "2026-09-28")
            .unwrap();
        let listed = db.query_recent_session_metrics_on(5, "2026-09-28").unwrap();
        let found = listed
            .iter()
            .find(|m| m.session_id == "claude:a")
            .expect("session is listed");

        // Aggregate fields must match; the fold reads the same window either way.
        assert_eq!(found.turns, single.turns);
        assert_eq!(found.measured_turns, single.measured_turns);
        assert_eq!(found.output_tokens, single.output_tokens);
        assert_eq!(found.model_ms, single.model_ms);
        assert_eq!(found.ttft_sum_ms, single.ttft_sum_ms);
        assert_eq!(found.ttft_samples, single.ttft_samples);
        assert_eq!(found.tool_wait_ms, single.tool_wait_ms);
        assert_eq!(found.tool_waits, single.tool_waits);
        assert_eq!(found.tps(), single.tps());
        assert_eq!(found.avg_ttft_ms(), single.avg_ttft_ms());
        assert_eq!(found.model, single.model);
        assert_eq!(found.last_at, single.last_at);
    }

    /// A turn not preceded by a tool request contributes no tool time —
    /// otherwise every ordinary turn would claim the previous turn's spacing.
    #[test]
    fn session_metrics_ignore_a_non_tool_predecessor() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:s2",
            "2026-09-28 10:00:00",
            1_000,
            100,
            500,
            100,
            false,
        );
        insert_turn(
            &db,
            "claude:s2",
            "2026-09-28 10:00:30",
            1_000,
            100,
            500,
            100,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s2", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, 2);
        assert_eq!(
            m.tool_wait_ms(),
            None,
            "a text-only predecessor is not a tool wait"
        );
        assert_eq!(m.tool_waits, 0);
    }

    /// Tool time accumulates across several tool round-trips in one window.
    #[test]
    fn session_metrics_sum_tool_time_across_turns() {
        let db = StatsDb::in_memory().unwrap();

        // Each pair: a tool-requesting turn, then its successor 10 s later that
        // itself took 2 s → a 8 s tool wait each time.
        insert_turn(
            &db,
            "claude:s7",
            "2026-09-28 10:00:00",
            2_000,
            100,
            500,
            10,
            true,
        );
        insert_turn(
            &db,
            "claude:s7",
            "2026-09-28 10:00:10",
            2_000,
            100,
            500,
            10,
            false,
        );
        insert_turn(
            &db,
            "claude:s7",
            "2026-09-28 10:00:20",
            2_000,
            100,
            500,
            10,
            true,
        );
        insert_turn(
            &db,
            "claude:s7",
            "2026-09-28 10:00:30",
            2_000,
            100,
            500,
            10,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s7", "2026-09-28")
            .unwrap();
        assert_eq!(m.tool_waits, 2, "two tool round-trips");
        assert_eq!(m.tool_wait_ms(), Some(16_000), "two 8s waits summed");
    }

    /// Insert one row with an explicit `created_at`, so timing arithmetic can be
    /// tested without depending on the wall clock.
    #[allow(clippy::too_many_arguments)]
    fn insert_turn(
        db: &StatsDb,
        session: &str,
        created_at: &str,
        duration_ms: i64,
        ttft_ms: i64,
        model_ms: i64,
        output_tokens: i64,
        tool: bool,
    ) {
        let conn = db.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO request_logs (
                date, created_at, model, route, output_tokens, duration_ms,
                ttft_ms, model_ms, ended_with_tool_call, streamed, status,
                session_id, client
             ) VALUES (?1, ?2, 'hy3', '/v1/messages', ?3, ?4, ?5, ?6, ?7, 1, 200, ?8, 'claude')",
            params![
                &created_at[..10],
                created_at,
                output_tokens,
                duration_ms,
                ttft_ms,
                model_ms,
                if tool { 1 } else { 0 },
                session
            ],
        )
        .expect("insert test turn");
    }

    /// [`insert_turn`] with prompt-side token counts, for the cache-hit tests.
    ///
    /// Kept separate so the dozen existing call sites do not have to pass two
    /// more zeroes to express "this fixture never cared about prompt tokens".
    #[allow(clippy::too_many_arguments)]
    fn insert_turn_with_prompt(
        db: &StatsDb,
        session: &str,
        created_at: &str,
        input_tokens: i64,
        cache_read_tokens: i64,
        duration_ms: i64,
        ttft_ms: i64,
        model_ms: i64,
        output_tokens: i64,
    ) {
        let conn = db.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO request_logs (
                date, created_at, model, route, input_tokens, cache_read_tokens,
                output_tokens, duration_ms, ttft_ms, model_ms,
                ended_with_tool_call, streamed, status, session_id, client
             ) VALUES (?1, ?2, 'hy3', '/v1/messages', ?3, ?4, ?5, ?6, ?7, ?8, 0, 1, 200, ?9, 'claude')",
            params![
                &created_at[..10],
                created_at,
                input_tokens,
                cache_read_tokens,
                output_tokens,
                duration_ms,
                ttft_ms,
                model_ms,
                session
            ],
        )
        .expect("insert test turn with prompt");
    }

    /// A session's figures are restricted to the given day.
    ///
    /// This is what makes the panel describe "today": turns from an earlier date
    /// must not reach the aggregate, even for the same session id.
    #[test]
    fn session_metrics_are_restricted_to_the_requested_day() {
        let db = StatsDb::in_memory().unwrap();

        // Yesterday: a huge, fast turn that would dominate any figure.
        insert_turn(
            &db,
            "claude:day",
            "2026-09-28 10:00:00",
            1_000,
            10,
            1_000,
            100_000,
            false,
        );
        // Today: two ordinary turns.
        insert_turn(
            &db,
            "claude:day",
            "2026-09-29 10:00:00",
            2_000,
            100,
            1_000,
            100,
            false,
        );
        insert_turn(
            &db,
            "claude:day",
            "2026-09-29 10:00:30",
            2_000,
            100,
            1_000,
            100,
            false,
        );

        let today = db
            .query_session_metrics_on("claude:day", "2026-09-29")
            .unwrap();
        assert_eq!(today.turns, 2, "yesterday's turn must be excluded");
        assert_eq!(today.output_tokens, 200);

        // The same session, read as of yesterday, sees only that day.
        let yesterday = db
            .query_session_metrics_on("claude:day", "2026-09-28")
            .unwrap();
        assert_eq!(yesterday.turns, 1);
        assert_eq!(yesterday.output_tokens, 100_000);

        // A day with no traffic is empty rather than an error.
        let empty = db
            .query_session_metrics_on("claude:day", "2026-09-27")
            .unwrap();
        assert_eq!(empty.turns, 0);
        assert_eq!(empty.tps(), None);
    }

    /// The day filter applies to the comparison columns too, so a session that
    /// ran only on an earlier day is not listed beside today's.
    #[test]
    fn recent_sessions_are_restricted_to_the_requested_day() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:old",
            "2026-09-28 10:00:00",
            1_000,
            10,
            1_000,
            500,
            false,
        );
        insert_turn(
            &db,
            "claude:new",
            "2026-09-29 10:00:00",
            1_000,
            10,
            1_000,
            500,
            false,
        );

        let today = db.query_recent_session_metrics_on(5, "2026-09-29").unwrap();
        assert_eq!(today.len(), 1, "yesterday-only session must not be listed");
        assert_eq!(today[0].session_id, "claude:new");

        let yesterday = db.query_recent_session_metrics_on(5, "2026-09-28").unwrap();
        assert_eq!(yesterday.len(), 1);
        assert_eq!(yesterday[0].session_id, "claude:old");
    }

    /// The card's session is today's last one, not the most recent ever.
    #[test]
    fn latest_session_id_ignores_an_earlier_day() {
        let db = StatsDb::in_memory().unwrap();
        // Inserted second, so it also has the higher row id — only the day
        // filter can keep it out.
        insert_turn(
            &db,
            "claude:today",
            "2026-09-29 09:00:00",
            1_000,
            10,
            1_000,
            100,
            false,
        );
        insert_turn(
            &db,
            "claude:yesterday",
            "2026-09-28 23:00:00",
            1_000,
            10,
            1_000,
            100,
            false,
        );

        assert_eq!(
            db.latest_session_id_on("2026-09-29").unwrap().as_deref(),
            Some("claude:today")
        );
        assert_eq!(
            db.latest_session_id_on("2026-09-28").unwrap().as_deref(),
            Some("claude:yesterday")
        );
        assert_eq!(db.latest_session_id_on("2026-09-27").unwrap(), None);
    }

    /// Request count and cache-hit rate over the window.
    ///
    /// The rate uses the same formula as the overview's daily figure
    /// (`cache_read / (input + cache_read)`), so a session's number and the
    /// day's number mean the same thing.
    #[test]
    fn session_metrics_report_request_count_and_cache_hit_rate() {
        let db = StatsDb::in_memory().unwrap();
        // 300 fresh input + 700 cached = 70% hit, over two requests.
        insert_turn_with_prompt(
            &db,
            "claude:cache",
            "2026-09-29 10:00:00",
            100,
            400,
            1_000,
            50,
            1_000,
            10,
        );
        insert_turn_with_prompt(
            &db,
            "claude:cache",
            "2026-09-29 10:00:10",
            200,
            300,
            1_000,
            50,
            1_000,
            10,
        );

        let m = db
            .query_session_metrics_on("claude:cache", "2026-09-29")
            .unwrap();
        assert_eq!(m.turns, 2, "requests = turns in the day window");
        assert_eq!(m.input_tokens, 300);
        assert_eq!(m.cache_read_tokens, 700);
        assert_eq!(m.cache_hit_pct(), Some(70));
    }

    /// An unmeasured turn still counts toward the token totals.
    ///
    /// These are quantities, not rates: a plain non-streamed reply contributes
    /// no generation time but its prompt tokens are real traffic, and dropping
    /// them would understate the session's input.
    #[test]
    fn prompt_tokens_count_even_on_an_unmeasured_turn() {
        let db = StatsDb::in_memory().unwrap();
        // model_ms = 0 => unmeasurable for the rate, but tokens are recorded.
        insert_turn_with_prompt(
            &db,
            "claude:u",
            "2026-09-29 10:00:00",
            100,
            900,
            500,
            0,
            0,
            50,
        );

        let m = db
            .query_session_metrics_on("claude:u", "2026-09-29")
            .unwrap();
        assert_eq!(m.turns, 1);
        assert_eq!(m.measured_turns, 0);
        assert_eq!(m.tps(), None, "no generation span, so no rate");
        assert_eq!(m.input_tokens, 100);
        assert_eq!(m.cache_read_tokens, 900);
        assert_eq!(m.cache_hit_pct(), Some(90));
    }

    /// No prompt traffic means no hit rate — `None`, not 0%.
    ///
    /// A session with requests but no token accounting must not claim it cached
    /// nothing; the panel renders `None` as `—`.
    #[test]
    fn cache_hit_is_none_rather_than_zero_without_prompt_traffic() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:notokens",
            "2026-09-29 10:00:00",
            1_000,
            50,
            1_000,
            10,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:notokens", "2026-09-29")
            .unwrap();
        assert_eq!(m.turns, 1);
        assert_eq!(m.cache_hit_pct(), None);
        // A defaulted aggregate reports nothing at all.
        assert_eq!(SessionMetrics::default().cache_hit_pct(), None);
    }

    /// The defaulted metrics report nothing rather than a misleading zero.
    #[test]
    fn default_session_metrics_have_no_cache_or_turns() {
        let m = SessionMetrics::default();
        assert_eq!(m.turns, 0);
        assert_eq!(m.cache_hit_pct(), None);
        assert_eq!(m.input_tokens, 0);
        assert_eq!(m.cache_read_tokens, 0);
    }

    /// A turn written through the real logging path is found by "today".
    ///
    /// The other session-metric tests insert rows directly with an explicit
    /// date, which cannot catch the failure this one guards: `record_request_log`
    /// stamping a `date` the day filter does not match (wrong format, wrong
    /// timezone, empty). That would make the whole panel silently show "—".
    #[test]
    fn a_turn_recorded_now_is_visible_to_the_today_filter() {
        let db = StatsDb::in_memory().unwrap();

        let tokens = TokenRecord {
            input: 40,
            cache_read: 60,
            cache_write: 0,
            output: 25,
        };
        let mut out = outcome(
            "claude-3-5-sonnet-20241022",
            "/v1/messages",
            &tokens,
            200,
            None,
            true,
        );
        out.session_id = "claude:live";
        out.client = "claude";
        db.record_request_log(out).unwrap();

        // `query_session_metrics` (no explicit date) is exactly what the GUI
        // command calls, so this covers the production path end to end.
        let m = db.query_session_metrics("claude:live").unwrap();
        assert_eq!(m.turns, 1, "the row must fall inside today's window");
        assert_eq!(m.input_tokens, 40);
        assert_eq!(m.cache_read_tokens, 60);
        assert_eq!(m.cache_hit_pct(), Some(60));

        // And the card's session is found the same way.
        assert_eq!(
            db.latest_session_id().unwrap().as_deref(),
            Some("claude:live")
        );
        assert_eq!(db.query_recent_session_metrics(3).unwrap().len(), 1);
    }

    #[test]
    fn daily_stats_are_derived_from_request_logs() {
        let db = StatsDb::in_memory().unwrap();

        let ok = TokenRecord {
            input: 100,
            cache_read: 900,
            cache_write: 0,
            output: 50,
        };
        db.record_request_log(outcome(
            "claude-3-5-sonnet-20241022",
            "/v1/messages",
            &ok,
            200,
            None,
            true,
        ))
        .unwrap();

        let failed = TokenRecord::default();
        db.record_request_log(outcome(
            "gpt-4o",
            "/v1/chat/completions",
            &failed,
            502,
            Some("Upstream gateway error"),
            false,
        ))
        .unwrap();

        let today = db.query_today().unwrap();
        assert_eq!(today.requests_total, 2);
        assert_eq!(today.requests_success, 1);
        assert_eq!(today.requests_failed, 1);
        assert_eq!(today.tokens_input, 100);
        assert_eq!(today.tokens_cache_read, 900);
        assert_eq!(today.tokens_output, 50);
        assert_eq!(today.cache_hit_pct(), 90); // 900/(100+900)*100
        assert_eq!(today.tokens_total(), 1050);
        assert!(!today.date.is_empty());
    }

    /// Only rows whose `override_key` is actually set count as overrides.
    ///
    /// A request served exactly as configured must not inflate the count: the
    /// overview's 今日 override 次数 card is read as "how often did the proxy
    /// have to deviate today", and counting clean requests would make that
    /// number meaningless.
    #[test]
    fn override_count_only_counts_rows_with_an_override_key() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord::default();

        let mut overridden = outcome("m", "/v1/messages", &tokens, 200, None, true);
        overridden.override_key = "k-3ba06c";
        overridden.override_model = "hy4-preview";
        overridden.override_reason = "401 from wb-5735d0 (降级密钥池)";
        db.record_request_log(overridden).unwrap();

        db.record_request_log(outcome("m", "/v1/messages", &tokens, 200, None, true))
            .unwrap();

        // A failed request can still be an override — the failover did happen,
        // the request just did not succeed afterwards.
        let mut failed_override = outcome("m", "/v1/messages", &tokens, 502, Some("boom"), true);
        failed_override.override_key = "wb-a85801";
        db.record_request_log(failed_override).unwrap();

        let today = db.query_today().unwrap();
        assert_eq!(today.requests_total, 3);
        assert_eq!(today.overrides_total, 2, "clean rows must not count");

        db.clear_request_logs().unwrap();
        assert_eq!(db.query_today().unwrap().overrides_total, 0);
    }

    #[test]
    fn request_logs_crud_and_filtering() {
        let db = StatsDb::in_memory().unwrap();

        // 1. Record success log
        let ok_tokens = TokenRecord {
            input: 150,
            cache_read: 50,
            cache_write: 0,
            output: 200,
        };
        db.record_request_log(outcome(
            "claude-3-5-sonnet-20241022",
            "/v1/messages",
            &ok_tokens,
            200,
            None,
            true,
        ))
        .unwrap();

        // 2. Record failure log
        let err_tokens = TokenRecord::default();
        db.record_request_log(outcome(
            "gpt-4o",
            "/v1/chat/completions",
            &err_tokens,
            502,
            Some("Upstream gateway error"),
            false,
        ))
        .unwrap();

        // Query all
        let all = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert_eq!(all.total, 2);
        assert_eq!(all.items.len(), 2);
        assert_eq!(all.models.len(), 2);
        assert_eq!(all.items[0].model, "gpt-4o"); // Most recent first (id DESC)
        assert_eq!(all.items[0].status, 502);
        assert!(!all.items[0].streamed);
        assert_eq!(all.items[1].model, "claude-3-5-sonnet-20241022");
        assert_eq!(all.items[1].status, 200);
        assert!(all.items[1].streamed);
        assert_eq!(all.items[1].input_tokens, 150);

        // Filter by model
        let claude_only = db
            .query_request_logs(&RequestLogFilter {
                model: Some("claude-3-5-sonnet-20241022".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(claude_only.total, 1);
        assert_eq!(claude_only.items[0].model, "claude-3-5-sonnet-20241022");

        // Filter by status success (2xx)
        let success_only = db
            .query_request_logs(&RequestLogFilter {
                status_group: Some("2xx".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(success_only.total, 1);
        assert_eq!(success_only.items[0].status, 200);

        // Filter by status error
        let error_only = db
            .query_request_logs(&RequestLogFilter {
                status_group: Some("error".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(error_only.total, 1);
        assert_eq!(error_only.items[0].status, 502);

        // Filter by streamed
        let stream_only = db
            .query_request_logs(&RequestLogFilter {
                streamed: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(stream_only.total, 1);
        assert!(stream_only.items[0].streamed);

        // Clear logs
        db.clear_request_logs().unwrap();
        let empty = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert_eq!(empty.total, 0);
        assert_eq!(empty.items.len(), 0);
    }

    #[test]
    fn daily_stats_reflect_cleared_logs() {
        // The two sources can no longer disagree: clearing the log zeroes the
        // counters, because the counters are computed from the log itself.
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord {
            input: 10,
            cache_read: 0,
            cache_write: 0,
            output: 5,
        };
        db.record_request_log(outcome("m", "/v1/messages", &tokens, 200, None, false))
            .unwrap();
        assert_eq!(db.query_today().unwrap().requests_total, 1);

        db.clear_request_logs().unwrap();
        assert_eq!(db.query_today().unwrap().requests_total, 0);
    }

    #[test]
    fn clearing_a_file_backed_database_actually_removes_the_rows() {
        // Regression: `clear_request_logs` now borrows through the read pool, so
        // it must still delete from the same database the writer thread fills.
        // The in-memory test cannot cover this — it shares a single connection.
        let dir = std::env::temp_dir().join(format!("proxy-rs-clear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.db");
        let db = StatsDb::open_at(&path).unwrap();

        for _ in 0..5 {
            db.record_request_log(outcome(
                "m",
                "/v1/messages",
                &TokenRecord::default(),
                200,
                None,
                true,
            ))
            .unwrap();
        }

        // Wait for the writer thread to commit before clearing.
        let mut total = 0;
        for _ in 0..200 {
            total = db.query_today().unwrap().requests_total;
            if total == 5 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(total, 5, "rows should be committed before the clear");

        db.clear_request_logs().unwrap();

        assert_eq!(
            db.query_today().unwrap().requests_total,
            0,
            "the delete must be visible to a subsequent read"
        );
        let listed = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert_eq!(listed.total, 0, "and to the request-log listing");
        assert!(listed.items.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_drops_legacy_daily_stats_and_backfills_dates() {
        // Simulate a database written by the previous schema: no `date` column,
        // plus the now-redundant `daily_stats` table.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE daily_stats (
                date TEXT PRIMARY KEY,
                requests_total INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL,
                model TEXT NOT NULL,
                route TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                streamed INTEGER NOT NULL DEFAULT 0,
                status INTEGER NOT NULL DEFAULT 0,
                error TEXT
             );
             INSERT INTO request_logs (created_at, model, route, status)
             VALUES ('2026-09-22 10:00:00', 'hy3', '/v1/responses', 200);",
        )
        .unwrap();

        StatsDb::init_schema(&conn).unwrap();

        let db = StatsDb::from_conn(conn);
        let stats = db.query_date("2026-09-22").unwrap();
        assert_eq!(stats.requests_total, 1, "date backfilled from created_at");
        assert_eq!(stats.requests_success, 1);
    }

    /// The override trio round-trips through the DB, so the detail view can
    /// explain *why* a non-default credential served a request.
    #[test]
    fn override_fields_round_trip_through_the_database() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord::default();
        let mut o = outcome("hy3", "/v1/responses", &tokens, 200, None, true);
        o.override_key = "wb-b1";
        o.override_model = "glm-5.3";
        o.override_reason = "429 from wb-a1; content_blocked 400";
        let _ = db.record_request_log(o);

        let res = db
            .query_request_logs(&RequestLogFilter {
                limit: Some(10),
                ..Default::default()
            })
            .unwrap();
        let item = &res.items[0];
        assert_eq!(item.override_key, "wb-b1");
        assert_eq!(item.override_model, "glm-5.3");
        assert_eq!(item.override_reason, "429 from wb-a1; content_blocked 400");
    }

    /// An older database gains the `override_reason` column (defaulting to the
    /// empty string) instead of failing to open.
    #[test]
    fn migration_adds_the_override_reason_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL,
                model TEXT NOT NULL,
                route TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                streamed INTEGER NOT NULL DEFAULT 0,
                status INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                session_id TEXT NOT NULL DEFAULT '',
                client TEXT NOT NULL DEFAULT '',
                override_key TEXT NOT NULL DEFAULT '',
                override_model TEXT NOT NULL DEFAULT ''
             );",
        )
        .unwrap();

        StatsDb::init_schema(&conn).unwrap();

        // The new column exists and old rows read back as an empty reason.
        let db = StatsDb::from_conn(conn);
        let res = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert!(res.items.is_empty());
    }

    #[test]
    fn sessions_are_listed_db_wide_not_just_from_the_current_page() {
        // The dropdown must offer conversations that are not on the visible
        // page, otherwise an older chat cannot be selected at all.
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord::default();

        let mut outcomes = Vec::new();
        for (session, client) in [
            ("claude:aaa", "claude"),
            ("claude:aaa", "claude"), // a second turn of the same chat
            ("codex:bbb", "codex"),
            ("", "unknown"), // unidentified: must not appear as a session
        ] {
            let mut o = outcome("hy3", "/v1/messages", &tokens, 200, None, true);
            o.session_id = session;
            o.client = client;
            outcomes.push(o);
        }
        for o in &outcomes {
            db.record_request_log(o.clone()).unwrap();
        }

        // Ask for a single row: the session list must still be complete.
        let res = db
            .query_request_logs(&RequestLogFilter {
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(res.items.len(), 1, "page is limited to one row");
        let ids: Vec<&str> = res.sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["codex:bbb", "claude:aaa"], "newest first");
        assert!(
            !ids.iter().any(|id| id.is_empty()),
            "an unidentified request is not a session"
        );
        assert_eq!(
            res.sessions
                .iter()
                .find(|s| s.session_id == "claude:aaa")
                .unwrap()
                .client,
            "claude"
        );
        // The duplicate turn collapses into one entry.
        assert_eq!(res.sessions.len(), 2);
    }

    #[test]
    fn concurrent_readers_do_not_share_one_connection() {
        // The point of the pool: the GUI polls the stats line and the request
        // table every two seconds, and one shared lock made those queue behind
        // each other. Two readers held at once must therefore use *different*
        // connections.
        let dir = std::env::temp_dir().join(format!("proxy-rs-pool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.db");
        let db = StatsDb::open_at(&path).unwrap();

        let a = db.read_conn();
        let b = db.read_conn();
        // Pointer identity is the assertion that matters: distinct connections,
        // not merely two guards over the same one.
        assert_ne!(
            a.get() as *const Connection,
            b.get() as *const Connection,
            "concurrent readers must not serialize on one connection"
        );

        drop((a, b));

        // Returned connections are kept for reuse. The pool settles at its
        // high-water mark (2 here, because the second read found an empty pool
        // and opened one) rather than growing with each query.
        for _ in 0..10 {
            let _ = db.read_conn();
        }
        assert_eq!(
            db.readers.lock().unwrap().len(),
            2,
            "the pool must plateau, not grow per query"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pooled_read_sees_rows_written_by_the_writer_thread() {
        // A pool is only useful if its connections observe the committed data:
        // separate connections to the same file, not snapshots.
        let dir = std::env::temp_dir().join(format!("proxy-rs-poolvis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.db");
        let db = StatsDb::open_at(&path).unwrap();

        db.record_request_log(outcome(
            "hy3",
            "/v1/messages",
            &TokenRecord::default(),
            200,
            None,
            true,
        ))
        .unwrap();

        // Poll until the writer thread commits (it is asynchronous by design).
        let mut seen = 0;
        for _ in 0..200 {
            seen = db.query_today().unwrap().requests_total;
            if seen == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(seen, 1, "a pooled read must see the committed row");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_in_memory_database_shares_its_single_handle() {
        // A second connection to `:memory:` would be a different, empty
        // database, so in-memory mode must keep sharing one handle.
        let db = StatsDb::in_memory().unwrap();
        db.record_request_log(outcome(
            "hy3",
            "/v1/messages",
            &TokenRecord::default(),
            200,
            None,
            true,
        ))
        .unwrap();
        assert_eq!(
            db.query_today().unwrap().requests_total,
            1,
            "inline writes must be visible to reads"
        );
        assert!(
            db.reader_source.is_none(),
            "in-memory has no path to reopen"
        );
    }

    #[test]
    fn writer_thread_persists_queued_rows() {
        // Persistent mode: record_request_log only queues, a background thread
        // commits. The queue must eventually land every row.
        let dir = std::env::temp_dir().join(format!("proxy-rs-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.db");

        let read_conn = StatsDb::connect(&path).unwrap();
        StatsDb::init_schema(&read_conn).unwrap();
        let write_conn = StatsDb::connect(&path).unwrap();
        let (tx, rx) = channel::<RequestRow>();
        std::thread::spawn(move || writer_loop(write_conn, rx));

        let db = StatsDb {
            readers: Mutex::new(vec![read_conn]),
            reader_source: Some(path.clone()),
            conn: Arc::new(Mutex::new(StatsDb::connect(&path).unwrap())),
            queue: Some(tx),
        };

        let tokens = TokenRecord {
            input: 20,
            cache_read: 80,
            cache_write: 0,
            output: 20,
        };
        for _ in 0..50 {
            db.record_request_log(outcome("hy3", "/v1/responses", &tokens, 200, None, true))
                .unwrap();
        }

        // Poll until the writer thread has drained the queue.
        let mut total = 0;
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(10));
            total = db.query_today().unwrap().requests_total;
            if total == 50 {
                break;
            }
        }
        assert_eq!(total, 50);

        let today = db.query_today().unwrap();
        assert_eq!(today.tokens_input, 20 * 50);
        assert_eq!(today.tokens_cache_read, 80 * 50);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
