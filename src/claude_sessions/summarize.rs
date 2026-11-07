//! The single-pass accumulator that folds one transcript into a summary.

use super::*;

/// Everything derived from a transcript while streaming its lines.
///
/// One pass feeds both the list (summary only) and the detail (summary + the
/// last `keep` timeline entries, bounded so a 10 MB file does not become a
/// 10 MB response).
pub(crate) struct Accum {
    pub(crate) summary: SessionSummary,
    seen_uuids: HashSet<String>,
    seen_turns: HashSet<(String, String)>,
    models: HashMap<String, ModelUsage>,
    pub(crate) tokens: TokenTotals,
    pub(crate) sidechain_tokens: TokenTotals,
    pub(crate) cost_state: Option<SessionCostState>,
    first_prompt: Option<String>,
    pub(crate) ignored: IgnoreCounts,
    pub(crate) timeline: VecDeque<TimelineEntry>,
    kept: usize,
    pub(crate) timeline_total: usize,
}

impl Accum {
    pub(crate) fn new(keep: usize) -> Self {
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
    pub(crate) fn absorb_line(&mut self, line: &str, keep_entries: bool) {
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

    pub(crate) fn finish(mut self) -> Self {
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

pub(crate) fn meta_from_accum(acc: &Accum) -> Vec<ModelUsage> {
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
pub(crate) fn summary_from_file(path: &Path, encoded_project: &str) -> Result<SessionSummary> {
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

pub(crate) fn count_subagents(session_file: &Path) -> u32 {
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
