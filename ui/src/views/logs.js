/**
 * Logs view.
 *
 * Two views over different sources, sharing one toolbar:
 *   table   — request rows from the SQLite stats DB (components/table.js)
 *   console — the raw in-memory log tail from `get_logs`
 *
 * The whole pane tree is built here, into the empty `#tab-logs` mount point, so
 * index.html stays a shell. Every element the rest of the app reaches for by id
 * (`#table-filter-row`, `#request-logs-tbody`, `#logs`, …) is created by
 * `renderLogsView`, which main.js calls once before `initLogs`.
 *
 * The view mode lives in `appState.logsViewMode` because the tray's
 * `window.switchTab('logs')` and the poller both need to know which one is
 * live without reaching into this module.
 */

import { $, setHidden, html, raw } from '../core/dom.js';
import { invoke } from '../core/ipc.js';
import { appState } from '../core/state.js';
import { toast } from '../components/toast.js';
import { fetchRequestLogs, initRequestTable } from '../components/table.js';

/** The table view's dropdown filter options. */
const TABLE_FILTERS = [
  { id: 'table-model-filter', title: '按模型筛选', options: [['all', '全部模型']] },
  {
    id: 'table-status-filter',
    title: '按状态筛选',
    options: [
      ['all', '全部状态'],
      ['2xx', '2xx 成功'],
      ['4xx', '4xx 客户端错误'],
      ['5xx', '5xx 服务端错误'],
      ['error', '仅异常 / 错误'],
    ],
  },
  {
    id: 'table-streamed-filter',
    title: '按流式模式筛选',
    options: [
      ['all', '全部流式'],
      ['stream', '仅流式 (Stream)'],
      ['sync', '仅非流式'],
    ],
  },
  { id: 'table-client-filter', title: '按会话/客户端筛选', options: [['all', '全部会话']] },
];

/** The console view's level filter options. */
const LEVEL_FILTERS = [
  ['ALL', '全部级别'],
  ['INFO', '仅 INFO'],
  ['WARN', '仅 WARN'],
  ['ERROR', '仅 ERROR'],
];

/** `<select>` markup from `[value, label]` pairs. */
function options(pairs, id, title) {
  const body = pairs.map(([value, label]) => html`
    <option value="${value}">${label}</option>`).join('');
  return html`<select id="${id}" class="log-select" title="${title}">${raw(body)}</select>`;
}

/**
 * The table view's column headers.
 *
 * Column widths live in CSS (`components.css`), not inline: a fixed-layout table
 * sizes columns from `width`, and keeping the numbers in one place is what stops
 * a header from disagreeing with the cell rules that clip against it.
 */
function tableHead() {
  return html`
    <tr>
      <th class="col-time">时间</th>
      <th class="col-session">会话</th>
      <th class="col-model">模型 ID / 路径</th>
      <th class="col-input">Input</th>
      <th class="col-output">Output</th>
      <th class="col-duration">Duration</th>
      <th class="col-streamed">Streamed</th>
      <th class="col-status">Status</th>
      <th class="col-actions">操作</th>
    </tr>
  `;
}

/**
 * Build the whole logs pane.
 *
 * Called once from main.js. Both sub-views are always in the DOM and are shown
 * or hidden by `setLogsViewMode`; building them lazily would mean a second
 * `initRequestTable()`-style wiring pass, and the hidden pane is what keeps the
 * two lists' scroll positions alive across a toggle.
 */
export function renderLogsView() {
  const root = $('#tab-logs');
  if (!root) return;

  const tableFilters = TABLE_FILTERS.map(f => options(f.options, f.id, f.title)).join('');

  root.innerHTML = html`
    <!-- Row 1 (shared by both views): view toggle + log-directory shortcut. -->
    <div class="logs-toolbar">
      <div class="view-mode-group">
        <button type="button" class="btn-toggle active" id="view-mode-table" title="表格视图">表格</button>
        <button type="button" class="btn-toggle" id="view-mode-console" title="原始控制台日志">控制台</button>
      </div>
      <button class="btn btn-small" id="btn-reveal-logs" title="在访达中显示日志文件">日志目录</button>
    </div>

    <!-- Row 2 (table view): filters on the left, table actions on the right,
         mirroring the console subbar's layout. -->
    <div class="logs-filter-row" id="table-filter-row">
      <div class="logs-filter-group">
        ${raw(tableFilters)}
        <input type="text" id="table-search-input" class="log-input" placeholder="搜索模型/路由/会话/错误...">
      </div>
      <div class="logs-actions">
        <span class="log-count" id="table-log-count">0 条记录</span>
        <label class="auto-scroll-label">
          <input type="checkbox" id="table-autorefresh" checked> 实时刷新
        </label>
        <button class="btn btn-small btn-danger" id="btn-clear-db-logs" title="清空数据库记录">清空记录</button>
      </div>
    </div>

    <!-- Table view (default). -->
    <div id="table-view" class="pane">
      <div class="table-responsive">
        <table class="data-table" id="request-logs-table">
          <thead>${raw(tableHead())}</thead>
          <tbody id="request-logs-tbody">
            <tr><td colspan="9" class="table-empty">暂无请求记录</td></tr>
          </tbody>
        </table>
      </div>
      <div class="table-pagination">
        <div class="pagination-info" id="pagination-info">第 1 页 · 共 0 条</div>
        <div class="pagination-buttons">
          <button class="btn btn-small" id="btn-prev-page" disabled>上一页</button>
          <button class="btn btn-small" id="btn-next-page" disabled>下一页</button>
        </div>
      </div>
    </div>

    <!-- Console view (secondary). -->
    <div id="console-view" class="logs-console-view" hidden>
      <div class="console-subbar">
        ${raw(options(LEVEL_FILTERS, 'log-level-filter', '按级别筛选'))}
        <input type="text" id="log-search-input" class="log-input" placeholder="搜索控制台内容...">
        <span class="log-count" id="log-count">0 条日志</span>
        <label class="auto-scroll-label auto-scroll-label--end">
          <input type="checkbox" id="log-autoscroll" checked> 自动滚动
        </label>
        <button class="btn btn-small" id="btn-clear-logs">清空控制台</button>
      </div>
      <div id="logs" class="logs-container"></div>
    </div>
  `;
}

/** Switch between the table and console panes. */
export function setLogsViewMode(mode) {
  appState.logsViewMode = mode;

  $('#view-mode-table')?.classList.toggle('active', mode === 'table');
  $('#view-mode-console')?.classList.toggle('active', mode === 'console');

  const isTable = mode === 'table';
  setHidden($('#table-filter-row'), !isTable);
  setHidden($('#table-view'), !isTable);
  setHidden($('#console-view'), isTable);

  // `console-filter-row` no longer exists as a separate row: both rows below the
  // toolbar are table-specific and the console's own subbar lives inside
  // #console-view, so the console pane needs no extra row to hide.

  if (isTable) fetchRequestLogs();
  else fetchLogs();
}

/** Load the raw log tail. */
export async function fetchLogs() {
  try {
    const res = await invoke('get_logs');
    if (res?.entries) {
      appState.logs = res.entries;
      renderLogs();
    }
  } catch (err) {
    console.error('fetchLogs error:', err);
  }
}

/** Repaint the console pane from the cached entries. */
export function renderLogs() {
  const container = $('#logs');
  const levelFilter = $('#log-level-filter');
  const searchInput = $('#log-search-input');
  if (!container || !levelFilter || !searchInput) return;

  const level = levelFilter.value;
  const query = searchInput.value.trim().toLowerCase();

  const filtered = (appState.logs || []).filter((entry) => {
    if (level !== 'ALL' && entry.level !== level) return false;
    if (query && !String(entry.message).toLowerCase().includes(query)) return false;
    return true;
  });

  const countEl = $('#log-count');
  if (countEl) countEl.textContent = `${filtered.length} 条日志`;

  container.innerHTML = filtered.map(l => html`<div class="log-line ${l.level}">
    <span class="ts">${l.ts}</span>
    <span class="lvl">[${l.level}]</span>
    <span class="msg">${l.message}</span>
  </div>`).join('');

  const autoscroll = $('#log-autoscroll');
  if (autoscroll?.checked) {
    container.scrollTop = container.scrollHeight;
  }
}

/**
 * Called by the logs poller while the table view is live. Skips the fetch when
 * the user has turned 实时刷新 off.
 */
export function pollRequestLogs() {
  const autoRefresh = $('#table-autorefresh');
  if (autoRefresh?.checked) fetchRequestLogs(false);
}

/** Wire the toolbar and console filters. Called once, from main.js. */
export function initLogs() {
  initRequestTable();

  $('#view-mode-table')?.addEventListener('click', () => setLogsViewMode('table'));
  $('#view-mode-console')?.addEventListener('click', () => setLogsViewMode('console'));

  $('#log-level-filter')?.addEventListener('change', renderLogs);
  $('#log-search-input')?.addEventListener('input', renderLogs);

  // Confirm via toast: the console is cleared only when the user taps 确认,
  // so an accidental click cannot wipe the visible logs.
  $('#btn-clear-logs')?.addEventListener('click', () => {
    toast('确定清空控制台日志？', 'ok', {
      label: '确认',
      timeout: 5000,
      onClick: async (dismiss) => {
        try {
          await invoke('clear_logs');
          appState.logs = [];
          renderLogs();
          dismiss();
          toast('控制台已清空');
        } catch (e) {
          dismiss();
          toast('清空失败: ' + e, 'error');
        }
      },
    });
  });

  $('#btn-reveal-logs')?.addEventListener('click', () => invoke('open_logs_dir'));
}
