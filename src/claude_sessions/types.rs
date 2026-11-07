//! Wire shapes for the 会话 page: summaries, detail, totals and the request arguments.

use super::*;

/// Zero-cost namespace used when ids are joined against `stats.db`.
pub(crate) const GATEWAY_PREFIX: &str = "claude:";

pub const DEFAULT_LIMIT: usize = 50;

pub const MAX_LIMIT: usize = 200;

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ListArgs {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
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
    /// Gateway totals joined by `claude:<id>`; `None` = not recorded, which is
    /// not the same claim as zero requests.
    pub gateway: Option<GatewayUsage>,
    /// Gateway query failed; the local row is still shown.
    pub gateway_error: Option<String>,
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

pub(crate) struct Candidate {
    pub(crate) path: PathBuf,
    pub(crate) encoded_project: String,
    pub(crate) mtime: i64,
    pub(crate) id: String,
}
