//! Browsing local Claude Code sessions.

use super::*;

/// A local-session source descriptor the console renders as a source segment.
///
/// DSH is listed even though it is not readable yet: an absent control reads
/// as a missing feature, a disabled one with a reason reads as a known limit.
fn session_sources() -> Value {
    json!([
        { "kind": "claude_local", "label": "Claude Code", "available": true, "note": Value::Null },
        {
            "kind": "dsh_local",
            "label": "DSH",
            "available": false,
            "note": "DSH 会话是 zstd 压缩的本地存档，暂不支持浏览"
        }
    ])
}

fn sessions_unavailable(reason: String) -> Value {
    json!({
        "available": false,
        "reason": reason,
        "root": Value::Null,
        "sources": session_sources(),
        "stats_available": false,
        "total": 0,
        "offset": 0,
        "limit": 0,
        "sessions": [],
    })
}

/// One filesystem pass over the page's transcripts, on a blocking thread.
async fn read_session_page(
    args: claude_sessions::ListArgs,
) -> Result<Option<claude_sessions::ListPage>> {
    let Some(src) = claude_sessions::ClaudeSessionsSource::from_env() else {
        return Ok(None);
    };
    let page = tokio::task::spawn_blocking(move || claude_sessions::list_sessions(&src, &args))
        .await
        .map_err(|e| anyhow!("会话读取异常终止: {e}"))?
        .map_err(|e| anyhow!("读取会话失败: {e}"))?;
    Ok(Some(page))
}

/// Gateway totals for the page's ids, on their own blocking thread.
///
/// An error is reported per page (`gateway_error`) rather than failing the
/// whole request: the local transcript is still worth showing when SQLite is
/// locked or not yet open.
async fn gateway_totals(
    stats: Option<Arc<StatsDb>>,
    ids: Vec<String>,
) -> (bool, Option<String>, Vec<claude_sessions::GatewayUsage>) {
    let Some(stats) = stats else {
        return (false, None, Vec::new());
    };
    if ids.is_empty() {
        return (true, None, Vec::new());
    }
    match tokio::task::spawn_blocking(move || stats.session_usage_totals_for(&ids)).await {
        Ok(Ok(totals)) => (true, None, totals),
        Ok(Err(e)) => (true, Some(format!("网关用量查询失败: {e}")), Vec::new()),
        Err(e) => (true, Some(format!("网关用量查询异常终止: {e}")), Vec::new()),
    }
}

/// The 会话 page: local Claude Code conversations with gateway usage joined
/// by `claude:<uuid>`.
pub async fn claude_sessions_list(
    stats: Option<Arc<StatsDb>>,
    args: claude_sessions::ListArgs,
) -> Result<Value> {
    let Some(page) = read_session_page(args).await? else {
        return Ok(sessions_unavailable(
            "未找到 ~/.claude/projects：当前用户没有 Claude Code 本地会话记录".to_string(),
        ));
    };
    let ids = claude_sessions::gateway_ids(&page.sessions);
    let (stats_available, gateway_error, totals) = gateway_totals(stats, ids).await;
    let mut rows = page.sessions;
    claude_sessions::attach_gateway(&mut rows, &totals);
    Ok(json!({
        "available": true,
        "reason": Value::Null,
        "root": page.root,
        "sources": session_sources(),
        "stats_available": stats_available,
        "gateway_error": gateway_error,
        "total": page.total,
        "offset": page.offset,
        "limit": page.limit,
        "sessions": rows,
    }))
}

/// One session's full transcript (the newest page of entries) plus totals.
pub async fn claude_session_detail(
    stats: Option<Arc<StatsDb>>,
    id: String,
    args: claude_sessions::DetailArgs,
) -> Result<Value> {
    let src = claude_sessions::ClaudeSessionsSource::from_env()
        .ok_or_else(|| anyhow!("未找到 ~/.claude/projects"))?;
    let sid = id.clone();
    let mut detail =
        tokio::task::spawn_blocking(move || claude_sessions::read_session(&src, &sid, &args))
            .await
            .map_err(|e| anyhow!("会话读取异常终止: {e}"))?
            .map_err(|e| anyhow!("{e}"))?;
    let ids = vec![format!("claude:{}", detail.summary.id)];
    let (_, gateway_error, totals) = gateway_totals(stats, ids).await;
    detail.gateway = totals.into_iter().next();
    detail.gateway_error = gateway_error;
    Ok(serde_json::to_value(detail)?)
}

/// One subagent transcript under a session.
pub async fn claude_subagent_detail(
    id: String,
    agent: String,
    args: claude_sessions::DetailArgs,
) -> Result<Value> {
    let src = claude_sessions::ClaudeSessionsSource::from_env()
        .ok_or_else(|| anyhow!("未找到 ~/.claude/projects"))?;
    let detail = tokio::task::spawn_blocking(move || {
        claude_sessions::read_subagent(&src, &id, &agent, &args)
    })
    .await
    .map_err(|e| anyhow!("会话读取异常终止: {e}"))?
    .map_err(|e| anyhow!("{e}"))?;
    Ok(serde_json::to_value(detail)?)
}

// ── Helpers ────────────────────────────────────────────────────────────────
