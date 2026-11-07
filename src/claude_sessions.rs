//! Claude Code local session transcripts, for the console's 会话 page.
//!
//! ## What it reads
//!
//! Claude Code writes one append-only JSONL transcript per conversation at
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. The file name is the
//! session id, and that is the same id the gateway sees in
//! `x-claude-code-session-id` (namespaced `claude:<uuid>` in `stats.db`) — which
//! is what makes the usage join in [`crate::admin::claude_sessions_list`]
//! possible without any new bookkeeping.
//!
//! Subagent dispatches leave `agent-*.jsonl` files under
//! `<session-id>/subagents/`; they are deliberately **not** listed as sessions
//! (the directory walk never descends), because the subagent's own turns are
//! already inside the parent transcript as `isSidechain: true` entries.
//!
//! ## Why this module is synchronous
//!
//! Every function here does blocking file IO and knows nothing about Tokio or
//! Tauri. Callers wrap a whole request in one `spawn_blocking` — see
//! [`crate::admin::claude_sessions_list`]. Putting the async adaptation inside
//! would invite a `spawn_blocking` per file, which is the one thing this must
//! not do: the blocking pool is a small, shared resource.
//!
//! ## The three parsing traps in real data
//!
//! 1. **One API turn, several `assistant` lines.** Claude Code splits a response
//!    into one line per content block, and every split line carries the *same*
//!    `message.usage`. Summing per line inflates tokens several-fold, so usage
//!    is folded once per turn key `(message.id | uuid, requestId)`.
//! 2. **Resumed sessions replay their history.** The same `uuid` can be
//!    appended again; entries are deduplicated by `uuid`.
//! 3. **`cost-state` is a snapshot that half the files never write.** Nothing
//!    the page presents as authoritative is derived from it; it is shown only
//!    as "Claude Code 自报" when present.
//!
//! Timestamps are returned as epoch milliseconds and formatted by the browser:
//! the server's timezone is not the viewer's, and a UTC string printed with the
//! wrong offset is a silent, plausible-looking error.
//!
//! ## Known non-goals
//!
//! DSH sessions (`~/.dsh/sessions/**/session.vN.jsonl.zstd`, ~492 MB on a
//! typical machine) are zstd-compressed and are not read here. The console
//! shows that source as disabled with an explanation.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::settings::home_dir;
use crate::stats::SessionUsageTotal;

/// Zero-cost namespace used when ids are joined against `stats.db`.
const GATEWAY_PREFIX: &str = "claude:";

pub const DEFAULT_LIMIT: usize = 50;
pub const MAX_LIMIT: usize = 200;

/// Head and tail window for the list view. The list only needs the first lines
/// (`cwd`, `version`, the opening prompt) and the last ones (`ai-title`,
/// `cost-state`, the latest timestamp), so a 10 MB transcript is read at most
/// this much rather than in full.
const HEAD_TAIL_BYTES: u64 = 256 * 1024;
/// A single JSONL line beyond this is dropped instead of buffered. Attachment
/// snapshots are the only lines that grow this big.
const MAX_LINE_BYTES: u64 = 32 * 1024 * 1024;
/// Entry text is capped by *characters* (not bytes) and flagged when cut.
const TEXT_CAP: usize = 4_000;
const TOOL_INPUT_CAP: usize = 2_000;
const DETAIL_LIMIT: usize = 200;
const DETAIL_MAX_LIMIT: usize = 1_000;

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
    fn add(&mut self, other: &TokenTotals) {
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

/// Where transcripts are read from.
///
/// Built from a path so tests never touch the real home; production builds one
/// with [`Self::from_env`].
#[derive(Debug, Clone)]
pub struct ClaudeSessionsSource {
    pub projects_root: PathBuf,
}

impl ClaudeSessionsSource {
    pub fn new(projects_root: impl Into<PathBuf>) -> Self {
        Self {
            projects_root: projects_root.into(),
        }
    }

    /// `$CLAUDE_PROJECTS_DIR` (test/override hook) → `~/.claude/projects`.
    ///
    /// `None` when the directory does not exist: a machine that never ran
    /// Claude Code has no sessions, and the caller reports that as an
    /// explanatory payload rather than an error.
    pub fn from_env() -> Option<Self> {
        if let Some(dir) = std::env::var_os("CLAUDE_PROJECTS_DIR") {
            let path = PathBuf::from(dir);
            if path.is_dir() {
                return Some(Self::new(path));
            }
            return None;
        }
        let home = home_dir()?;
        let path = home.join(".claude").join("projects");
        path.is_dir().then(|| Self::new(path))
    }
}

// ── Id validation and paths ─────────────────────────────────────────────

/// Strict lowercase-hex UUID (`8-4-4-4-12`). This one check rejects `..`,
/// separators, absolute paths and symlink tricks before any path is built.
pub fn is_valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() || b.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}

/// Agent file suffix: the bare id after `agent-`, without the extension.
pub fn is_valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

fn agent_id_from_file_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".jsonl")?;
    let id = stem.strip_prefix("agent-")?;
    is_valid_agent_id(id).then(|| id.to_string())
}

/// Locate `<root>/<any-project>/<id>.jsonl`, confined to the root.
pub fn resolve_session_path(src: &ClaudeSessionsSource, id: &str) -> Result<PathBuf> {
    if !is_valid_session_id(id) {
        bail!("无效的会话 ID");
    }
    let root = src
        .projects_root
        .canonicalize()
        .with_context(|| format!("会话根目录不可读: {}", src.projects_root.display()))?;
    let wanted = format!("{id}.jsonl");
    let entries = fs::read_dir(&root).with_context(|| format!("读取失败: {}", root.display()))?;
    for entry in entries.flatten() {
        // Never follow symlinks: a project directory is a real directory or it
        // is not one this module reads.
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let candidate = entry.path().join(&wanted);
        let Ok(canon) = candidate.canonicalize() else {
            continue;
        };
        if canon.starts_with(&root) && canon.is_file() {
            return Ok(canon);
        }
    }
    bail!("会话不存在: {id}")
}

// ── Pure helpers ────────────────────────────────────────────────────────

/// `"2026-10-25T11:34:10.152Z"` → epoch ms. Accepts 0–9 fractional digits and
/// an optional `+HH:MM` / `-HH:MM` offset.
pub fn parse_iso_ms(s: &str) -> Option<i64> {
    let bytes = s.as_bytes();
    if bytes.len() < 20 {
        return None;
    }
    let num = |range: std::ops::Range<usize>| -> Option<i64> {
        let slice = s.get(range)?;
        if slice.bytes().all(|b| b.is_ascii_digit()) {
            slice.parse().ok()
        } else {
            None
        }
    };
    let year = num(0..4)?;
    let month = num(5..7)?;
    let day = num(8..10)?;
    let hour = num(11..13)?;
    let minute = num(14..16)?;
    let second = num(17..19)?;
    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    // Fractional seconds, then the offset.
    let mut rest = &s[19..];
    let mut millis = 0i64;
    if let Some(frac) = rest.strip_prefix('.') {
        let digits: String = frac.chars().take_while(|c| c.is_ascii_digit()).collect();
        if digits.len() > 9 {
            return None;
        }
        let padded = format!("{digits:0<3}");
        millis = padded[..3].parse().ok()?;
        rest = &frac[digits.len()..];
    }
    let offset_minutes = match rest {
        "Z" | "z" | "" => 0,
        other => {
            let sign = match other.as_bytes().first()? {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let body = &other[1..];
            let (h, m) = if let Some((h, m)) = body.split_once(':') {
                (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
            } else if body.len() == 4 {
                (body[..2].parse().ok()?, body[2..].parse().ok()?)
            } else {
                return None;
            };
            sign * (h * 60 + m)
        }
    };
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let days = days_from_civil(year, month, day);
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(secs * 1_000 + millis)
}

/// Howard Hinnant's `days_from_civil`, valid for the whole proleptic period.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Whether a `user` text is a real prompt rather than injected machinery.
///
/// Claude Code writes system reminders, background task notifications and
/// slash-command wrappers into the transcript as plain user strings. Counting
/// them would make every session look several times longer than it was.
pub fn human_prompt(text: &str) -> bool {
    let t = text.trim_start();
    if t.is_empty() {
        return false;
    }
    const NOISE: &[&str] = &[
        "<task-notification>",
        "<system-reminder>",
        "<local-command-stdout>",
        "<local-command-caveat>",
        "<command-name>",
        "Caveat: The messages below",
        "<user-prompt-submit-hook>",
    ];
    !NOISE.iter().any(|p| t.starts_with(p))
}

fn cap_chars(s: &str, cap: usize) -> (String, bool) {
    match s.char_indices().nth(cap) {
        Some((idx, _)) => (s[..idx].to_string(), true),
        None => (s.to_string(), false),
    }
}

fn ts_of(v: &serde_json::Value) -> Option<i64> {
    v.get("timestamp")
        .and_then(|t| t.as_str())
        .and_then(parse_iso_ms)
}

fn str_of<'a>(v: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(|x| x.as_str())
}

fn i64_of(v: &serde_json::Value, key: &str) -> i64 {
    v.get(key).and_then(|x| x.as_i64()).unwrap_or(0)
}

/// `usage`, tolerating the (observed) case of it being a JSON-encoded string.
fn usage_of(message: &serde_json::Value) -> Option<TokenTotals> {
    let usage = match message.get("usage")? {
        serde_json::Value::Object(_) => message.get("usage")?,
        serde_json::Value::String(s) => {
            let parsed: serde_json::Value = serde_json::from_str(s).ok()?;
            return usage_from_object(&parsed);
        }
        _ => return None,
    };
    usage_from_object(usage)
}

fn usage_from_object(u: &serde_json::Value) -> Option<TokenTotals> {
    if !u.is_object() {
        return None;
    }
    let thinking = u
        .get("output_tokens_details")
        .and_then(|d| d.get("thinking_tokens"))
        .and_then(|x| x.as_i64())
        .unwrap_or(0);
    Some(TokenTotals {
        input: i64_of(u, "input_tokens"),
        output: i64_of(u, "output_tokens"),
        cache_read: i64_of(u, "cache_read_input_tokens"),
        cache_write: i64_of(u, "cache_creation_input_tokens"),
        thinking,
    })
}

// ── Accumulator ─────────────────────────────────────────────────────────

/// Everything derived from a transcript while streaming its lines.
///
/// One pass feeds both the list (summary only) and the detail (summary + the
/// last `keep` timeline entries, bounded so a 10 MB file does not become a
/// 10 MB response).
struct Accum {
    summary: SessionSummary,
    seen_uuids: HashSet<String>,
    seen_turns: HashSet<(String, String)>,
    models: HashMap<String, ModelUsage>,
    tokens: TokenTotals,
    sidechain_tokens: TokenTotals,
    cost_state: Option<SessionCostState>,
    first_prompt: Option<String>,
    ignored: IgnoreCounts,
    timeline: VecDeque<TimelineEntry>,
    kept: usize,
    timeline_total: usize,
}

impl Accum {
    fn new(keep: usize) -> Self {
        Self {
            summary: SessionSummary::default(),
            seen_uuids: HashSet::new(),
            seen_turns: HashSet::new(),
            models: HashMap::new(),
            tokens: TokenTotals::default(),
            sidechain_tokens: TokenTotals::default(),
            cost_state: None,
            first_prompt: None,
            ignored: IgnoreCounts::default(),
            timeline: VecDeque::new(),
            kept: keep,
            timeline_total: 0,
        }
    }

    fn push_entry(&mut self, entry: TimelineEntry) {
        self.timeline_total += 1;
        if self.kept == 0 {
            return;
        }
        self.timeline.push_back(entry);
        while self.timeline.len() > self.kept {
            self.timeline.pop_front();
        }
    }

    fn note_times(&mut self, ts: Option<i64>) {
        let Some(ts) = ts else { return };
        self.summary.first_seen_ms = Some(self.summary.first_seen_ms.map_or(ts, |f| f.min(ts)));
        self.summary.last_seen_ms = Some(self.summary.last_seen_ms.map_or(ts, |l| l.max(ts)));
    }

    /// Tolerant per-line entry point. Malformed lines and unknown types are
    /// counted and skipped; nothing here can abort a file.
    fn absorb_line(&mut self, line: &str, keep_entries: bool) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        let v: serde_json::Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => {
                self.ignored.malformed += 1;
                return;
            }
        };
        let Some(kind) = str_of(&v, "type") else {
            self.ignored.other += 1;
            return;
        };
        match kind {
            "ai-title" => {
                if let Some(title) = str_of(&v, "aiTitle").filter(|t| !t.trim().is_empty()) {
                    self.summary.title = title.trim().to_string();
                } else {
                    self.ignored.other += 1;
                }
            }
            "cost-state" => self.cost_state = Some(parse_cost_state(&v)),
            "user" => self.absorb_user(&v, keep_entries),
            "assistant" => self.absorb_assistant(&v, keep_entries),
            "system" => self.absorb_context(&v),
            _ => {
                // attachment / file-history-* / mode / last-prompt / queue-operation / …
                self.absorb_context(&v);
                self.ignored.other += 1;
            }
        }
    }

    /// Header fields that can appear on any message line.
    fn absorb_context(&mut self, v: &serde_json::Value) {
        if self.summary.cwd.is_empty() {
            if let Some(cwd) = str_of(v, "cwd").filter(|s| !s.is_empty()) {
                self.summary.cwd = cwd.to_string();
            }
        }
        if self.summary.git_branch.is_none() {
            if let Some(b) = str_of(v, "gitBranch").filter(|s| !s.is_empty()) {
                self.summary.git_branch = Some(b.to_string());
            }
        }
        if self.summary.version.is_none() {
            if let Some(ver) = str_of(v, "version").filter(|s| !s.is_empty()) {
                self.summary.version = Some(ver.to_string());
            }
        }
    }

    fn is_duplicate(&mut self, v: &serde_json::Value) -> bool {
        match str_of(v, "uuid") {
            Some(uuid) if !uuid.is_empty() => {
                if self.seen_uuids.insert(uuid.to_string()) {
                    false
                } else {
                    self.ignored.duplicate_uuid += 1;
                    true
                }
            }
            _ => false,
        }
    }

    fn absorb_user(&mut self, v: &serde_json::Value, keep_entries: bool) {
        self.absorb_context(v);
        let ts = ts_of(v);
        self.note_times(ts);
        if self.is_duplicate(v) {
            return;
        }
        let uuid = str_of(v, "uuid").unwrap_or_default();
        let parent = str_of(v, "parentUuid").map(str::to_string);
        let sidechain = v
            .get("isSidechain")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let is_meta = v.get("isMeta").and_then(|x| x.as_bool()).unwrap_or(false);
        let message = v.get("message").cloned().unwrap_or(serde_json::Value::Null);
        let content = message
            .get("content")
            .cloned()
            .unwrap_or(serde_json::Value::Null);

        let mut entries: Vec<TimelineEntry> = Vec::new();
        let mut real_prompt = false;

        match content {
            serde_json::Value::String(text) => {
                if is_meta {
                    entries.push(self.text_entry(
                        uuid,
                        &parent,
                        ts,
                        sidechain,
                        EntryKind::Meta,
                        &text,
                    ));
                } else if human_prompt(&text) {
                    real_prompt = true;
                    entries.push(self.text_entry(
                        uuid,
                        &parent,
                        ts,
                        sidechain,
                        EntryKind::Text,
                        &text,
                    ));
                } else {
                    self.ignored.user_noise += 1;
                }
            }
            serde_json::Value::Array(blocks) => {
                let mut has_text = false;
                for (i, block) in blocks.iter().enumerate() {
                    let bt = str_of(block, "type").unwrap_or("");
                    let uid = format!("{uuid}-{i}");
                    match bt {
                        "text" => {
                            has_text = true;
                            let text = str_of(block, "text").unwrap_or_default();
                            entries.push(self.text_entry(
                                &uid,
                                &parent,
                                ts,
                                sidechain,
                                EntryKind::Text,
                                text,
                            ));
                        }
                        "tool_result" => {
                            let text = tool_result_text(block);
                            let (text, truncated) = cap_chars(&text, TEXT_CAP);
                            entries.push(TimelineEntry {
                                uuid: uid,
                                parent_uuid: parent.clone(),
                                ts_ms: ts,
                                role: EntryRole::User,
                                kind: EntryKind::ToolResult,
                                sidechain,
                                text,
                                text_truncated: truncated,
                                tool_use_id: str_of(block, "tool_use_id").map(str::to_string),
                                tool_result_is_error: block
                                    .get("is_error")
                                    .and_then(|x| x.as_bool()),
                                ..TimelineEntry::default()
                            });
                        }
                        _ => {}
                    }
                }
                // An array with no text is a tool result carrier, not a prompt.
                real_prompt = has_text && !is_meta;
            }
            _ => {}
        }

        if real_prompt {
            self.summary.turns += 1;
            if self.first_prompt.is_none() {
                let first = entries
                    .iter()
                    .find(|e| e.role == EntryRole::User && !e.sidechain)
                    .map(|e| e.text.clone());
                self.first_prompt = first.filter(|t| !t.trim().is_empty());
            }
        }
        if keep_entries {
            for e in entries {
                self.push_entry(e);
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn text_entry(
        &self,
        uuid: &str,
        parent: &Option<String>,
        ts: Option<i64>,
        sidechain: bool,
        kind: EntryKind,
        text: &str,
    ) -> TimelineEntry {
        let (text, text_truncated) = cap_chars(text, TEXT_CAP);
        TimelineEntry {
            uuid: uuid.to_string(),
            parent_uuid: parent.clone(),
            ts_ms: ts,
            role: EntryRole::User,
            kind,
            sidechain,
            text,
            text_truncated,
            ..TimelineEntry::default()
        }
    }

    fn absorb_assistant(&mut self, v: &serde_json::Value, keep_entries: bool) {
        self.absorb_context(v);
        let ts = ts_of(v);
        self.note_times(ts);
        if self.is_duplicate(v) {
            return;
        }
        let message = match v.get("message") {
            Some(serde_json::Value::Object(_)) => v.get("message").unwrap(),
            _ => {
                self.ignored.other += 1;
                return;
            }
        };
        let uuid = str_of(v, "uuid").unwrap_or_default();
        let parent = str_of(v, "parentUuid").map(str::to_string);
        let sidechain = v
            .get("isSidechain")
            .and_then(|x| x.as_bool())
            .unwrap_or(false);
        let model = str_of(message, "model")
            .filter(|m| !m.is_empty())
            .map(str::to_string);

        // Turn key: one API response is split across several lines sharing
        // `message.id`; some versions only carry `uuid` per line.
        let turn_id = str_of(message, "id")
            .map(str::to_string)
            .unwrap_or_else(|| uuid.to_string());
        let request_id = str_of(v, "requestId")
            .map(str::to_string)
            .unwrap_or_default();
        let is_new_turn = self.seen_turns.insert((turn_id, request_id));
        let usage = usage_of(message);
        let mut first_entry_tokens = None;
        if is_new_turn {
            self.summary.assistant_turns += 1;
            if let Some(usage) = &usage {
                if sidechain {
                    self.sidechain_tokens.add(usage);
                } else {
                    self.tokens.add(usage);
                }
                let name = model.clone().unwrap_or_else(|| "unknown".to_string());
                let row = self
                    .models
                    .entry(name.clone())
                    .or_insert_with(|| ModelUsage {
                        model: name,
                        ..ModelUsage::default()
                    });
                row.turns += 1;
                row.tokens.add(usage);
            }
            first_entry_tokens = usage;
        }
        if let Some(m) = &model {
            self.summary.last_model = Some(m.clone());
        }

        if !keep_entries {
            return;
        }
        let blocks = message.get("content");
        match blocks {
            Some(serde_json::Value::Array(blocks)) => {
                let mut tokens_attached = false;
                for (i, block) in blocks.iter().enumerate() {
                    let bt = str_of(block, "type").unwrap_or("");
                    let uid = format!("{uuid}-{i}");
                    let mut entry = TimelineEntry {
                        uuid: uid,
                        parent_uuid: parent.clone(),
                        ts_ms: ts,
                        role: EntryRole::Assistant,
                        sidechain,
                        model: model.clone(),
                        ..TimelineEntry::default()
                    };
                    match bt {
                        "text" => {
                            entry.kind = EntryKind::Text;
                            let (t, tr) =
                                cap_chars(str_of(block, "text").unwrap_or_default(), TEXT_CAP);
                            entry.text = t;
                            entry.text_truncated = tr;
                        }
                        "thinking" | "reasoning" => {
                            entry.kind = EntryKind::Thinking;
                            let text = str_of(block, "thinking")
                                .or_else(|| str_of(block, "text"))
                                .unwrap_or_default();
                            let (t, tr) = cap_chars(text, TEXT_CAP);
                            entry.text = t;
                            entry.text_truncated = tr;
                        }
                        "tool_use" => {
                            entry.kind = EntryKind::ToolUse;
                            entry.tool_name = str_of(block, "name").map(str::to_string);
                            entry.tool_use_id = str_of(block, "id").map(str::to_string);
                            if let Some(input) = block.get("input") {
                                let rendered = serde_json::to_string(input).unwrap_or_default();
                                let (t, tr) = cap_chars(&rendered, TOOL_INPUT_CAP);
                                entry.tool_input = Some(t);
                                entry.text_truncated = tr;
                            }
                            self.summary.tool_calls += 1;
                        }
                        _ => continue,
                    }
                    if !tokens_attached {
                        entry.tokens = first_entry_tokens.clone();
                        tokens_attached = true;
                    }
                    self.push_entry(entry);
                }
            }
            Some(serde_json::Value::String(text)) => {
                let mut entry = TimelineEntry {
                    uuid: uuid.to_string(),
                    parent_uuid: parent,
                    ts_ms: ts,
                    role: EntryRole::Assistant,
                    kind: EntryKind::Text,
                    sidechain,
                    model,
                    ..TimelineEntry::default()
                };
                let (t, tr) = cap_chars(text, TEXT_CAP);
                entry.text = t;
                entry.text_truncated = tr;
                entry.tokens = first_entry_tokens;
                self.push_entry(entry);
            }
            _ => {}
        }
    }

    fn finish(mut self) -> Self {
        if self.summary.title.trim().is_empty() {
            self.summary.title = self
                .first_prompt
                .take()
                .map(|p| cap_chars(&one_line(&p), 80).0)
                .unwrap_or_else(|| "(未命名会话)".to_string());
        }
        self
    }
}

fn one_line(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(200));
    for c in s.chars() {
        if c == '\n' || c == '\r' || c == '\t' {
            out.push(' ');
        } else {
            out.push(c);
        }
        if out.len() >= 400 {
            break;
        }
    }
    out.trim().to_string()
}

fn tool_result_text(block: &serde_json::Value) -> String {
    match block.get("content") {
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(serde_json::Value::Array(parts)) => parts
            .iter()
            .filter_map(|p| str_of(p, "text"))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn parse_cost_state(v: &serde_json::Value) -> SessionCostState {
    let f64_of = |key: &str| v.get(key).and_then(|x| x.as_f64()).unwrap_or(0.0);
    let mut models = Vec::new();
    if let Some(map) = v.get("modelUsage").and_then(|m| m.as_object()) {
        for (name, entry) in map {
            models.push(ModelUsage {
                model: name.clone(),
                turns: 0,
                tokens: TokenTotals {
                    input: i64_of(entry, "inputTokens"),
                    output: i64_of(entry, "outputTokens"),
                    cache_read: i64_of(entry, "cacheReadInputTokens"),
                    cache_write: i64_of(entry, "cacheCreationInputTokens"),
                    thinking: i64_of(entry, "thinkingTokens"),
                },
            });
        }
    }
    models.sort_by(|a, b| {
        b.tokens
            .output
            .cmp(&a.tokens.output)
            .then(a.model.cmp(&b.model))
    });
    SessionCostState {
        total_cost_usd: f64_of("totalCostUSD"),
        total_duration_ms: i64_of(v, "totalDuration"),
        api_duration_ms: i64_of(v, "totalAPIDuration"),
        tool_duration_ms: i64_of(v, "totalToolDuration"),
        start_time_ms: v.get("startTime").and_then(|x| x.as_i64()),
        lines_added: i64_of(v, "totalLinesAdded"),
        lines_removed: i64_of(v, "totalLinesRemoved"),
        has_unknown_model_cost: v
            .get("hasUnknownModelCost")
            .and_then(|x| x.as_bool())
            .unwrap_or(false),
        models,
    }
}

// ── Reading ─────────────────────────────────────────────────────────────

/// Iterate over complete lines of a reader, skipping (and counting) any single
/// line longer than [`MAX_LINE_BYTES`].
fn read_lines<R: BufRead>(reader: R, mut on_line: impl FnMut(&str)) -> Result<()> {
    let mut reader = reader;
    let mut buf: Vec<u8> = Vec::new();
    loop {
        buf.clear();
        let mut limited = (&mut reader).take(MAX_LINE_BYTES + 1);
        let n = limited.read_until(b'\n', &mut buf)?;
        if n == 0 {
            break;
        }
        if n as u64 > MAX_LINE_BYTES {
            // Drain the rest of the oversized line so the next read starts at
            // a line boundary.
            if buf.last() != Some(&b'\n') {
                let mut rest = Vec::new();
                let _ = reader.read_until(b'\n', &mut rest);
            }
            on_line("\u{0}oversized");
            continue;
        }
        let text = String::from_utf8_lossy(&buf);
        let text = text.trim_end_matches(['\n', '\r']);
        on_line(text);
    }
    Ok(())
}

fn file_mtime_ms(meta: &fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn meta_from_accum(acc: &Accum) -> Vec<ModelUsage> {
    let mut v: Vec<ModelUsage> = acc.models.values().cloned().collect();
    v.sort_by(|a, b| {
        b.tokens
            .output
            .cmp(&a.tokens.output)
            .then(a.model.cmp(&b.model))
    });
    v
}

/// Parser for the list path that only needs the summary.
fn summary_from_file(path: &Path, encoded_project: &str) -> Result<SessionSummary> {
    let meta = fs::metadata(path)?;
    let size = meta.len();
    let mtime = file_mtime_ms(&meta);
    let subagents = count_subagents(path);

    let mut acc = Accum::new(0);
    let mut full = false;
    if size <= HEAD_TAIL_BYTES * 2 {
        full = true;
        let file = fs::File::open(path)?;
        read_lines(BufReader::new(file), |l| acc.absorb_line(l, false))?;
    } else {
        // Head: cwd / version / opening prompt.
        let head = fs::File::open(path)?;
        read_lines(BufReader::new(head.take(HEAD_TAIL_BYTES)), |l| {
            acc.absorb_line(l, false)
        })?;
        // Tail: last ai-title (titles are rewritten as the session goes on),
        // last cost-state, latest timestamp.
        let mut file = fs::File::open(path)?;
        let start = size.saturating_sub(HEAD_TAIL_BYTES);
        file.seek(SeekFrom::Start(start))?;
        let mut reader = BufReader::new(file);
        // Skip the partial first line.
        let mut scratch = Vec::new();
        reader.read_until(b'\n', &mut scratch)?;
        read_lines(reader, |l| acc.absorb_line(l, false))?;
        // No title only occurs in tiny/degenerate files; fall back to a full
        // read so the list does not show "(未命名会话)" for a session that has one.
        if acc.summary.title.is_empty() {
            full = true;
            acc = Accum::new(0);
            let file = fs::File::open(path)?;
            read_lines(BufReader::new(file), |l| acc.absorb_line(l, false))?;
        }
    }

    let acc = acc.finish();
    let mut summary = acc.summary;
    summary.id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    summary.encoded_project = encoded_project.to_string();
    summary.file_mtime_ms = mtime;
    summary.size_bytes = size;
    summary.subagents = subagents;
    summary.counts_partial = !full;
    Ok(summary)
}

fn count_subagents(session_file: &Path) -> u32 {
    let dir = session_file.with_extension("").join("subagents");
    let Ok(entries) = fs::read_dir(dir) else {
        return 0;
    };
    entries
        .flatten()
        .filter(|e| {
            e.file_name()
                .to_str()
                .and_then(agent_id_from_file_name)
                .is_some()
        })
        .count() as u32
}

struct Candidate {
    path: PathBuf,
    encoded_project: String,
    mtime: i64,
    id: String,
}

fn candidates(root: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    let walk = walkdir::WalkDir::new(root)
        .min_depth(1)
        .max_depth(1)
        .follow_links(false)
        .sort_by_file_name();
    for entry in walk.into_iter().flatten() {
        if !entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let Ok(children) = fs::read_dir(entry.path()) else {
            continue;
        };
        for child in children.flatten() {
            if !child.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let fname = child.file_name().to_string_lossy().to_string();
            let Some(stem) = fname.strip_suffix(".jsonl") else {
                continue;
            };
            if !is_valid_session_id(stem) {
                continue;
            }
            let Ok(meta) = child.metadata() else {
                continue;
            };
            out.push(Candidate {
                path: child.path(),
                encoded_project: name.clone(),
                mtime: file_mtime_ms(&meta),
                id: stem.to_string(),
            });
        }
    }
    out
}

// ── Public API ──────────────────────────────────────────────────────────

/// All sessions, newest activity first (mtime), paginated.
///
/// Only the rows on the requested page are opened; the rest cost one `stat`.
pub fn list_sessions(src: &ClaudeSessionsSource, args: &ListArgs) -> Result<ListPage> {
    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = args.offset.unwrap_or(0);
    let mut all = candidates(&src.projects_root);
    all.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.id.cmp(&a.id)));
    let total = all.len();
    let mut sessions = Vec::new();
    for c in all.into_iter().skip(offset).take(limit) {
        match summary_from_file(&c.path, &c.encoded_project) {
            Ok(summary) => sessions.push(SessionRow {
                summary,
                ..SessionRow::default()
            }),
            Err(err) => {
                // One unreadable transcript must not blank the page.
                tracing::debug!(path = %c.path.display(), error = %err, "skip unreadable session");
            }
        }
    }
    Ok(ListPage {
        available: true,
        reason: None,
        root: src.projects_root.display().to_string(),
        stats_available: false,
        total,
        offset,
        limit,
        sessions,
    })
}

/// The namespaced ids for a page's rows, ready for [`crate::stats::StatsDb::session_usage_totals_for`].
pub fn gateway_ids(rows: &[SessionRow]) -> Vec<String> {
    rows.iter()
        .map(|r| format!("{GATEWAY_PREFIX}{}", r.summary.id))
        .collect()
}

/// Attach gateway totals by namespaced id. Ids with no rows get `None`, which
/// the UI renders as "网关未记录", not as zero.
pub fn attach_gateway(rows: &mut [SessionRow], totals: &[GatewayUsage]) {
    let mut by_id: HashMap<&str, &GatewayUsage> = HashMap::new();
    for t in totals {
        by_id.insert(t.session_id.as_str(), t);
    }
    for row in rows.iter_mut() {
        let key = format!("{GATEWAY_PREFIX}{}", row.summary.id);
        row.gateway = by_id.get(key.as_str()).map(|t| (*t).clone());
    }
}

/// Full parse of one session: exact summary plus the last `offset+limit`
/// timeline entries.
pub fn read_session(
    src: &ClaudeSessionsSource,
    id: &str,
    args: &DetailArgs,
) -> Result<SessionDetail> {
    let path = resolve_session_path(src, id)?;
    let encoded = path
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    read_detail(&path, &encoded, None).map(|d| paginate(d, args))
}

/// The subagent transcripts are the same format; this reads one of them.
pub fn read_subagent(
    src: &ClaudeSessionsSource,
    id: &str,
    agent_id: &str,
    args: &DetailArgs,
) -> Result<SessionDetail> {
    if !is_valid_agent_id(agent_id) {
        bail!("无效的子代理 ID");
    }
    let session_file = resolve_session_path(src, id)?;
    let dir = session_file.with_extension("").join("subagents");
    let dir_canon = dir
        .canonicalize()
        .with_context(|| format!("子代理目录不存在: {id}"))?;
    let file = dir_canon.join(format!("agent-{agent_id}.jsonl"));
    let file = file
        .canonicalize()
        .with_context(|| format!("子代理不存在: {agent_id}"))?;
    if !file.starts_with(&dir_canon) || !file.is_file() {
        bail!("子代理不存在: {agent_id}");
    }
    let encoded = session_file
        .parent()
        .and_then(|p| p.file_name())
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    read_detail(&file, &encoded, Some(agent_id)).map(|d| paginate(d, args))
}

fn read_detail(
    path: &Path,
    encoded_project: &str,
    agent_id: Option<&str>,
) -> Result<SessionDetail> {
    let meta = fs::metadata(path)?;
    // Keep every entry; `paginate` slices the response and the peak is a tidy
    // per-entry Vec on a blocking thread, not the whole response body.
    let mut acc = Accum::new(usize::MAX);
    let file = fs::File::open(path)?;
    read_lines(BufReader::new(file), |l| acc.absorb_line(l, true))?;
    let mut acc = acc.finish();

    let mut summary = acc.summary.clone();
    summary.id = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    summary.encoded_project = encoded_project.to_string();
    summary.file_mtime_ms = file_mtime_ms(&meta);
    summary.size_bytes = meta.len();
    summary.subagents = count_subagents(path);
    summary.counts_partial = false;

    let mut tokens_incl = acc.tokens.clone();
    tokens_incl.add(&acc.sidechain_tokens);

    let timeline: Vec<TimelineEntry> = acc.timeline.drain(..).collect();
    let timeline_total = acc.timeline_total;

    Ok(SessionDetail {
        summary,
        cost_state: acc.cost_state.take(),
        models: meta_from_accum(&acc),
        tokens: acc.tokens.clone(),
        tokens_incl_subagents: tokens_incl,
        timeline,
        timeline_offset: 0,
        timeline_total,
        has_more: false,
        ignored: acc.ignored.clone(),
        subagents: if agent_id.is_none() {
            list_subagents(path)
        } else {
            Vec::new()
        },
        agent_id: agent_id.map(str::to_string),
        gateway: None,
        gateway_error: None,
    })
}

/// Slice the timeline to `offset`/`limit` (counted from the end: offset 0 is
/// the newest page), keeping the response bounded for huge sessions.
fn paginate(mut detail: SessionDetail, args: &DetailArgs) -> SessionDetail {
    let limit = args
        .limit
        .unwrap_or(DETAIL_LIMIT)
        .clamp(1, DETAIL_MAX_LIMIT);
    let offset = args.offset.unwrap_or(0);
    let total = detail.timeline.len();
    let end = total.saturating_sub(offset);
    let start = end.saturating_sub(limit);
    detail.timeline = detail.timeline[start..end].to_vec();
    detail.timeline_offset = offset;
    detail.timeline_total = total;
    detail.has_more = start > 0;
    detail
}

fn list_subagents(session_file: &Path) -> Vec<SubagentMeta> {
    let dir = session_file.with_extension("").join("subagents");
    let Ok(entries) = fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let Some(agent_id) = entry.file_name().to_str().and_then(agent_id_from_file_name) else {
            continue;
        };
        let meta_path = entry.path().with_extension("meta.json");
        let (agent_type, description, name) = match fs::read_to_string(&meta_path)
            .ok()
            .and_then(|s| serde_json::from_str::<serde_json::Value>(&s).ok())
        {
            Some(v) => (
                str_of(&v, "agentType").map(str::to_string),
                str_of(&v, "description").map(str::to_string),
                str_of(&v, "name").map(str::to_string),
            ),
            None => (None, None, None),
        };
        out.push(SubagentMeta {
            agent_id,
            agent_type,
            description,
            name,
        });
    }
    out.sort_by(|a, b| a.agent_id.cmp(&b.agent_id));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("proxy-rs-claude-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn src(&self) -> ClaudeSessionsSource {
            ClaudeSessionsSource::new(&self.root)
        }

        fn write(&self, project: &str, file: &str, lines: &[String]) -> PathBuf {
            let dir = self.root.join(project);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let mut body = lines.join("\n");
            if !body.is_empty() {
                body.push('\n');
            }
            fs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const SID: &str = "b9c3800a-311e-4c89-9a38-c533d8969b5d";
    const PROJ: &str = "-Users-denis-Sites-proxy-rs";

    fn user(uuid: &str, ts: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user", "uuid": uuid, "parentUuid": null,
            "timestamp": ts, "cwd": "/Users/denis/Sites/proxy-rs", "version": "2.1.277",
            "gitBranch": "main",
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    fn tool_result_user(uuid: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "user", "uuid": uuid, "timestamp": ts,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
            ]}
        })
        .to_string()
    }

    fn assistant(uuid: &str, ts: &str, msg_id: &str, block: serde_json::Value) -> String {
        serde_json::json!({
            "type": "assistant", "uuid": uuid, "timestamp": ts,
            "message": {
                "id": msg_id, "role": "assistant", "model": "deepseek-v4.1-flash",
                "usage": {"input_tokens": 100, "output_tokens": 50,
                          "cache_read_input_tokens": 10, "cache_creation_input_tokens": 5},
                "content": [block]
            }
        })
        .to_string()
    }

    fn title(t: &str) -> String {
        serde_json::json!({"type": "ai-title", "aiTitle": t, "sessionId": SID}).to_string()
    }

    #[test]
    fn list_finds_uuid_files_only() {
        let f = Fixture::new("list-files");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hi")],
        );
        f.write(
            PROJ,
            "not-a-session.jsonl",
            &[user("x", "2026-11-01T10:00:00.000Z", "x")],
        );
        f.write(PROJ, "memory/MEMORY.md", &[]);
        let page = list_sessions(&f.src(), &ListArgs::default()).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.sessions[0].summary.id, SID);
    }

    #[test]
    fn cwd_comes_from_the_line_not_the_directory_name() {
        let f = Fixture::new("cwd");
        f.write(
            "-Users-a-b-c",
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hi")],
        );
        let page = list_sessions(&f.src(), &ListArgs::default()).unwrap();
        // The transcript says /Users/denis/Sites/proxy-rs, which the dir name cannot encode.
        assert_eq!(page.sessions[0].summary.cwd, "/Users/denis/Sites/proxy-rs");
    }

    #[test]
    fn title_is_the_last_ai_title_and_falls_back_to_first_prompt() {
        let f = Fixture::new("title");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[
                title("first"),
                user("u1", "2026-11-01T10:00:00.000Z", "the real question"),
                title("second"),
                title("third"),
            ],
        );
        let page = list_sessions(&f.src(), &ListArgs::default()).unwrap();
        assert_eq!(page.sessions[0].summary.title, "third");

        let f2 = Fixture::new("title-fallback");
        f2.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hello world")],
        );
        let page = list_sessions(&f2.src(), &ListArgs::default()).unwrap();
        assert_eq!(page.sessions[0].summary.title, "hello world");
    }

    #[test]
    fn assistant_lines_sharing_message_id_count_tokens_once() {
        let f = Fixture::new("turn-dedupe");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[
                user("u1", "2026-11-01T10:00:00.000Z", "go"),
                assistant("a1", "2026-11-01T10:00:01.000Z", "msg_1", serde_json::json!({"type":"text","text":"部分一"})),
                assistant("a2", "2026-11-01T10:00:01.500Z", "msg_1", serde_json::json!({"type":"tool_use","id":"toolu_1","name":"Bash","input":{"cmd":"ls"}})),
            ],
        );
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.assistant_turns, 1);
        assert_eq!(d.tokens.output, 50);
        assert_eq!(d.tokens.input, 100);
        assert_eq!(d.summary.tool_calls, 1);
        // Usage appears on exactly one entry.
        assert_eq!(d.timeline.iter().filter(|e| e.tokens.is_some()).count(), 1);
    }

    #[test]
    fn tool_results_and_noise_are_not_turns() {
        let f = Fixture::new("noise");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[
                user(
                    "u0",
                    "2026-11-01T09:00:00.000Z",
                    "<system-reminder>ignore me</system-reminder>",
                ),
                user("u1", "2026-11-01T10:00:00.000Z", "real prompt"),
                tool_result_user("u2", "2026-11-01T10:00:05.000Z"),
            ],
        );
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.turns, 1);
        assert_eq!(d.ignored.user_noise, 1);
        assert!(d.timeline.iter().any(|e| e.kind == EntryKind::ToolResult));
    }

    #[test]
    fn duplicate_uuids_are_dropped() {
        let f = Fixture::new("dedupe");
        let base = vec![
            user("u1", "2026-11-01T10:00:00.000Z", "one"),
            user("u2", "2026-11-01T10:00:10.000Z", "two"),
        ];
        let mut replay = base.clone();
        replay.push(user("u1", "2026-11-01T10:00:00.000Z", "one"));
        f.write(PROJ, &format!("{SID}.jsonl"), &replay);
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.turns, 2);
        assert_eq!(d.ignored.duplicate_uuid, 1);
    }

    #[test]
    fn malformed_lines_do_not_abort_the_file() {
        let f = Fixture::new("malformed");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[
                "{".to_string(),
                "null".to_string(),
                "[]".to_string(),
                "\"str\"".to_string(),
                "{\"type\":\"brand-new\"}".to_string(),
                user("u1", "2026-11-01T10:00:00.000Z", "survivor"),
            ],
        );
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.turns, 1);
        // `{` is not JSON (malformed); `null` / `[]` / `"str"` are JSON but not
        // transcript lines, and `brand-new` is an unknown type — all `other`.
        assert_eq!(d.ignored.malformed, 1);
        assert_eq!(d.ignored.other, 4);
        assert_eq!(d.timeline.len(), 1);
    }

    #[test]
    fn stringified_message_and_usage_do_not_panic() {
        let f = Fixture::new("stringified");
        let line = serde_json::json!({
            "type": "assistant", "uuid": "a1", "timestamp": "2026-11-01T10:00:00.000Z",
            "message": {"id": "m", "model": "x", "usage": "{\"output_tokens\": 7}",
                        "content": "plain string content"}
        })
        .to_string();
        let user_line = serde_json::json!({
            "type": "user", "uuid": "u1", "timestamp": "2026-11-01T09:00:00.000Z",
            "message": "just a string"
        })
        .to_string();
        f.write(PROJ, &format!("{SID}.jsonl"), &[user_line, line]);
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.tokens.output, 7);
        assert!(d.timeline.iter().any(|e| e.text == "plain string content"));
    }

    #[test]
    fn cost_state_last_wins_and_model_usage_parsed() {
        let f = Fixture::new("cost");
        let cost = |usd: f64| {
            serde_json::json!({
                "type": "cost-state", "totalCostUSD": usd, "totalDuration": 1000,
                "startTime": 1792925722078i64,
                "modelUsage": {"hy3": {"inputTokens": 10, "outputTokens": 20}}
            })
            .to_string()
        };
        f.write(PROJ, &format!("{SID}.jsonl"), &[cost(1.0), cost(2.5)]);
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        let c = d.cost_state.expect("cost state");
        assert_eq!(c.total_cost_usd, 2.5);
        assert_eq!(c.models[0].tokens.output, 20);
    }

    #[test]
    fn head_tail_listing_uses_tail_for_title() {
        let f = Fixture::new("headtail");
        let big = "x".repeat(500);
        let mut lines: Vec<String> = Vec::new();
        lines.push(user("u0", "2026-11-01T10:00:00.000Z", "opening"));
        // Pad well past 2 * HEAD_TAIL_BYTES.
        for i in 0..1200 {
            lines.push(
                serde_json::json!({"type":"attachment","uuid":format!("x{i}"),"rendered":big})
                    .to_string(),
            );
        }
        lines.push(title("tail title"));
        lines.push(user("u_last", "2026-11-01T12:00:00.000Z", "closing"));
        let path = f.write(PROJ, &format!("{SID}.jsonl"), &lines);
        assert!(fs::metadata(&path).unwrap().len() > HEAD_TAIL_BYTES * 2);
        let page = list_sessions(&f.src(), &ListArgs::default()).unwrap();
        let s = &page.sessions[0].summary;
        assert_eq!(s.title, "tail title");
        assert!(s.counts_partial);
        assert_eq!(s.cwd, "/Users/denis/Sites/proxy-rs", "cwd from the head");
    }

    #[test]
    fn timeline_pagination_is_stable() {
        let f = Fixture::new("paging");
        let mut lines = Vec::new();
        for i in 0..10 {
            let ts = format!("2026-11-01T10:00:{i:02}.000Z");
            lines.push(user(&format!("u{i}"), &ts, &format!("prompt {i}")));
        }
        f.write(PROJ, &format!("{SID}.jsonl"), &lines);
        let newest = read_session(
            &f.src(),
            SID,
            &DetailArgs {
                offset: Some(0),
                limit: Some(3),
            },
        )
        .unwrap();
        assert_eq!(newest.timeline.len(), 3);
        assert_eq!(newest.timeline.last().unwrap().text, "prompt 9");
        assert!(newest.has_more);
        assert_eq!(newest.timeline_total, 10);
        let older = read_session(
            &f.src(),
            SID,
            &DetailArgs {
                offset: Some(3),
                limit: Some(3),
            },
        )
        .unwrap();
        assert_eq!(older.timeline.last().unwrap().text, "prompt 6");
        let last = read_session(
            &f.src(),
            SID,
            &DetailArgs {
                offset: Some(9),
                limit: Some(3),
            },
        )
        .unwrap();
        assert_eq!(last.timeline.len(), 1);
        assert!(!last.has_more);
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let f = Fixture::new("unsafe");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hi")],
        );
        for bad in [
            "../../etc/passwd",
            "..",
            "/etc/passwd",
            "x",
            &"a".repeat(100),
            "B9C3800A-311E-4C89-9A38-C533D8969B5D",
        ] {
            assert!(
                read_session(&f.src(), bad, &DetailArgs::default()).is_err(),
                "{bad} must be rejected"
            );
            assert!(resolve_session_path(&f.src(), bad).is_err());
        }
        // Valid but absent.
        assert!(read_session(
            &f.src(),
            "00000000-0000-4000-8000-000000000000",
            &DetailArgs::default()
        )
        .is_err());
    }

    #[test]
    fn subagent_read_path_is_confined() {
        let f = Fixture::new("subagent");
        let path = f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hi")],
        );
        let sub_dir = path.with_extension("").join("subagents");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(
            sub_dir.join("agent-abc123.jsonl"),
            user("s1", "2026-11-01T10:00:01.000Z", "sub"),
        )
        .unwrap();
        fs::write(
            sub_dir.join("agent-abc123.meta.json"),
            r#"{"agentType":"Explore","name":"scout"}"#,
        )
        .unwrap();

        let ok = read_subagent(&f.src(), SID, "abc123", &DetailArgs::default()).unwrap();
        assert_eq!(ok.summary.turns, 1);
        assert_eq!(ok.agent_id.as_deref(), Some("abc123"));

        let list = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(list.summary.subagents, 1);
        assert_eq!(list.subagents[0].agent_type.as_deref(), Some("Explore"));

        assert!(read_subagent(&f.src(), SID, "../x", &DetailArgs::default()).is_err());
        assert!(read_subagent(&f.src(), SID, "missing", &DetailArgs::default()).is_err());
    }

    #[test]
    fn non_utf8_bytes_become_replacement_chars() {
        let f = Fixture::new("nonutf8");
        let dir = f.root.join(PROJ);
        fs::create_dir_all(&dir).unwrap();
        let mut bytes = user("u1", "2026-11-01T10:00:00.000Z", "ok").into_bytes();
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"type\":\"user\",\"uuid\":\"u2\",\"text\":\"\xff\xfe\"}\n");
        fs::write(dir.join(format!("{SID}.jsonl")), bytes).unwrap();
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.turns, 1);
    }

    #[test]
    fn parse_iso_ms_handles_offsets_and_fractions() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.123456Z"), Some(123));
        assert_eq!(parse_iso_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(
            parse_iso_ms("2026-10-25T11:34:10.152Z"),
            parse_iso_ms("2026-10-25T11:34:10.152Z")
        );
        assert!(parse_iso_ms("garbage").is_none());
    }

    #[test]
    fn human_prompt_filters_machinery() {
        assert!(!human_prompt("<system-reminder>x</system-reminder>"));
        assert!(!human_prompt("<task-notification>"));
        assert!(!human_prompt("   "));
        assert!(human_prompt("规划会话管理"));
    }

    #[test]
    fn gateway_join_uses_the_claude_namespace() {
        use crate::stats::{RequestOutcome, StatsDb, TokenRecord, UpstreamTiming};
        let db = StatsDb::in_memory().unwrap();
        let sid = format!("claude:{SID}");
        let tokens = TokenRecord {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5,
        };
        db.record_request_log(RequestOutcome {
            model: "deepseek-v4.1-flash",
            client_model: "deepseek-v4.1-flash",
            route: "/v1/messages",
            tokens: &tokens,
            duration_ms: 10,
            timing: UpstreamTiming::default(),
            streamed: true,
            status: 200,
            error: None,
            session_id: &sid,
            parent_session_id: "",
            client: "claude",
            override_key: "",
            override_model: "",
            override_reason: "",
        })
        .unwrap();

        let mut rows = vec![
            SessionRow {
                summary: SessionSummary {
                    id: SID.to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
            SessionRow {
                summary: SessionSummary {
                    id: "00000000-0000-4000-8000-000000000000".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        let ids = gateway_ids(&rows);
        let totals = db.session_usage_totals_for(&ids).unwrap();
        attach_gateway(&mut rows, &totals);
        assert_eq!(rows[0].gateway.as_ref().unwrap().requests, 1);
        assert_eq!(rows[0].gateway.as_ref().unwrap().output_tokens, 50);
        assert!(rows[1].gateway.is_none(), "unknown id is absent, not zero");
    }
}
