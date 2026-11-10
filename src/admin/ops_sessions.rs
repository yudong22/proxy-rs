//! Browsing local sessions, across every registered source.
//!
//! ## What this module is responsible for
//!
//! Exactly two things that the source layer cannot know about: the `stats.db` join
//! (which lives here because `AdminState` owns the database) and the JSON envelope
//! the console and scripts read.
//!
//! ## Why the wire is composed here and not by `serde`
//!
//! `ListPage` carries the fields the source layer computes, but the response also
//! has to carry `sources` (the registry's descriptors) and `gateway_error` (the
//! outcome of the join) — neither of which belongs to a page. It is built as one
//! `json!` object on purpose, and every field the page or the join produces has to
//! be spelled out here. That is a known sharp edge: adding a field to `ListPage`
//! and forgetting to add it below produces **no compile error and no failing
//! test**, just a field that never reaches the client. `page_json` is kept in one
//! piece and `sessions_json_keys_match_the_page` (in this module's tests) exists
//! to make that mistake loud.

use super::*;

/// The `sources` array, derived from the registry.
///
/// DSH appears here as `available: false` with a reason — registered as *known*
/// but not readable, so the console renders a greyed segment that explains itself
/// instead of a control that silently does nothing. The order is the registry
/// order, and it is asserted by `tests/admin_api.rs`.
fn session_sources() -> Value {
    let rows: Vec<Value> = crate::sessions::descriptors()
        .into_iter()
        .map(|d| {
            json!({
                "kind": d.kind,
                "slug": d.slug,
                "label": d.label,
                "available": d.available,
                "note": d.note,
            })
        })
        .collect();
    Value::Array(rows)
}

/// The payload for "this machine has no sessions at all".
///
/// Still lists the sources: a machine with no `~/.claude/projects` should show
/// the same segments as one with sessions, otherwise the page's shape changes
/// based on a directory that has nothing to do with the UI.
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
        "group_by": Value::Null,
        "groups": Value::Null,
    })
}

/// One filesystem pass over the page's transcripts, on a blocking thread.
///
/// `None` means "no registered source has any data on this machine", which is
/// reported as an explanatory payload rather than an error.
async fn read_session_page(
    args: claude_sessions::ListArgs,
) -> Result<Option<claude_sessions::ListPage>> {
    // Resolve the requested sources first: an unknown slug is a client error, and
    // resolving it inside `spawn_blocking` would turn it into an opaque task
    // failure.
    let sources = crate::sessions::resolve(args.source.as_deref())?;
    let any_root = sources.iter().any(|s| s.root().is_some());
    if !any_root {
        return Ok(None);
    }
    let page = tokio::task::spawn_blocking(move || crate::sessions::list_page(&sources, &args))
        .await
        .map_err(|e| anyhow!("会话读取异常终止: {e}"))?
        .map_err(|e| anyhow!("读取会话失败: {e}"))?;
    Ok(Some(page))
}

/// Gateway totals for the page's ids, on their own blocking thread.
///
/// An error is reported per page (`gateway_error`) rather than failing the whole
/// request: the local transcript is still worth showing when SQLite is locked or
/// not yet open.
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

/// The 会话 page: local conversations with gateway usage joined by
/// `<source>:<id>`.
pub async fn sessions_list(
    stats: Option<Arc<StatsDb>>,
    args: claude_sessions::ListArgs,
) -> Result<Value> {
    // An unknown source is answered here, before any I/O, so the 404 comes from
    // the same place the "no data" answer does.
    crate::sessions::resolve(args.source.as_deref())?;

    let Some(mut page) = read_session_page(args).await? else {
        return Ok(sessions_unavailable(
            "未找到本地会话目录：当前用户没有可浏览的会话记录（Claude Code 为 ~/.claude/projects）"
                .to_string(),
        ));
    };
    let ids = crate::sessions::page_gateway_ids(&page);
    let (stats_available, gateway_error, totals) = gateway_totals(stats, ids).await;
    crate::sessions::attach_gateway_to_page(&mut page, &totals);
    page.stats_available = stats_available;
    Ok(page_json(&page, gateway_error))
}

/// The response envelope.
///
/// One place, one shape. See the module notes for why every `ListPage` field has
/// to be listed explicitly and why that is checked by a test.
fn page_json(page: &claude_sessions::ListPage, gateway_error: Option<String>) -> Value {
    json!({
        "available": page.available,
        "reason": page.reason,
        "root": page.root,
        "sources": session_sources(),
        "stats_available": page.stats_available,
        "gateway_error": gateway_error,
        "total": page.total,
        "offset": page.offset,
        "limit": page.limit,
        "sessions": page.sessions,
        "group_by": page.group_by,
        "groups": page.groups,
    })
}

/// One session's full transcript (the newest page of entries) plus totals.
///
/// `source` is the slug the address/UI named; `None` means the default source,
/// which is what a bare `#sessions/<uuid>` link and every 3.5.5-era script mean.
pub async fn session_detail(
    stats: Option<Arc<StatsDb>>,
    source: Option<String>,
    id: String,
    args: claude_sessions::DetailArgs,
) -> Result<Value> {
    let src = resolve_source(source.as_deref())?;
    let sid = id.clone();
    let mut detail =
        tokio::task::spawn_blocking(move || crate::sessions::read_session(src, &sid, &args))
            .await
            .map_err(|e| anyhow!("会话读取异常终止: {e}"))?
            .map_err(|e| anyhow!("{e}"))?;
    let ids = vec![format!("{}{}", src.gateway_prefix(), detail.summary.id)];
    let (_, gateway_error, totals) = gateway_totals(stats, ids).await;
    detail.gateway = totals.into_iter().next();
    detail.gateway_error = gateway_error;
    Ok(serde_json::to_value(detail)?)
}

/// One subagent transcript under a session.
pub async fn session_subagent_detail(
    source: Option<String>,
    id: String,
    agent: String,
    args: claude_sessions::DetailArgs,
) -> Result<Value> {
    let src = resolve_source(source.as_deref())?;
    let detail = tokio::task::spawn_blocking(move || {
        crate::sessions::read_subagent(src, &id, &agent, &args)
    })
    .await
    .map_err(|e| anyhow!("会话读取异常终止: {e}"))?
    .map_err(|e| anyhow!("{e}"))?;
    Ok(serde_json::to_value(detail)?)
}

/// The source named by a request, or the default one.
fn resolve_source(slug: Option<&str>) -> Result<&'static dyn crate::sessions::SessionSource> {
    match slug {
        None => Ok(crate::sessions::default_source()),
        Some(s) => crate::sessions::by_slug(s),
    }
}

// ── Helps ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// The failure this module warns about: a field that exists on `ListPage`
    /// but is missing from `page_json` reaches no client, and nothing fails.
    ///
    /// The check is textual on purpose — `serde` is not involved in building this
    /// payload, so there is no type-level link to lean on. It compares the JSON
    /// keys `page_json` emits against the field names `ListPage` declares.
    #[test]
    fn sessions_json_keys_match_the_page() {
        let page = claude_sessions::ListPage::default();
        let json = page_json(&page, None);
        let keys: std::collections::BTreeSet<&str> = json
            .as_object()
            .expect("page_json builds an object")
            .keys()
            .map(String::as_str)
            .collect();

        let src = include_str!("../claude_sessions/types.rs");
        let body = src
            .split("pub struct ListPage {")
            .nth(1)
            .and_then(|s| s.split("\n}").next())
            .expect("ListPage must be declared in types.rs");
        let fields: std::collections::BTreeSet<&str> = body
            .lines()
            .filter_map(|l| {
                let t = l.trim();
                let t = t.strip_prefix("pub ")?;
                t.split(':').next().map(str::trim)
            })
            .collect();

        let missing: Vec<&&str> = fields.difference(&keys).collect();
        assert!(
            missing.is_empty(),
            "these ListPage fields never reach the wire because page_json omits them: {missing:?}"
        );
        // The envelope also carries fields that are not page state; they must not
        // be dropped by a future refactor either.
        for required in ["sources", "gateway_error"] {
            assert!(keys.contains(required), "page_json must emit `{required}`");
        }
    }

    #[test]
    fn sources_come_from_the_registry_with_dsh_still_disabled() {
        let sources = session_sources();
        let arr = sources.as_array().expect("array");
        assert_eq!(arr[0]["kind"], "claude_local");
        assert_eq!(arr[0]["slug"], "claude");
        assert_eq!(arr[0]["available"], true);
        assert_eq!(arr[1]["kind"], "dsh_local");
        assert_eq!(arr[1]["available"], false);
        assert!(arr[1]["note"].as_str().is_some_and(|n| !n.is_empty()));
    }

    #[test]
    fn an_unavailable_machine_still_lists_its_sources() {
        let v = sessions_unavailable("no dir".to_string());
        assert_eq!(v["available"], false);
        assert!(v["sources"].as_array().is_some_and(|a| a.len() >= 2));
    }
}
