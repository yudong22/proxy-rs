/**
 * Overview view: service status, token totals, and the local endpoint
 * reference users copy into their clients.
 *
 * The markup is built here, into the empty `#tab-overview` mount point.
 */

import { $, setText, setClass, setHidden, html, raw } from '../core/dom.js';
import { invoke } from '../core/ipc.js';
import { appState } from '../core/state.js';
import { formatNumber, formatUptime } from '../lib/format.js';
import { toast } from '../components/toast.js';

/**
 * A metric card.
 *
 * @param {string} id       Element the value is written into.
 * @param {string} label
 * @param {{cardId?: string, clickable?: boolean, title?: string}} [opts]
 *        `cardId` is only needed when something targets the card itself (the
 *        cache card, which expands the detail panel below it).
 */
function metric(id, label, { cardId = '', clickable = false, title = '' } = {}) {
  return html`
    <div class="metric-card${clickable ? ' clickable' : ''}"${raw(cardId ? ` id="${cardId}"` : '')}${raw(title ? ` title="${title}"` : '')}>
      <div class="metric-label">${label}${raw(clickable ? ' <span class="stat-caret"></span>' : '')}</div>
      <div class="metric-value" id="${id}">-</div>
    </div>
  `;
}

/** The endpoint reference rows: label → id, with a copy button on some. */
const ENDPOINTS = [
  { label: 'Messages API', id: 'endpoint-messages', copy: true },
  { label: 'Responses API', id: 'endpoint-responses', copy: true },
  { label: 'Models API', id: 'endpoint-models', copy: true },
  { label: '当前上游地址', id: 'overview-upstream' },
  { label: '配置文件', id: 'overview-config-path', value: '~/.proxy-rs/gui-settings.json' },
  { label: '日志文件', id: 'overview-log-path', value: '~/.proxy-rs/logs/proxy.log' },
  { label: '开机自启动', id: 'overview-lal', plain: true },
];

/** Render this view into its mount point. Called once, from main.js. */
export function renderOverview() {
  const root = $('#tab-overview');
  if (!root) return;

  const rows = ENDPOINTS.map(e => html`
    <dt>${e.label}</dt>
    <dd><code id="${e.id}">${e.value || ''}</code>${
      raw(e.copy ? ` <button class="btn-copy" data-copy="${e.id}">复制</button>` : '')
    }</dd>
  `).join('');

  root.innerHTML = html`
    <div class="metrics-grid">
      ${raw(metric('metric-status', '服务状态', { cardId: 'card-status' }))}
      ${raw(metric('metric-port', '监听端口'))}
      ${raw(metric('metric-uptime', '运行时间'))}
      ${raw(metric('metric-provider', '已配置厂商'))}
    </div>

    <div class="metrics-grid">
      ${raw(metric('stat-requests', '请求总数'))}
      ${raw(metric('stat-tokens-total', 'Token 总量'))}
      ${raw(metric('stat-cache-pct', '缓存命中率', {
        cardId: 'stat-card-cache', clickable: true, title: '点击查看缓存明细',
      }))}
      ${raw(metric('stat-requests-failed', '失败请求'))}
    </div>

    <!-- Expanded by the 缓存命中率 card above; the hidden attribute is the
         initial state and the global [hidden] rule in base.css keeps it out of
         layout even after the section's own display is set. -->
    <div class="section stat-detail" id="stat-detail" hidden>
      <dl class="kv">
        <dt>输入 (未缓存)</dt><dd><code id="stat-tokens-input">0</code></dd>
        <dt>缓存读取</dt><dd><code id="stat-tokens-cache-read">0</code></dd>
        <dt>缓存写入</dt><dd><code id="stat-tokens-cache-write">0</code></dd>
        <dt>输出</dt><dd><code id="stat-tokens-output">0</code></dd>
      </dl>
    </div>

    <div class="section">
      <div class="section-title">本地调用接口</div>
      <dl class="kv">${raw(rows)}</dl>
    </div>
  `;
}

/** Read the current service state and repaint everything that depends on it. */
export async function refreshStatus() {
  let status;
  try {
    status = await invoke('get_status');
  } catch (err) {
    console.error('refreshStatus error:', err);
    return;
  }
  if (!status) return;

  appState.running = status.running || status.service_running;
  appState.port = status.port || status.configured_port || 3456;

  // Header
  setClass($('#status-dot'), 'running', appState.running);
  setClass($('#status-dot'), 'stopped', !appState.running);
  setText('status-text', appState.running ? `运行中 · 端口 ${appState.port}` : '已停止');

  const toggle = $('#btn-toggle-service');
  if (toggle) {
    toggle.textContent = appState.running ? '停止服务' : '启动服务';
    toggle.className = 'btn btn-small' + (appState.running ? '' : ' btn-primary');
  }

  // Status card
  const card = $('#card-status');
  if (card) {
    card.className = 'metric-card ' + (appState.running ? 'running' : 'stopped');
  }
  setText('metric-status', appState.running ? '正常运行' : '已停止');
  setText('metric-port', appState.port);
  setText('metric-uptime', formatUptime(status.uptime_secs || 0));
  setText('metric-provider', status.provider || '-');

  // Endpoint reference
  setText('endpoint-messages', `http://127.0.0.1:${appState.port}/v1/messages`);
  setText('endpoint-responses', `http://127.0.0.1:${appState.port}/v1/responses`);
  setText('endpoint-models', `http://127.0.0.1:${appState.port}/v1/models`);
  setText('overview-upstream', status.upstream_url || '-');
  if (status.version) setText('app-version', `v${status.version}`);
  if (status.log_path) setText('overview-log-path', status.log_path);

  // Reflect the user's persisted intent; whether the job is actually live is
  // reported separately by `launch_at_login_stale`.
  setText('overview-lal', status.launch_at_login ? '已开启' : '未开启');
  if (status.launch_at_login_stale) {
    // Setting says yes but launchd has no job: the login item is dead and the
    // app will not start at next login until it is re-armed.
    setText('overview-lal', '已开启（未生效）');
  }
}

/** Read today's token totals. */
export async function refreshStats() {
  let s;
  try {
    s = await invoke('get_stats');
  } catch (err) {
    console.error('refreshStats error:', err);
    return;
  }
  if (!s) return;

  setText('stat-requests', formatNumber(s.requests_total));
  setText('stat-tokens-total', formatNumber(s.tokens_total));
  setText('stat-cache-pct', `${s.cache_hit_pct || 0}%`);
  setText('stat-requests-failed', formatNumber(s.requests_failed));
  setText('stat-tokens-input', formatNumber(s.tokens_input));
  setText('stat-tokens-cache-read', formatNumber(s.tokens_cache_read));
  setText('stat-tokens-cache-write', formatNumber(s.tokens_cache_write));
  setText('stat-tokens-output', formatNumber(s.tokens_output));
}

/** Wire the interactions that belong to this view. Called once, from main.js. */
export function initOverview() {
  // Start / stop the service. `start_service` rejects when the port could not
  // be bound, so a busy port surfaces here instead of looking like success.
  const toggle = $('#btn-toggle-service');
  toggle?.addEventListener('click', async () => {
    toggle.disabled = true;
    try {
      await invoke(appState.running ? 'stop_service' : 'start_service');
      await refreshStatus();
    } catch (err) {
      console.error('Toggle service failed:', err);
      toast('操作失败: ' + err, 'error');
      await refreshStatus();
    } finally {
      toggle.disabled = false;
    }
  });

  // The 缓存命中率 card expands the token breakdown below it.
  $('#stat-card-cache')?.addEventListener('click', () => {
    const card = $('#stat-card-cache');
    const detail = $('#stat-detail');
    if (!card || !detail) return;
    const open = detail.hasAttribute('hidden');
    setHidden(detail, !open);
    card.classList.toggle('active', open);
  });

  // Copy-to-clipboard buttons on the endpoint rows.
  for (const btn of document.querySelectorAll('.btn-copy[data-copy]')) {
    btn.addEventListener('click', async () => {
      const el = document.getElementById(btn.dataset.copy);
      if (!el) return;
      await navigator.clipboard.writeText(el.textContent.trim());
      const original = btn.textContent;
      btn.textContent = '已复制!';
      setTimeout(() => { btn.textContent = original; }, 1500);
    });
  }
}
