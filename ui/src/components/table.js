/**
 * Request log table (the DB-backed view).
 *
 * Owns: the row markup, the four dropdown facets, the search box, paging, and
 * the fetch that feeds them. The console tail next door is a separate view.
 */

import { $, $$, setText, html, raw, debounce } from '../core/dom.js';
import { invoke } from '../core/ipc.js';
import { appState } from '../core/state.js';
import { shortSessionId, shortTime, formatDuration } from '../lib/format.js';
import { confirmAction } from './confirm.js';
import { toast } from './toast.js';
import { showRequestModal } from './request-modal.js';

/** Status pill for a 2xx/4xx/5xx (or failed) response. */
function statusBadge(item) {
  if (item.status >= 200 && item.status < 300) {
    return html`<span class="badge-pill status-pill status-2xx">${item.status} OK</span>`;
  }
  if (item.status >= 400 && item.status < 500) {
    return html`<span class="badge-pill status-pill status-4xx">${item.status}</span>`;
  }
  return html`<span class="badge-pill status-pill status-5xx">${item.status || 'ERR'}</span>`;
}

/** Session cell: client pill + shortened id, or an em dash when unknown. */
function sessionCell(item) {
  if (!item.session_id) {
    return html`<span class="cell-session-none">—</span>`;
  }
  const client = item.client || 'unknown';
  return html`<div class="cell-session-box">
    <span class="badge-pill client-pill client-${client}">${client}</span>
    <span class="cell-session" title="${item.session_id}">${shortSessionId(item.session_id, item.client)}</span>
  </div>`;
}

/**
 * One <tr>.
 *
 * `sessionCell` and `statusBadge` return *markup*, so they are interpolated
 * through `raw`: without it the `html` template would escape their tags and the
 * cell would print `<span class="…">` as text.
 */
function row(item) {
  const isErr = item.status >= 400 || Boolean(item.error);
  const input = item.input_tokens ? item.input_tokens.toLocaleString() : '0';
  const output = item.output_tokens ? item.output_tokens.toLocaleString() : '0';
  const cacheHint = item.cache_read_tokens > 0
    ? raw(`<span class="cell-cache-hint">cache: ${item.cache_read_tokens.toLocaleString()}</span>`)
    : '';

  return html`<tr class="${isErr ? 'row-error' : ''}">
    <td class="cell-time" title="${item.created_at}">${shortTime(item.created_at)}</td>
    <td>${raw(sessionCell(item))}</td>
    <td>
      <div class="cell-model-box">
        <span class="cell-model" title="${item.model}">${item.model}</span>
        <span class="cell-route">${item.route}</span>
      </div>
    </td>
    <td class="cell-num">${input}${cacheHint}</td>
    <td class="cell-num">${output}</td>
    <td class="cell-duration">${formatDuration(item.duration_ms)}</td>
    <td class="cell-center">${
      item.streamed
        ? raw('<span class="badge-pill stream-pill stream-true">流式</span>')
        : raw('<span class="badge-pill stream-pill stream-false">非流式</span>')
    }</td>
    <td class="cell-center">${raw(statusBadge(item))}</td>
    <td class="cell-center"><button type="button" class="btn-inspect" data-req-id="${item.id}">详情</button></td>
  </tr>`;
}

/** Repaint the table body and the pagination footer. */
function render() {
  const tbody = $('#request-logs-tbody');
  if (!tbody) return;

  const items = appState.requestLogs || [];
  tbody.innerHTML = items.length
    ? items.map(row).join('')
    : raw('<tr><td colspan="9" class="table-empty">未匹配到任何请求记录</td></tr>').value;

  for (const btn of $$('.btn-inspect')) {
    btn.addEventListener('click', (e) => {
      showRequestModal(parseInt(e.currentTarget.dataset.reqId, 10));
    });
  }

  const total = appState.requestLogsTotal;
  const pageSize = appState.requestLogsPageSize;
  const totalPages = Math.max(1, Math.ceil(total / pageSize));
  const currentPage = appState.requestLogsPage + 1;

  setText('table-log-count', `${total} 条记录`);
  setText('pagination-info', `第 ${currentPage} / ${totalPages} 页 · 共 ${total} 条`);

  const prev = $('#btn-prev-page');
  const next = $('#btn-next-page');
  if (prev) prev.disabled = currentPage <= 1;
  if (next) next.disabled = currentPage >= totalPages;
}

/** Rebuild the model dropdown, keeping the current selection when it survives. */
function updateModelDropdown() {
  const select = $('#table-model-filter');
  if (!select) return;
  const current = select.value;
  const models = appState.requestLogsModels || [];

  select.innerHTML = html`<option value="all">全部模型</option>`
    + models.map(m => html`<option value="${m}">${m}</option>`).join('');

  select.value = current && Array.from(select.options).some(o => o.value === current)
    ? current
    : 'all';
}

/**
 * Rebuild the session dropdown.
 *
 * Values are the exact `session_id`; a `<client>:` entry (trailing colon)
 * filters by client instead, so "all Codex requests" is one click. Built from
 * what is actually in the DB so the list stays short — preferring the DB-wide
 * lists so a conversation whose latest request sits on an older page is still
 * selectable, and falling back to the visible page for older backends.
 */
function updateSessionDropdown() {
  const select = $('#table-client-filter');
  if (!select) return;
  const current = select.value;

  const clients = appState.requestLogsClients || [];
  const fromDb = appState.requestLogsSessions || [];
  const sessions = fromDb.length
    ? fromDb
    : (appState.requestLogs || [])
        .filter(r => r.session_id)
        .map(r => ({ session_id: r.session_id, client: r.client }))
        .filter((s, i, all) => all.findIndex(o => o.session_id === s.session_id) === i);

  const seenClients = clients.length
    ? clients
    : [...new Set(sessions.map(s => s.client).filter(Boolean))];

  select.innerHTML = html`<option value="all">全部会话</option>`
    + seenClients.map(c => html`<option value="${c}:">仅 ${c}</option>`).join('')
    + sessions.map(s => html`<option value="${s.session_id}">${s.client ? `${s.client} · ${s.session_id}` : s.session_id}</option>`).join('');

  select.value = current && Array.from(select.options).some(o => o.value === current)
    ? current
    : 'all';
}

/**
 * Load one page of request logs.
 * @param {boolean} [resetPage] jump back to page 1 (a filter changed).
 */
export async function fetchRequestLogs(resetPage = false) {
  if (resetPage) appState.requestLogsPage = 0;

  const model = $('#table-model-filter')?.value || 'all';
  const status = $('#table-status-filter')?.value || 'all';
  const streamed = $('#table-streamed-filter')?.value || 'all';
  const client = $('#table-client-filter')?.value || 'all';
  const search = $('#table-search-input')?.value.trim() || '';

  const filter = {
    limit: appState.requestLogsPageSize,
    offset: appState.requestLogsPage * appState.requestLogsPageSize,
  };
  if (model !== 'all') filter.model = model;
  if (status !== 'all') filter.status_group = status;
  if (streamed === 'stream') filter.streamed = true;
  else if (streamed === 'sync') filter.streamed = false;
  if (client !== 'all') {
    // 'all:' is the "every session of this client" option.
    if (client.endsWith(':')) filter.client = client.slice(0, -1);
    else filter.session_id = client;
  }
  if (search) filter.search = search;

  try {
    const res = await invoke('get_request_logs', { filter });
    if (!res) return;
    appState.requestLogs = res.items || [];
    appState.requestLogsTotal = res.total || 0;
    appState.requestLogsModels = res.models || [];
    appState.requestLogsClients = res.clients || [];
    appState.requestLogsSessions = res.sessions || [];
    updateModelDropdown();
    updateSessionDropdown();
    render();
  } catch (err) {
    console.error('fetchRequestLogs error:', err);
  }
}

/** Wire the toolbar, filters and pager. Called once, from the logs view. */
export function initRequestTable() {
  const onFilterChange = () => fetchRequestLogs(true);

  // Debounced: typing in the search box should not re-query per keystroke.
  $('#table-search-input')?.addEventListener('input', debounce(() => fetchRequestLogs(true), 250));
  $('#table-model-filter')?.addEventListener('change', onFilterChange);
  $('#table-status-filter')?.addEventListener('change', onFilterChange);
  $('#table-streamed-filter')?.addEventListener('change', onFilterChange);
  $('#table-client-filter')?.addEventListener('change', onFilterChange);

  $('#btn-clear-db-logs')?.addEventListener('click', async () => {
    const ok = await confirmAction(
      '确定要清空数据库中的所有请求记录吗？此操作不可恢复。',
      '清空请求记录',
    );
    if (!ok) return;
    try {
      await invoke('clear_request_logs');
      await fetchRequestLogs(true);
      toast('请求记录已清空');
    } catch (e) {
      toast('清空失败: ' + e, 'error');
    }
  });

  $('#btn-prev-page')?.addEventListener('click', () => {
    if (appState.requestLogsPage > 0) {
      appState.requestLogsPage--;
      fetchRequestLogs(false);
    }
  });

  $('#btn-next-page')?.addEventListener('click', () => {
    const maxPage = Math.ceil(appState.requestLogsTotal / appState.requestLogsPageSize) - 1;
    if (appState.requestLogsPage < maxPage) {
      appState.requestLogsPage++;
      fetchRequestLogs(false);
    }
  });
}
