//! Wire shapes for the 会话 page: summaries, detail, totals and the request arguments.

use super::*;

/// Zero-cost namespace used when ids are joined against `stats.db`.
pub(crate) const GATEWAY_PREFIX: &str = "claude:";

pub const DEFAULT_LIMIT: usize = 50;

pub const MAX_LIMIT: usize = 200;

/// How many sessions of each project group the grouped list returns by default.
///
/// Ten is the number that makes the grouped response cost the *same* as the flat
/// one on real data: this machine's group sizes are `[10, 8, 4, 2, 2, 1, 1, 1,
/// 1, 1]`, so a cap of ten opens exactly the 31 files the flat `limit=50` opens
/// today. It is a measurement, not a guess about screen height.
pub const DEFAULT_GROUP_LIMIT: usize = 10;

/// The one group-by value the wire accepts for now.
///
/// An unknown value is an "无效的" error rather than a silent fallback to the
/// flat list: a client that asked for grouping and silently got rows would
/// render a half-empty sidebar with nothing to explain it.
pub const GROUP_BY_PROJECT: &str = "project";

/// How much of a group's newest transcript is read to learn its `cwd`.
///
/// Only the head, and only up to the first line carrying a `cwd`. On real data
/// the first `cwd` sits at byte 188 (median 507, max 52 491), so 64 KiB covers
/// every file on this machine with room to spare — while staying far away from
/// "read the whole session just to label a group".
pub(crate) const GROUP_CWD_PROBE_BYTES: u64 = 64 * 1024;

/// Head and tail window for the list view. The list only needs the first lines
/// (`cwd`, `version`, the opening prompt) and the last ones (`ai-title`,
/// `cost-state`, the latest timestamp), so a 10 MB transcript is read at most
/// this much rather than in full.
pub(crate) const HEAD_TAIL_BYTES: u64 = 256 * 1024;

/// A single JSONL line beyond this is dropped instead of buffered. Attachment
/// snapshots are the only lines that grow this big.
pub(crate) const MAX_LINE_BYTES: u64 = 32 * 1024 * 1024;

/// Entry text is capped by *characters* (not bytes) and flagged when cut.
pub(crate) const TEXT_CAP: usize = 4_000;

pub(crate) const TOOL_INPUT_CAP: usize = 2_000;

pub(crate) const DETAIL_LIMIT: usize = 200;

pub(crate) const DETAIL_MAX_LIMIT: usize = 1_000;

/// A conversation as the sidebar lists it.
///
/// Produced either from a head+tail read (`counts_partial: true` — the counts
/// are lower bounds) or a full parse (exact). The detail pane always replaces
/// approximate figures with exact ones.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    /// Project directory name as Claude Code encoded it (`/` → `-`).
    pub encoded_project: String,
    /// Working directory, from the transcript's own `cwd` field. Empty when no
    /// line carries one — the directory name is *not* used as a fallback,
    /// because `-` also appears in real path segments and decoding is ambiguous.
    pub cwd: String,
    pub title: String,
    pub first_seen_ms: Option<i64>,
    pub last_seen_ms: Option<i64>,
    pub file_mtime_ms: i64,
    pub size_bytes: u64,
    pub git_branch: Option<String>,
    pub version: Option<String>,
    /// Human prompts (see [`human_prompt`]).
    pub turns: u32,
    /// Distinct assistant turns (see the module notes on turn keys).
    pub assistant_turns: u32,
    pub tool_calls: u32,
    /// `agent-*.jsonl` files under `<id>/subagents/`.
    pub subagents: u32,
    pub last_model: Option<String>,
    /// True when the file was read head+tail only, so `turns` / `tool_calls`
    /// are lower bounds and `first_seen_ms` may be early-even.
    pub counts_partial: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenTotals {
    pub input: i64,
    pub output: i64,
    pub cache_read: i64,
    pub cache_write: i64,
    pub thinking: i64,
}

impl TokenTotals {
    pub(crate) fn add(&mut self, other: &TokenTotals) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.thinking += other.thinking;
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelUsage {
    pub model: String,
    pub turns: u32,
    pub tokens: TokenTotals,
}

/// The last `cost-state` line, verbatim in intent: what Claude Code itself
/// computed for the session. Present only when the transcript has one.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionCostState {
    pub total_cost_usd: f64,
    pub total_duration_ms: i64,
    pub api_duration_ms: i64,
    pub tool_duration_ms: i64,
    pub start_time_ms: Option<i64>,
    pub lines_added: i64,
    pub lines_removed: i64,
    pub has_unknown_model_cost: bool,
    pub models: Vec<ModelUsage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryRole {
    User,
    Assistant,
    // The default: a value that fails to parse must not read as a prompt.
    #[default]
    System,
    Attachment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    Text,
    Thinking,
    ToolUse,
    ToolResult,
    #[default]
    Meta,
}

/// One thing to put on the timeline.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TimelineEntry {
    /// Unique within the page; a multi-block line yields `"<uuid>-<i>"`.
    pub uuid: String,
    pub parent_uuid: Option<String>,
    pub ts_ms: Option<i64>,
    pub role: EntryRole,
    pub kind: EntryKind,
    /// Entry produced by a subagent (`isSidechain`).
    pub sidechain: bool,
    pub text: String,
    pub text_truncated: bool,
    pub tool_name: Option<String>,
    pub tool_input: Option<String>,
    pub tool_use_id: Option<String>,
    pub tool_result_is_error: Option<bool>,
    pub model: Option<String>,
    /// Usage for this turn, on the first entry of a turn only; `None` on the
    /// repeats that share the same `message.id`.
    pub tokens: Option<TokenTotals>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubagentMeta {
    pub agent_id: String,
    pub agent_type: Option<String>,
    pub description: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IgnoreCounts {
    pub user_noise: u32,
    pub duplicate_uuid: u32,
    pub other: u32,
    pub malformed: u32,
}

/// One list request.
///
/// Every new field is `Option` and defaults to "3.5.5 behaviour", which is what
/// makes the grouped list a pure addition: a caller that sends none of them gets
/// a byte-identical response.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListArgs {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    /// `None` (or `"none"`) = flat list, exactly as before. `"project"` groups.
    pub group_by: Option<String>,
    /// Restrict to one group, by the opaque `key` a previous response returned.
    /// Only meaningful with `group_by`; `offset`/`limit` then apply *within* it.
    pub group: Option<String>,
    /// Sessions returned per group in grouped mode. Clamped like `limit`.
    pub group_limit: Option<usize>,
    /// Source slug (`claude`, …). `None` = every readable source.
    pub source: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DetailArgs {
    pub offset: Option<usize>,
    pub limit: Option<usize>,
}

/// Gateway totals attached to one local session, or `None` when the gateway
/// never recorded it.
pub type GatewayUsage = SessionUsageTotal;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionRow {
    #[serde(flatten)]
    pub summary: SessionSummary,
    /// Which source produced this row (`claude`, …).
    ///
    /// Belongs to the row, not the summary: it answers "who gave me this", which
    /// is a property of the transport that found the file, while `SessionSummary`
    /// is a property of the conversation itself.
    pub source: String,
    /// Gateway totals joined by `<source>:<id>`; `None` = not recorded, which is
    /// not the same claim as zero requests.
    pub gateway: Option<GatewayUsage>,
    /// Gateway query failed; the local row is still shown.
    pub gateway_error: Option<String>,
}

/// One project group in the grouped list.
///
/// `count` / `last_seen_ms` / `size_bytes` come from `stat` only — they are exact
/// for the whole group, which is what the group header promises ("12 个会话 ·
/// 2 小时前"). Per-row `turns` / `tool_calls` stay "only for the rows we opened",
/// because computing those for a whole group means reading every file in it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionGroup {
    /// Opaque to the client; pass it back as `group=`.
    pub key: String,
    /// Display name — the last segment of `cwd` when known.
    pub label: String,
    /// Normalised absolute path from the group's newest transcript; `""` when
    /// no line carried one.
    ///
    /// Carried separately from `key` because the key is a *directory name*, and
    /// Claude Code's encoding of `/` to `-` is lossy: `/a/b/c` and `/a-b-c`
    /// encode alike. A future cross-source merge needs the real path, so it goes
    /// on the wire now even though 3.5.6 never merges.
    pub cwd: String,
    /// Source slugs that produced sessions in this group, ascending, deduped.
    pub sources: Vec<String>,
    /// Sessions in the group (stat; exact).
    pub count: usize,
    /// Newest transcript mtime in the group (stat; exact).
    pub last_seen_ms: i64,
    /// Sum of the group's transcript sizes (stat; exact).
    pub size_bytes: u64,
    /// `count > sessions.len()` — the group was truncated by `group_limit`.
    pub more: bool,
    pub sessions: Vec<SessionRow>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListPage {
    pub available: bool,
    /// Present only when `available` is false.
    pub reason: Option<String>,
    pub root: String,
    pub stats_available: bool,
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub sessions: Vec<SessionRow>,
    /// `None` when the request did not ask for grouping. That is what keeps the
    /// 3.5.5 response byte-identical for callers that send no `group_by`.
    pub group_by: Option<String>,
    pub groups: Option<Vec<SessionGroup>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub cost_state: Option<SessionCostState>,
    pub models: Vec<ModelUsage>,
    /// Main conversation only.
    pub tokens: TokenTotals,
    /// Main conversation plus subagent (`isSidechain`) turns.
    pub tokens_incl_subagents: TokenTotals,
    /// This page, in chronological order.
    pub timeline: Vec<TimelineEntry>,
    pub timeline_offset: usize,
    pub timeline_total: usize,
    pub has_more: bool,
    pub ignored: IgnoreCounts,
    pub subagents: Vec<SubagentMeta>,
    /// Which file this detail came from: the session itself or one subagent.
    pub agent_id: Option<String>,
    pub gateway: Option<GatewayUsage>,
    pub gateway_error: Option<String>,
}

/// One transcript file found by a source's directory scan, before it is opened.
///
/// `pub(crate)` on purpose: this is the cheapest injection point for everything
/// the grouped list needs, and changing it cannot break the wire. `size_bytes`
/// is filled from the `stat` the scan already did, so group totals cost no extra
/// I/O.
#[derive(Debug, Clone)]
pub(crate) struct Candidate {
    pub(crate) path: PathBuf,
    pub(crate) encoded_project: String,
    pub(crate) mtime: i64,
    pub(crate) size_bytes: u64,
    pub(crate) id: String,
    /// Source slug this candidate came from (`claude`, …).
    pub(crate) source: String,
}
