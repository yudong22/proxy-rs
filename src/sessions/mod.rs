//! Which session sources exist, and how their transcripts group into projects.
//!
//! ## Why this module exists
//!
//! 3.5.5 had exactly one readable source (Claude Code) and said so in fourteen
//! unrelated places: the projects root, the id validator, the `claude:` gateway
//! prefix (twice — see `ops_sessions`), the `sources` literal, the REST paths, the
//! RPC names, and a Chinese-substring 404 rule. Adding DSH or Codex therefore
//! meant editing the *centre* of the reading code, which is why it kept not
//! happening.
//!
//! This module is that centre, reduced to three things:
//!
//! 1. [`SessionSource`] — what a readable source must be able to answer.
//! 2. [`registry`] — the readable sources (today: Claude Code only) and
//!    [`PLANNED`] — the ones we know about but cannot read yet (today: DSH).
//!    The `sources` array the UI renders is derived from both, so "DSH is greyed
//!    out with a reason" is a property of a table, not of a `json!` literal.
//! 3. [`plan_groups`] — grouping is source-agnostic, so it lives here rather than
//!    in any one source's parser.
//!
//! ## Why the Claude implementation stayed where it was
//!
//! `claude_sessions` is ~1,900 lines with 17 tests over real transcript shapes
//! (`ai-title`, `cost-state`, sidechains, head+tail windowing). Moving it would
//! be a rename with a large blast radius and no benefit: [`claude::ClaudeSource`]
//! is a thin adapter over it, and the parser it needs stays its own business.
//! That is also the point of the seam — a new source brings its own parser
//! instead of adding a branch to a shared one (DSH's event stream and Codex's
//! `session_meta` share nothing with Claude's line types).
//!
//! ## Two identifiers, two jobs
//!
//! `kind` (`claude_local`) is what the wire already ships and what the UI's
//! `data-kind` segments and the existing tests are pinned to; it does not change.
//! `slug` (`claude`) is the short, URL-safe name used in the address
//! (`#sessions/claude/<uuid>`), in `ListArgs.source`, and in the per-row `source`
//! field. Keeping them separate is what lets the address grow a segment without
//! changing a key that existing assertions read.

use anyhow::{bail, Result};
use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::claude_sessions::{Candidate, DetailArgs, SessionDetail, SessionRow, SessionSummary};

pub(crate) mod claude;

/// The Claude Code source's slug.
///
/// Used in three places that must agree: the address segment, `ListArgs.source`,
/// and the `stats.db` namespace prefix. It is also the value the *old* address
/// shape (`#sessions/<uuid>`) resolves to, because that is the only source those
/// links could ever have come from.
pub(crate) const CLAUDE_SLUG: &str = "claude";

/// What one readable source has to answer.
///
/// `pub(crate)`: this is an internal seam, not a public API. A source that cannot
/// satisfy it is not registered, and an unregistered source is not readable at
/// all — that is why there is no `available()` method here. Availability has one
/// source of truth (registry membership), so "registered but says it is
/// unavailable" cannot happen.
pub(crate) trait SessionSource: Send + Sync {
    /// Short, URL-safe, frozen once shipped. `[a-z][a-z0-9_]{0,15}`.
    fn slug(&self) -> &'static str;
    /// The wire's historical identifier, pinned by existing tests. Never changes.
    fn kind(&self) -> &'static str;
    fn label(&self) -> &'static str;
    /// Namespace prefix for joining `stats.db` rows (`claude:`).
    fn gateway_prefix(&self) -> &'static str;

    /// Ids this source will accept. **Called before any path is built**, and each
    /// implementation must keep that promise: for Claude it is one strict
    /// lowercase-hex UUID check that rejects `..`, separators, absolute paths and
    /// symlink tricks in a single step.
    fn is_valid_id(&self, id: &str) -> bool;

    /// Where this source reads from, or `None` when the machine has no data for
    /// it (the caller renders that as "no sessions", not as an error).
    fn root(&self) -> Option<PathBuf>;

    /// One synchronous directory pass. Callers wrap a whole request in a single
    /// `spawn_blocking`; a source must not spawn its own.
    fn scan(&self) -> Result<Vec<Candidate>>;

    /// Fold one scanned file into a list summary.
    fn summary(&self, c: &Candidate) -> Result<SessionSummary>;

    fn read(&self, id: &str, args: &DetailArgs) -> Result<SessionDetail>;

    /// Subagents are a Claude Code concept (`<id>/subagents/agent-*.jsonl`).
    /// Sources without it inherit this refusal rather than returning an empty
    /// detail that would read as "this session has no subagents".
    fn read_subagent(&self, _id: &str, _agent: &str, _args: &DetailArgs) -> Result<SessionDetail> {
        bail!("无效的子代理请求：该来源没有子代理概念")
    }
}

/// Sources we know about but cannot read yet.
///
/// They are *not* in [`registry`], because registering is what makes a source
/// readable. Keeping them here is what makes the disabled segment in the UI a
/// table entry with a reason instead of a hardcoded `"available": false`, so
///接入 DSH 的动作是「把一行从 PLANNED 挪进 registry」，不是「在 json! 里改字面量」.
pub(crate) struct PlannedSource {
    pub(crate) slug: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) label: &'static str,
    pub(crate) note: &'static str,
}

pub(crate) const PLANNED: &[PlannedSource] = &[PlannedSource {
    slug: "dsh",
    kind: "dsh_local",
    label: "DSH",
    note: "DSH 会话是 zstd 压缩的本地存档，暂不支持浏览",
}];

static CLAUDE: claude::ClaudeSource = claude::ClaudeSource;

/// The readable sources, in the order the UI shows them.
///
/// A slice, not a `HashMap`: order is user-visible (it is the segment order) and
/// is asserted by `tests/admin_api.rs`.
static REGISTRY: &[&dyn SessionSource] = &[&CLAUDE];

pub(crate) fn registry() -> &'static [&'static dyn SessionSource] {
    REGISTRY
}

/// Look one source up by slug, or explain why it is not readable.
///
/// The message starts with 「无效的」 on purpose: `admin_routes::observe` maps that
/// prefix to a 404, and a caller asking for a source that does not exist is a
/// client error, not a server one.
pub(crate) fn by_slug(slug: &str) -> Result<&'static dyn SessionSource> {
    registry()
        .iter()
        .find(|s| s.slug() == slug)
        .copied()
        .ok_or_else(|| {
            if PLANNED.iter().any(|p| p.slug == slug) {
                // Known but unreadable: say which one and why, because "无效的
                // 来源" alone would send someone hunting a typo that is not there.
                let p = PLANNED
                    .iter()
                    .find(|p| p.slug == slug)
                    .expect("checked above");
                anyhow::anyhow!("无效的会话来源: {} 暂不支持（{}）", slug, p.note)
            } else {
                anyhow::anyhow!("无效的会话来源: {slug}")
            }
        })
}

/// The source a bare id belongs to when the address carries no source segment.
pub(crate) fn default_source() -> &'static dyn SessionSource {
    registry()[0]
}

/// One entry of the wire's `sources` array, independent of transport.
pub(crate) struct SourceFacts {
    pub(crate) slug: &'static str,
    pub(crate) kind: &'static str,
    pub(crate) label: &'static str,
    pub(crate) available: bool,
    pub(crate) note: Option<&'static str>,
}

/// Readable sources first, then the planned-but-unreadable ones.
///
/// The order is load-bearing: `tests/admin_api.rs` asserts `sources[0]` is
/// `claude_local` and `sources[1]` is `dsh_local`, and the UI renders the array
/// verbatim as its segment buttons.
pub(crate) fn descriptors() -> Vec<SourceFacts> {
    let mut out: Vec<SourceFacts> = registry()
        .iter()
        .map(|s| SourceFacts {
            slug: s.slug(),
            kind: s.kind(),
            label: s.label(),
            available: true,
            note: None,
        })
        .collect();
    out.extend(PLANNED.iter().map(|p| SourceFacts {
        slug: p.slug,
        kind: p.kind,
        label: p.label,
        available: false,
        note: Some(p.note),
    }));
    out
}

/// The sources a request should read.
///
/// `None` means "every readable one"; an unknown slug is an error rather than an
/// empty list, because an empty list renders as "no sessions" and would hide the
/// typo.
pub(crate) fn resolve(slug: Option<&str>) -> Result<Vec<&'static dyn SessionSource>> {
    match slug {
        None => Ok(registry().to_vec()),
        Some(s) => Ok(vec![by_slug(s)?]),
    }
}

// ── Reading a page across sources ───────────────────────────────────────

/// The list page for one or more sources.
///
/// This is the only place that knows how to combine sources, and in 3.5.6 there
/// is exactly one, so it reads as a loop with a single iteration — deliberately.
/// The alternative (a special case for "only Claude") is what makes adding the
/// second source a refactor instead of a registration.
///
/// `sources` is ordered: the registry order is the order groups are emitted when
/// two sources produce a group with the same newest mtime.
pub(crate) fn list_page(
    sources: &[&dyn SessionSource],
    args: &crate::claude_sessions::ListArgs,
) -> Result<crate::claude_sessions::ListPage> {
    use crate::claude_sessions::{
        ListPage, SessionGroup, DEFAULT_GROUP_LIMIT, DEFAULT_LIMIT, GROUP_BY_PROJECT, MAX_LIMIT,
    };

    let limit = args.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
    let offset = args.offset.unwrap_or(0);

    let group_by = match args.group_by.as_deref() {
        None | Some("none") | Some("") => None,
        Some(GROUP_BY_PROJECT) => Some(GROUP_BY_PROJECT.to_string()),
        Some(other) => bail!("无效的分组方式: {other}"),
    };

    let mut all: Vec<Candidate> = Vec::new();
    let mut roots: Vec<String> = Vec::new();
    for s in sources {
        if let Some(root) = s.root() {
            roots.push(root.display().to_string());
        }
        all.extend(s.scan()?);
    }
    let root = roots.first().cloned().unwrap_or_default();

    if let Some(group_by) = group_by {
        let group_limit = args
            .group_limit
            .unwrap_or(DEFAULT_GROUP_LIMIT)
            .clamp(1, MAX_LIMIT);
        let plans = plan_groups(
            all,
            args.group.as_deref(),
            GroupSlice {
                offset,
                limit: group_limit,
            },
        );
        // `total` keeps its meaning in grouped mode: how many sessions the request
        // matched. Under a group filter that is the group's size, so the same
        // number keeps the same name instead of gaining a second meaning.
        let total: usize = plans.iter().map(|p| p.count).sum();
        let mut groups = Vec::with_capacity(plans.len());
        for plan in plans {
            groups.push(SessionGroup {
                key: plan.key,
                label: plan.label,
                cwd: plan.cwd,
                sources: plan.sources,
                count: plan.count,
                last_seen_ms: plan.last_seen_ms,
                size_bytes: plan.size_bytes,
                more: plan.more,
                sessions: read_rows(sources, &plan.chosen),
            });
        }
        return Ok(ListPage {
            available: true,
            reason: None,
            root,
            stats_available: false,
            total,
            offset,
            limit,
            sessions: Vec::new(),
            group_by: Some(group_by),
            groups: Some(groups),
        });
    }

    all.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.id.cmp(&a.id)));
    let total = all.len();
    let page: Vec<Candidate> = all.into_iter().skip(offset).take(limit).collect();
    Ok(ListPage {
        available: true,
        reason: None,
        root,
        stats_available: false,
        total,
        offset,
        limit,
        sessions: read_rows(sources, &page),
        group_by: None,
        groups: None,
    })
}

/// Open one batch of candidates, dispatching each to the source that found it.
///
/// A transcript that cannot be read is skipped with a debug log rather than
/// blanking the page: one corrupt file must not hide the other twenty.
fn read_rows(sources: &[&dyn SessionSource], cands: &[Candidate]) -> Vec<SessionRow> {
    let mut out = Vec::with_capacity(cands.len());
    for c in cands {
        let Some(source) = sources.iter().find(|s| s.slug() == c.source) else {
            tracing::debug!(source = %c.source, "candidate from an unregistered source");
            continue;
        };
        match source.summary(c) {
            Ok(summary) => out.push(SessionRow {
                summary,
                source: c.source.clone(),
                ..SessionRow::default()
            }),
            Err(err) => {
                tracing::debug!(path = %c.path.display(), error = %err, "skip unreadable session");
            }
        }
    }
    out
}

/// One session's detail, from the source the caller named.
pub(crate) fn read_session(
    source: &'static dyn SessionSource,
    id: &str,
    args: &DetailArgs,
) -> Result<SessionDetail> {
    // Ordered on purpose: the id check is the whole path-safety story for a
    // source, and it must run before anything builds a path out of the id.
    if !source.is_valid_id(id) {
        bail!("无效的会话 ID");
    }
    source.read(id, args)
}

/// One subagent transcript, from the source the caller named.
pub(crate) fn read_subagent(
    source: &'static dyn SessionSource,
    id: &str,
    agent: &str,
    args: &DetailArgs,
) -> Result<SessionDetail> {
    if !source.is_valid_id(id) {
        bail!("无效的会话 ID");
    }
    source.read_subagent(id, agent, args)
}

// ── Wire-level helpers: gateway namespaces and the join ─────────────────

/// The `stats.db` namespace for one source slug (`claude:`).
///
/// The prefix is a property of the source, so it comes from the registry rather
/// than a constant: a second source must not be able to join its rows against
/// Claude Code's namespace by accident.
pub(crate) fn prefix_of(source: &str) -> String {
    match by_slug(source) {
        Ok(s) => s.gateway_prefix().to_string(),
        // Only reachable for a row whose source was unregistered between listing
        // and joining. Keeping the historical prefix is the safe answer: it
        // produces "no gateway data" instead of joining against someone else's.
        Err(_) => crate::claude_sessions::GATEWAY_PREFIX.to_string(),
    }
}

/// The namespaced ids for a batch of rows, ready for
/// `StatsDb::session_usage_totals_for`.
pub(crate) fn gateway_ids(rows: &[SessionRow]) -> Vec<String> {
    rows.iter()
        .map(|r| format!("{}{}", prefix_of(&r.source), r.summary.id))
        .collect()
}

/// The same for a whole page, including every row inside its groups.
pub(crate) fn page_gateway_ids(page: &crate::claude_sessions::ListPage) -> Vec<String> {
    let mut rows: Vec<&SessionRow> = page.sessions.iter().collect();
    if let Some(groups) = &page.groups {
        for g in groups {
            rows.extend(g.sessions.iter());
        }
    }
    gateway_ids(&rows.into_iter().cloned().collect::<Vec<_>>())
}

/// Attach gateway totals by namespaced id.
///
/// An id with no row gets `None`, which the UI renders as 「网关未记录」 — that is
/// not the same claim as zero requests, and conflating them would make every
/// session that never went through this proxy look like it cost nothing.
pub(crate) fn attach_gateway(
    rows: &mut [SessionRow],
    totals: &[crate::claude_sessions::GatewayUsage],
) {
    use std::collections::HashMap;
    let mut by_id: HashMap<&str, _> = HashMap::new();
    for t in totals {
        by_id.insert(t.session_id.as_str(), t);
    }
    for row in rows.iter_mut() {
        let key = format!("{}{}", prefix_of(&row.source), row.summary.id);
        row.gateway = by_id.get(key.as_str()).map(|t| (*t).clone());
    }
}

/// The same, for every row in a page and in each of its groups.
pub(crate) fn attach_gateway_to_page(
    page: &mut crate::claude_sessions::ListPage,
    totals: &[crate::claude_sessions::GatewayUsage],
) {
    attach_gateway(&mut page.sessions, totals);
    if let Some(groups) = page.groups.as_mut() {
        for g in groups.iter_mut() {
            attach_gateway(&mut g.sessions, totals);
        }
    }
}

// ── Grouping ────────────────────────────────────────────────────────────

/// How many of a group's sessions to hand back, and from where.
///
/// One struct serves both paging modes the wire offers: "the newest N of every
/// group" (`offset: 0`) and "page N of one group" (`only` + a real offset), so
/// there is a single selection path instead of two that can disagree.
pub(crate) struct GroupSlice {
    pub(crate) offset: usize,
    pub(crate) limit: usize,
}

/// One group, selected and measured but not yet read.
///
/// `count` / `last_seen_ms` / `size_bytes` come from the `stat` the directory
/// scan already did, so they are exact for the whole group and cost nothing
/// extra. `chosen` is the part we actually open.
pub(crate) struct GroupPlan {
    pub(crate) key: String,
    pub(crate) label: String,
    pub(crate) cwd: String,
    pub(crate) sources: Vec<String>,
    pub(crate) count: usize,
    pub(crate) last_seen_ms: i64,
    pub(crate) size_bytes: u64,
    pub(crate) more: bool,
    pub(crate) chosen: Vec<Candidate>,
}

/// The identity of a group.
///
/// The source slug is part of the key because two sources can legitimately have a
/// project directory of the same name, and merging them is a decision nobody has
/// data for yet (see the module notes on `cwd`). Namespacing now means the wire
/// does not have to change when that decision is made.
pub(crate) fn group_key(c: &Candidate) -> String {
    format!("{}:{}", c.source, c.encoded_project)
}

/// Bucket candidates into project groups, newest group first.
///
/// Ordering is `last_seen_ms` descending with `key` ascending as the tiebreak, so
/// two groups that were touched in the same millisecond still come back in a
/// stable order (a list that reshuffles between refreshes looks like it is
/// lying).
pub(crate) fn plan_groups(
    all: Vec<Candidate>,
    only: Option<&str>,
    slice: GroupSlice,
) -> Vec<GroupPlan> {
    let mut buckets: BTreeMap<String, Vec<Candidate>> = BTreeMap::new();
    for c in all {
        let key = group_key(&c);
        if only.is_some_and(|want| want != key) {
            continue;
        }
        buckets.entry(key).or_default().push(c);
    }

    let mut plans: Vec<GroupPlan> = buckets
        .into_iter()
        .map(|(key, mut group)| {
            group.sort_by(|a, b| b.mtime.cmp(&a.mtime).then_with(|| b.id.cmp(&a.id)));
            let count = group.len();
            let last_seen_ms = group.first().map(|c| c.mtime).unwrap_or(0);
            let size_bytes = group.iter().map(|c| c.size_bytes).sum();
            let mut sources: Vec<String> = group.iter().map(|c| c.source.clone()).collect();
            sources.sort();
            sources.dedup();

            // The label comes from the newest transcript's own `cwd`, because the
            // directory name cannot be decoded back into a path (`-` is a legal
            // path segment). One head read per group, capped, and a miss is not
            // an error — it just means "no label available".
            let cwd = group
                .first()
                .and_then(|c| crate::claude_sessions::probe_cwd(&c.path))
                .unwrap_or_default();
            let label = group_label(&cwd, &group[0].encoded_project);

            let chosen: Vec<Candidate> = group
                .into_iter()
                .skip(slice.offset)
                .take(slice.limit)
                .collect();
            let more = count > slice.offset + chosen.len();

            GroupPlan {
                key,
                label,
                cwd,
                sources,
                count,
                last_seen_ms,
                size_bytes,
                more,
                chosen,
            }
        })
        .collect();

    plans.sort_by(|a, b| {
        b.last_seen_ms
            .cmp(&a.last_seen_ms)
            .then_with(|| a.key.cmp(&b.key))
    });
    plans
}

/// A group's display name: the last path segment, else the directory name.
///
/// The fallback is only ever a *name*, never a path — showing `-Users-a-b-c` as
/// if it were `/Users/a/b/c` would be inventing a path the transcript never
/// claimed.
pub(crate) fn group_label(cwd: &str, encoded_project: &str) -> String {
    if let Some(name) = std::path::Path::new(cwd)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| !n.is_empty())
    {
        return name.to_string();
    }
    encoded_project.trim_start_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::claude_sessions::DetailArgs;

    fn cand(source: &str, project: &str, id: &str, mtime: i64, size: u64) -> Candidate {
        Candidate {
            path: PathBuf::from(format!("/nonexistent/{project}/{id}.jsonl")),
            encoded_project: project.to_string(),
            mtime,
            size_bytes: size,
            id: id.to_string(),
            source: source.to_string(),
        }
    }

    /// The registry's contract, so a future source cannot quietly break the wire.
    #[test]
    fn registry_slugs_are_url_safe_and_disjoint_from_planned() {
        let mut seen = std::collections::HashSet::new();
        for s in registry() {
            let slug = s.slug();
            assert!(
                !slug.is_empty() && slug.len() <= 16,
                "slug length out of range: {slug}"
            );
            assert!(
                slug.starts_with(|c: char| c.is_ascii_lowercase())
                    && slug
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_'),
                "slug must match [a-z][a-z0-9_]* so it can be a URL segment: {slug}"
            );
            assert!(seen.insert(slug), "duplicate slug: {slug}");
            assert!(
                !PLANNED.iter().any(|p| p.slug == slug),
                "{slug} cannot be both readable and planned"
            );
        }
        let mut prefixes = std::collections::HashSet::new();
        for s in registry() {
            assert!(
                prefixes.insert(s.gateway_prefix()),
                "duplicate gateway prefix: {}",
                s.gateway_prefix()
            );
        }
    }

    #[test]
    fn descriptors_keep_the_order_the_ui_and_tests_expect() {
        let d = descriptors();
        assert_eq!(d.len(), registry().len() + PLANNED.len());
        assert_eq!(d[0].kind, "claude_local");
        assert!(d[0].available);
        assert_eq!(d[1].kind, "dsh_local");
        assert!(!d[1].available);
        assert!(d[1].note.is_some());
    }

    #[test]
    fn an_unknown_source_is_an_error_not_an_empty_list() {
        assert!(resolve(Some("nope")).is_err());
        // The message must carry the 404 prefix `observe` keys on. Matched rather
        // than `unwrap_err()` because the `Ok` side is a `&dyn` without `Debug`.
        let Err(err) = by_slug("nope") else {
            panic!("an unregistered slug must not resolve");
        };
        let err = err.to_string();
        assert!(err.starts_with("无效的"), "{err}");
        // A known-but-unreadable source explains itself instead of looking like a
        // typo hunt.
        let Err(err) = by_slug("dsh") else {
            panic!("a planned source must not resolve yet");
        };
        let err = err.to_string();
        assert!(err.starts_with("无效的") && err.contains("zstd"), "{err}");
        assert_eq!(resolve(None).unwrap().len(), registry().len());
    }

    #[test]
    fn groups_are_measured_from_stat_and_truncated_by_the_slice() {
        let all = vec![
            cand("claude", "proj-a", "a1", 100, 10),
            cand("claude", "proj-a", "a2", 300, 20),
            cand("claude", "proj-a", "a3", 200, 30),
            cand("claude", "proj-b", "b1", 400, 40),
        ];
        let plans = plan_groups(
            all.clone(),
            None,
            GroupSlice {
                offset: 0,
                limit: 2,
            },
        );
        // Order: the newest group first.
        assert_eq!(plans[0].key, "claude:proj-b");
        assert_eq!(plans[0].count, 1);
        assert!(!plans[0].more);
        assert_eq!(plans[1].key, "claude:proj-a");
        assert_eq!(plans[1].count, 3, "count is the whole group, not the slice");
        assert_eq!(plans[1].size_bytes, 60, "sum over the whole group");
        assert_eq!(plans[1].last_seen_ms, 300, "max mtime");
        assert_eq!(plans[1].chosen.len(), 2);
        assert_eq!(plans[1].chosen[0].id, "a2", "newest first inside the group");
        assert!(plans[1].more, "one session did not fit");

        // The second page of that one group.
        let page = plan_groups(
            all,
            Some("claude:proj-a"),
            GroupSlice {
                offset: 2,
                limit: 2,
            },
        );
        assert_eq!(page.len(), 1);
        assert_eq!(page[0].chosen.len(), 1);
        assert_eq!(page[0].chosen[0].id, "a1");
        assert!(!page[0].more, "offset+shown reaches count");
    }

    #[test]
    fn a_group_key_carries_the_source_so_two_sources_cannot_collide() {
        let a = cand("claude", "proj", "s1", 1, 1);
        let b = cand("codex", "proj", "s2", 2, 2);
        assert_ne!(group_key(&a), group_key(&b));
        let plans = plan_groups(
            vec![a, b],
            None,
            GroupSlice {
                offset: 0,
                limit: 10,
            },
        );
        assert_eq!(plans.len(), 2, "same directory name, different sources");
        // `sources` on the group is the deduped set, ascending.
        assert!(plans.iter().all(|p| p.sources.len() == 1));
    }

    #[test]
    fn a_group_label_prefers_the_real_path_and_falls_back_to_a_name_only() {
        assert_eq!(
            group_label("/Users/denis/Sites/proxy-rs", "-Users"),
            "proxy-rs"
        );
        // No cwd: the encoded directory name is a *name*, stripped of its
        // leading separator. It is never turned back into a path.
        assert_eq!(
            group_label("", "-Users-denis-Sites-proxy-rs"),
            "Users-denis-Sites-proxy-rs"
        );
    }

    /// The seam, end to end: a synthetic source proves that grouping, the wire's
    /// source labels and the registry are genuinely source-agnostic. Without this
    /// the "extensible" claim would be untested until someone ships a real second
    /// source — i.e. exactly when it is most expensive to discover it was wrong.
    #[test]
    fn a_synthetic_source_flows_through_grouping_without_touching_claude_code() {
        struct FakeSource;
        impl SessionSource for FakeSource {
            fn slug(&self) -> &'static str {
                "fake"
            }
            fn kind(&self) -> &'static str {
                "fake_local"
            }
            fn label(&self) -> &'static str {
                "Fake"
            }
            fn gateway_prefix(&self) -> &'static str {
                "fake:"
            }
            fn is_valid_id(&self, id: &str) -> bool {
                id.starts_with("fake-")
            }
            fn root(&self) -> Option<PathBuf> {
                Some(PathBuf::from("/nonexistent/fake"))
            }
            fn scan(&self) -> Result<Vec<Candidate>> {
                Ok(vec![
                    cand("fake", "proj", "fake-1", 10, 5),
                    cand("fake", "proj", "fake-2", 20, 7),
                ])
            }
            fn summary(&self, c: &Candidate) -> Result<SessionSummary> {
                Ok(SessionSummary {
                    id: c.id.clone(),
                    ..SessionSummary::default()
                })
            }
            fn read(&self, _id: &str, _args: &DetailArgs) -> Result<SessionDetail> {
                bail!("fake source has no detail")
            }
        }

        let plans = plan_groups(
            FakeSource.scan().unwrap(),
            None,
            GroupSlice {
                offset: 0,
                limit: 10,
            },
        );
        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].count, 2);
        assert_eq!(plans[0].sources, vec!["fake".to_string()]);
        assert_eq!(plans[0].key, "fake:proj");
        // The default subagent message is the trait's, not a special case.
        assert!(FakeSource
            .read_subagent("fake-1", "x", &DetailArgs::default())
            .is_err());
    }
}
