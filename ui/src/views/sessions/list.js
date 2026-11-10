/**
 * 会话 sidebar: source segments, the session list (flat or grouped), and paging.
 *
 * Owns no state of its own beyond the selection marker — the selected id, the
 * page, the loaded rows, the active source and the collapsed-group set live in
 * `sessions/index.js`, which is also the only caller of `renderList`.
 */

import { $, Raw, html, raw } from '../../core/dom.js';
import {
  formatBytes,
  formatLocalDateTime,
  formatRelativeTime,
  formatTokens,
} from '../../lib/format.js';

/** The selected row, so a re-render (a refresh, a paging step) keeps it marked. */
let selectedId = null;

/**
 * Is this page being served by the gateway to a real browser?
 *
 * The same front-end runs in three homes (see `core/ipc.js`), and one of them
 * changes what a row's shortcut can mean: in the server console the page *is*
 * already in the browser, so "open in browser" would be a no-op dressed up as a
 * button. That console shows a copy-link action instead, and the flag is read at
 * render time rather than captured at module load because a scenario (and a
 * reload of the page) can swap the transport.
 */
export function isBrowserHostedConsole() {
  return Boolean(window.__PROXY_RS_SERVER__);
}

/**
 * Mark the active source segment.
 *
 * Claude Code is the only readable source today; DSH is rendered disabled with
 * its note as a `title` so the reason is one hover away instead of a dead
 * control. A segment becomes switchable only when the backend says it is
 * available *and* names its slug — the slug is what a request can be filtered by,
 * so a segment without one would be a button that cannot do anything.
 *
 * `active` is the filter currently in force; with one readable source, "no
 * filter" and "that source only" select the same rows, so the segment stays lit
 * either way rather than flickering off after the first click.
 */
export function renderSources(sources, { active = null } = {}) {
  const host = $('#sessions-sources');
  if (!host) return;
  host.innerHTML = (sources || [])
    .map((s) => {
      const slug = s.available ? s.slug || '' : '';
      const lit = Boolean(slug) && (active === null || active === slug);
      return html`
        <button type="button" class="btn-toggle ${lit ? 'active' : ''}"
                data-kind="${s.kind}" data-slug="${slug}" ${s.available ? '' : 'disabled'}
                title="${s.note || s.label}">${s.label}</button>`;
    })
    .join('');
}

function gatewayBadgeValue(row) {
  return row.gateway
    ? `<span class="sess-badge" title="网关记录 ${row.gateway.requests} 次请求">网关 ${row.gateway.requests}</span>`
    : '';
}

/**
 * The per-row shortcut, which is a *different action per console flavour*.
 *
 * Desktop: the console is a Tauri webview, so opening a session in the machine's
 * real browser is a genuinely new window — the backend does the OS call, so this
 * is only an arrow with a tooltip.
 *
 * Server: the console page already lives in that browser, so "open in browser"
 * is meaningless. The same corner offers a copy-link action instead, which is
 * the only thing an operator on that page could actually want.
 */
function rowActionMarkup(source, id) {
  if (isBrowserHostedConsole()) {
    return html`
      <button type="button" class="sess-row-open sess-row-copy" data-copy-id="${id}"
              data-copy-source="${source}"
              title="复制此会话的深链接"
              aria-label="复制此会话的深链接">
        <span class="sess-row-open-icon">⧉</span>
      </button>`;
  }
  return html`
    <button type="button" class="sess-row-open" data-open-id="${id}"
            data-open-source="${source}"
            title="在浏览器打开此会话"
            aria-label="在浏览器打开此会话">
      <span class="sess-row-open-icon">↗</span>
    </button>`;
}

/**
 * One session row.
 *
 * `nested` is only a class on the *wrapper*: the 60px row height and the title's
 * right padding are contracts the list is measured against, and both live on the
 * children. Indentation therefore has exactly one legal place — the wrapper.
 */
function rowMarkup(row, { nested = false } = {}) {
  // The backend flattens `SessionSummary` into the row (`#[serde(flatten)]`),
  // so the summary fields sit at the top level beside `source`/`gateway`.
  const s = row.summary || row;
  const approx = s.counts_partial
    ? `<span class="sess-approx" title="列表只读取文件首尾，打开会话查看精确值">≈</span>`
    : '';
  const when = formatLocalDateTime(s.last_seen_ms || s.file_mtime_ms);
  const cwd = s.cwd || s.encoded_project;
  // Nested snippets are marked `raw` so the one `html` pass below does not
  // escape their markup; every value from the transcript (title, cwd, …) is
  // still escaped by this `html` call.
  //
  // The wrapper exists because HTML forbids a `<button>` inside a `<button>`: a
  // row that is clickable *and* carries a per-row action cannot nest the two, so
  // the row and its shortcut are siblings and the wrapper is the positioning
  // context. Anything counting rows still counts `.sess-row`, which is unchanged.
  return new Raw(html`
    <div class="sess-row-wrap ${nested ? 'is-nested' : ''}">
      <button type="button" class="sess-row ${s.id === selectedId ? 'active' : ''}" data-id="${s.id}">
        <div class="sess-row-title">${s.title}</div>
        <div class="sess-row-path" title="${cwd}">${cwd}</div>
        <div class="sess-row-foot">
          <span>${when}</span>
          <span title="${s.size_bytes} 字节">${formatBytes(s.size_bytes)}</span>
          <span title="人工提示 ${s.turns} · 助手回合 ${s.assistant_turns} · 工具调用 ${s.tool_calls}">${approx}${s.turns} 轮 · ${formatTokens(s.assistant_turns)} 回复</span>
          ${raw(row.subagents ? html`<span>子代理 ${row.subagents}</span>` : '')}
          ${raw(gatewayBadgeValue(row))}
        </div>
      </button>
      ${raw(rowActionMarkup(row.source || 'claude', s.id))}
    </div>`);
}

/**
 * Join pre-rendered row snippets (each already a `Raw`) into one markup string.
 *
 * `Raw`s are unwrapped explicitly: `html` would stringify them as
 * `[object Object]`, and passing a plain array would escape their markup.
 */
function outer(rawSnippets) {
  return rawSnippets.map((r) => r.value).join('');
}

/**
 * One project group's header.
 *
 * Deliberately **not** `.sess-row`: the 60px row height and `data-id` are the
 * row's contract, and a header wearing the row class would both break the
 * sidebar's rhythm and make "one session = one row" untestable. It is a sibling
 * of the rows inside `.sess-rows`, and the whole header is the collapse control,
 * so the caret is decorative and the label never has to be hit precisely.
 *
 * The `count` / `last_seen_ms` / `size_bytes` line is exact for the whole group
 * (the backend reads them from `stat`, not from the ten rows it returned) — which
 * is the only reason a header can be honest about a group bigger than a page.
 */
function groupHeadMarkup(group, isCollapsed) {
  const key = group.key || '';
  const when = formatRelativeTime(group.last_seen_ms);
  const meta = [when, group.size_bytes ? formatBytes(group.size_bytes) : ''].filter(Boolean).join(' · ');
  return new Raw(html`
    <button type="button" class="sess-group-head" data-group-key="${key}"
            aria-expanded="${isCollapsed ? 'false' : 'true'}"
            title="${group.cwd || group.label || ''}">
      <span class="sess-group-line">
        <span class="sess-group-caret">${isCollapsed ? '▸' : '▾'}</span>
        <span class="sess-group-label">${group.label || key}</span>
        <span class="sess-group-count">${group.count}</span>
      </span>
      <span class="sess-group-meta">${meta}</span>
    </button>`);
}

/**
 * "Show the rest of this group".
 *
 * One row per truncated group, and it names both the hidden count and the group
 * size: `group.count - group.sessions.length` is what a click would add, and the
 * total is what the header already claims, so the two numbers must agree with it
 * instead of starting a second, quieter count.
 */
function groupMoreMarkup(group) {
  const hidden = group.count - (group.sessions || []).length;
  return new Raw(html`
    <button type="button" class="sess-group-more" data-group-key="${group.key || ''}"
            data-group-count="${group.count}">显示其余 ${hidden} 个（共 ${group.count} 个）</button>`);
}

/**
 * Paint one group: header, its rows, and the remainder control when truncated.
 *
 * The body is a wrapper element (not a fragment) because folding is a `hidden`
 * attribute and "show the rest" replaces exactly this group's rows in place.
 * `Raw` is returned rather than a string so the caller can join groups with the
 * same `outer` used for rows — a plain string joined that way would stringify as
 * `undefined`.
 */
function groupMarkup(group, collapsed) {
  const isCollapsed = collapsed instanceof Set && collapsed.has(group.key);
  const rows = (group.sessions || []).map((row) => rowMarkup(row, { nested: true }));
  return new Raw(outer([
    groupHeadMarkup(group, isCollapsed),
    new Raw(`<div class="sess-group-body" data-group-key="${group.key || ''}"${isCollapsed ? ' hidden' : ''}>`),
    ...rows,
    ...(group.more ? [groupMoreMarkup(group)] : []),
    new Raw('</div>'),
  ]));
}

/**
 * Paint the list.
 *
 * Two shapes, one function. `page.groups` means the backend grouped the rows by
 * project (3.5.6); no `groups` key at all means the flat 3.5.5 response, which is
 * still what a script gets when it sends no `group_by`. The fallback is not
 * politeness: the mock in `ui/tools/smoke` and a browser holding a cached bundle
 * across a rolling upgrade both hit it, and a blank sidebar in either case would
 * look like data loss.
 *
 * `empty` is the message for a page with no rows (no source, no sessions, or
 * an error) — the caller decides which, because it knows why. `collapsed` is the
 * caller's set of group keys the user has folded shut; it is read, never written,
 * because the caller owns that interaction (see `toggleGroup`'s handler).
 */
export function renderList(page, { selected, empty, collapsed } = {}) {
  const host = $('#sessions-rows');
  if (!host) return;
  if (selected !== undefined) selectedId = selected;

  const groups = Array.isArray(page?.groups) ? page.groups : null;
  const grouped = Boolean(groups);
  // `count == 0` groups are not noise the user asked for: this machine has 13
  // project directories of which 3 hold no session, and three empty headers
  // would push the real sessions off screen.
  const visible = grouped ? groups.filter((g) => Number(g.count) > 0) : [];
  const rows = grouped ? [] : page?.sessions || [];

  if (grouped) {
    host.innerHTML = visible.length
      ? outer(visible.map((g) => groupMarkup(g, collapsed)))
      : html`<div class="sess-empty">${empty || '暂无会话'}</div>`;
  } else {
    // Each row is one already-escaped `html` snippet, so the join is marked raw
    // (the same rule as the detail timeline).
    host.innerHTML = rows.length
      ? outer(rows.map((r) => rowMarkup(r)))
      : html`<div class="sess-empty">${empty || '暂无会话'}</div>`;
  }

  const info = $('#sessions-paging-info');
  if (info) {
    const total = page?.total || 0;
    if (grouped) {
      // Grouped mode has no page offset to report — every group carries its own
      // count, so the toolbar states the two totals a reader actually asks for.
      info.textContent = total ? `共 ${total} 个 · ${visible.length} 个项目` : '';
    } else {
      const shown = rows.length;
      const offset = page?.offset || 0;
      info.textContent = total
        ? `显示第 ${offset + 1}–${offset + shown} 个 · 共 ${total} 个`
        : '';
    }
  }

  // Two paging dimensions on one screen is the classic double-pager trap: in
  // grouped mode each group owns its own "show the rest", so the global pager is
  // hidden rather than left to page a list that is no longer the unit on screen.
  const pager = $('.sess-pager');
  if (pager) {
    if (grouped) pager.setAttribute('hidden', '');
    else pager.removeAttribute('hidden');
  }

  const prev = $('#btn-sessions-prev');
  const next = $('#btn-sessions-next');
  if (prev) prev.disabled = grouped || !page || page.offset <= 0;
  if (next) {
    const total = page?.total || 0;
    const offset = page?.offset || 0;
    const limit = page?.limit || 0;
    next.disabled = grouped || !page || offset + limit >= total;
  }
}

/** The row the user clicked, or null. */
export function rowFromEvent(event) {
  const btn = event.target.closest('.sess-row');
  return btn ? btn.dataset.id : null;
}

/**
 * The group header the user clicked, as `{ key, collapsedNow }`, or null.
 *
 * Returned with the *next* state rather than the current one so the caller does
 * not have to read `aria-expanded` back out of the DOM to know what it just
 * asked for — and so the two directions cannot disagree about which one it was.
 */
export function groupFromEvent(event) {
  const head = event.target.closest('.sess-group-head');
  if (!head) return null;
  const key = head.dataset.groupKey || '';
  if (!key) return null;
  return { key, collapsedNow: head.getAttribute('aria-expanded') === 'true' };
}

/** The "show the rest of this group" control the user clicked, or null. */
export function groupMoreFromEvent(event) {
  const btn = event.target.closest('.sess-group-more');
  if (!btn) return null;
  return { key: btn.dataset.groupKey || '', count: Number(btn.dataset.groupCount) || 0 };
}

/**
 * Fold or unfold one group in place.
 *
 * Deliberately a DOM edit rather than a re-render: the list can be hundreds of
 * rows tall and scrolled, and `innerHTML = …` would send the reader back to the
 * top every time they opened a group.
 */
export function setGroupCollapsed(key, isCollapsed) {
  for (const head of document.querySelectorAll('#sessions-rows .sess-group-head')) {
    if (head.dataset.groupKey !== key) continue;
    head.setAttribute('aria-expanded', isCollapsed ? 'false' : 'true');
    const caret = head.querySelector('.sess-group-caret');
    if (caret) caret.textContent = isCollapsed ? '▸' : '▾';
  }
  for (const body of document.querySelectorAll('#sessions-rows .sess-group-body')) {
    if (body.dataset.groupKey !== key) continue;
    if (isCollapsed) body.setAttribute('hidden', '');
    else body.removeAttribute('hidden');
  }
}

/**
 * Unfold the group holding `id`, if it is inside a folded one.
 *
 * A deep link (or a restored session) can point at a row the user folded away
 * earlier in the same page lifetime. The row is still in the DOM — folded groups
 * are hidden, not discarded — so this only has to lift the `hidden`, and the
 * `scrollIntoView` the caller does next then has somewhere to go.
 */
export function expandGroupOfRow(id) {
  const row = document.querySelector(`#sessions-rows .sess-row[data-id="${id}"]`);
  const body = row?.closest('.sess-group-body');
  if (!body || !body.hasAttribute('hidden')) return;
  setGroupCollapsed(body.dataset.groupKey || '', false);
}

/**
 * Append one group's remaining rows in place.
 *
 * Used by "显示其余 N 个": the response is the whole group, so the rows replace
 * the truncated set rather than being appended after it (the truncated rows are
 * the *first* N of the same ordering, so appending would duplicate them).
 */
export function expandGroupRows(group) {
  if (!group?.key) return;
  for (const body of document.querySelectorAll('#sessions-rows .sess-group-body')) {
    if (body.dataset.groupKey !== group.key) continue;
    body.innerHTML = outer((group.sessions || []).map((row) => rowMarkup(row, { nested: true })));
    return;
  }
}

/**
 * The per-row shortcut the user clicked, as `{ action, id, source }`, or null.
 *
 * Returned as a triple rather than just the id because the two flavours do
 * different things with it, and the caller must not have to re-inspect the class
 * list to know which one it got. `source` travels with the id: an id is only
 * unique *within* a source, so a shortcut that dropped it would open whatever the
 * default source happens to have under that name.
 */
export function rowActionFromEvent(event) {
  const btn = event.target.closest('.sess-row-open');
  if (!btn) return null;
  const source = btn.dataset.copySource || btn.dataset.openSource || 'claude';
  if (btn.dataset.copyId) return { action: 'copy', id: btn.dataset.copyId, source };
  if (btn.dataset.openId) return { action: 'open', id: btn.dataset.openId, source };
  return null;
}

/**
 * The deep link for a session on the console that is serving this page.
 *
 * Only used by the *copy* path — the desktop path never builds a URL, because
 * the backend owns the console port and the front-end must not guess it. On the
 * server the page's own origin plus `/admin/` is exactly the address the
 * operator is already reading, so it is the one link guaranteed to work for
 * whoever they send it to.
 *
 * The source is a segment, not a prefix on the id: the id's shape is per-source
 * (Codex ids are UUIDs too), so `source` and `id` have to stay separable for the
 * reader to validate either one.
 */
export function sessionDeepLink(source, id) {
  const slug = source || 'claude';
  return `${window.location.origin}/admin/#sessions/${slug}/${id}`;
}
