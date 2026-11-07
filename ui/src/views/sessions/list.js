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
  return new Raw(html`
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
    </button>`);
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

