/**
 * 会话 sidebar: source segments, the session list, and paging.
 *
 * Owns no state of its own beyond a render of the rows it is handed — the
 * selected id, the page and the loaded rows live in `sessions/index.js`, which
 * is also the only caller of `renderList`.
 */

import { $, Raw, html, raw } from '../../core/dom.js';
import {
  formatBytes,
  formatLocalDateTime,
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
 * Only Claude Code is readable today; DSH is rendered disabled with its note
 * as a `title` so the reason is one hover away instead of a dead control.
 */
export function renderSources(sources) {
  const host = $('#sessions-sources');
  if (!host) return;
  host.innerHTML = (sources || [])
    .map(
      (s) => html`
        <button type="button" class="btn-toggle ${s.available ? 'active' : ''}"
                data-kind="${s.kind}" ${s.available ? '' : 'disabled'}
                title="${s.note || s.label}">${s.label}</button>`,
    )
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
function rowActionMarkup(id) {
  if (isBrowserHostedConsole()) {
    return html`
      <button type="button" class="sess-row-open sess-row-copy" data-copy-id="${id}"
              title="复制此会话的深链接"
              aria-label="复制此会话的深链接">
        <span class="sess-row-open-icon">⧉</span>
      </button>`;
  }
  return html`
    <button type="button" class="sess-row-open" data-open-id="${id}"
            title="在浏览器打开此会话"
            aria-label="在浏览器打开此会话">
      <span class="sess-row-open-icon">↗</span>
    </button>`;
}

function rowMarkup(row) {
  // The backend flattens `SessionSummary` into the row (`#[serde(flatten)]`),
  // so the summary fields sit at the top level beside `gateway`.
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
    <div class="sess-row-wrap">
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
      ${raw(rowActionMarkup(s.id))}
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
 * Paint the list.
 *
 * `empty` is the message for a page with no rows (no source, no sessions, or
 * an error) — the caller decides which, because it knows why.
 */
export function renderList(page, { selected, empty } = {}) {
  const host = $('#sessions-rows');
  if (!host) return;
  if (selected !== undefined) selectedId = selected;
  const rows = page?.sessions || [];
  // Each row is one already-escaped `html` snippet, so the join is marked raw
  // (the same rule as the detail timeline).
  host.innerHTML = rows.length
    ? outer(rows.map(rowMarkup))
    : html`<div class="sess-empty">${empty || '暂无会话'}</div>`;

  const info = $('#sessions-paging-info');
  if (info) {
    const total = page?.total || 0;
    const shown = rows.length;
    const offset = page?.offset || 0;
    info.textContent = total
      ? `显示第 ${offset + 1}–${offset + shown} 个 · 共 ${total} 个`
      : '';
  }
  const prev = $('#btn-sessions-prev');
  const next = $('#btn-sessions-next');
  if (prev) prev.disabled = !page || page.offset <= 0;
  if (next) {
    const total = page?.total || 0;
    const offset = page?.offset || 0;
    const limit = page?.limit || 0;
    next.disabled = !page || offset + limit >= total;
  }
}

/** The row the user clicked, or null. */
export function rowFromEvent(event) {
  const btn = event.target.closest('.sess-row');
  return btn ? btn.dataset.id : null;
}

/**
 * The per-row shortcut the user clicked, as `{ action, id }`, or null.
 *
 * Returned as a pair rather than just the id because the two flavours do
 * different things with it, and the caller must not have to re-inspect the class
 * list to know which one it got.
 */
export function rowActionFromEvent(event) {
  const btn = event.target.closest('.sess-row-open');
  if (!btn) return null;
  if (btn.dataset.copyId) return { action: 'copy', id: btn.dataset.copyId };
  if (btn.dataset.openId) return { action: 'open', id: btn.dataset.openId };
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
 */
export function sessionDeepLink(id) {
  return `${window.location.origin}/admin/#sessions/${id}`;
}

