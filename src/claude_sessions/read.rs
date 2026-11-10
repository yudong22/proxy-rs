//! The read entry points: listing, detail, subagents and the gateway usage join.

use super::*;

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
