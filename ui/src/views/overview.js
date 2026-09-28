/**
 * Overview view: service status, token totals, and the local endpoint
 * reference users copy into their clients.
 *
 * The markup is built here, into the empty `#tab-overview` mount point.
 */

import { $, setText, setClass, setHidden, html, raw } from '../core/dom.js';
import { invoke } from '../core/ipc.js';
import { appState } from '../core/state.js';
import {
  formatNumber,
  formatPoints,
  formatTps,
  formatLongDuration,
  formatSeconds,
} from '../lib/format.js';
import { toast } from '../components/toast.js';

/**
 * A metric card.
 *
 * @param {string} id       Element the value is written into.
 * @param {string} label
 * @param {{cardId?: string, clickable?: boolean, title?: string, sub?: string}} [opts]
 *        `cardId` is only needed when something targets the card itself (the
 *        cards that expand a detail panel below).
 *        `sub` renders a secondary element (an id for the sub-line) so a card
 *        can carry a second, smaller line under its value.
 */
function metric(id, label, { cardId = '', clickable = false, title = '', sub = '' } = {}) {
  return html`
    <div class="metric-card${clickable ? ' clickable' : ''}"${raw(cardId ? ` id="${cardId}"` : '')}${raw(title ? ` title="${title}"` : '')}>
      <div class="metric-label">${label}${raw(clickable ? ' <span class="stat-caret"></span>' : '')}</div>
      <div class="metric-value" id="${id}">-</div>
      ${raw(sub ? `<div class="metric-sub" id="${sub}"></div>` : '')}
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
      ${raw(metric('metric-provider', '配置厂商'))}
      ${raw(metric('metric-points', '剩余积分', {
        cardId: 'stat-card-points', clickable: true, title: '点击刷新剩余积分',
        sub: 'metric-points-account',
      }))}
      ${raw(metric('metric-speed', '输出速度', {
        cardId: 'stat-card-speed', clickable: true, title: '点击查看本次生成详情',
      }))}
      ${raw(metric('metric-overrides', '今日 override 次数', {
        cardId: 'stat-card-overrides', clickable: true, title: '点击查看请求日志',
      }))}
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

    <!-- Expanded by the 输出速度 card. Values are session aggregates over the
         recent-turns window; the comparison table below shows the last few
         conversations. A cell with no measurement shows "—" rather than a
         plausible-looking guess. -->
    <div class="section stat-detail" id="speed-detail" hidden>
      <dl class="kv">
        <dt>当前会话</dt><dd><code id="speed-session" class="speed-session-id">—</code></dd>
        <dt>模型用时（累计）</dt><dd><code id="speed-model-time">—</code></dd>
        <dt>工具调用用时（累计）</dt><dd><code id="speed-tool-time">—</code></dd>
        <dt>首 token 平均（TTFT）</dt><dd><code id="speed-ttft">—</code></dd>
        <dt>输出 tokens（累计）</dt><dd><code id="speed-tokens">—</code></dd>
        <dt>输出速度（TPS）</dt><dd><code id="speed-tps">—</code></dd>
      </dl>

      <div class="speed-sessions-title">最近会话对比</div>
      <div class="speed-sessions-wrap">
        <table class="speed-sessions">
          <thead>
            <tr>
              <th class="col-session">会话</th>
              <th>模型用时</th>
              <th>TTFT</th>
              <th>TPS</th>
            </tr>
          </thead>
          <tbody id="speed-sessions-body"></tbody>
        </table>
      </div>
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

  // The identity in force right now: 配置厂商 / 剩余积分 (+账号) / 输出速度.
  const identity = status.current_identity || {};
  appState.currentIdentity = identity;
  // 配置厂商: the backend reports the provider id, as before. An identity
  // label describes *who* is serving, this describes *where* — and only the
  // latter is known unconditionally (it survives a stopped pool).
  setText('metric-provider', status.provider || '-');
  setText('metric-points', formatPoints(identity.points));
  // The account name is shown as the 剩余积分 card's sub-line: the balance
  // belongs to that account, so reading them together is what the number means.
  setText('metric-points-account', identity.label || '—');

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
  setText('metric-overrides', formatNumber(s.overrides_total));
}

/**
 * Paint the 输出速度 card and its drill-down from the current session's metrics.
 *
 * The values are session-level aggregates, not the latest turn: `speed_tps` is
 * summed tokens over summed generation time, TTFT is the mean across measured
 * turns, and the tool figure is the total attributed inside the window.
 *
 * Every field is rendered even when unmeasured — a missing number shows as "—"
 * so the panel never implies a measurement that was not taken.
 */
export async function refreshSessionMetrics() {
  let m;
  try {
    m = await invoke('get_session_metrics');
  } catch (err) {
    console.error('refreshSessionMetrics error:', err);
    return;
  }
  if (!m) return;
  appState.sessionMetrics = m;

  setText('metric-speed', formatTps(m.speed_tps));
  // The card's tooltip carries the context the small value cannot: which model,
  // how many turns, and how many tokens the rate was measured over.
  const card = $('#stat-card-speed');
  if (card) {
    const parts = [];
    if (m.model) parts.push(m.model);
    const turns = Number(m.measured_turns) || 0;
    if (turns > 0) parts.push(`平均 ${turns} 轮`);
    if (m.output_tokens > 0) parts.push(`共 ${formatNumber(m.output_tokens)} tokens`);
    card.title = parts.length
      ? `${parts.join(' · ')} — 点击查看会话生成详情`
      : '暂无可测量的生成记录';
  }

  // The session's identity is its id from the request log, not the model name:
  // several models can serve one conversation (a fallback switch changes it),
  // but the id is what ties these turns together.
  setText('speed-session', m.session_id || '—');
  // 模型用时 is summed generation time over the window, not one request's
  // duration — the latter also contains the client's read tail.
  setText('speed-model-time', m.model_ms > 0 ? formatLongDuration(m.model_ms) : '—');
  setText(
    'speed-tool-time',
    m.tool_wait_ms === null || m.tool_wait_ms === undefined
      ? '—'
      : `${formatSeconds(m.tool_wait_ms)}${m.tool_waits > 1 ? `（${m.tool_waits} 次）` : ''}`,
  );
  setText(
    'speed-ttft',
    m.avg_ttft_ms === null || m.avg_ttft_ms === undefined
      ? '—'
      : formatSeconds(m.avg_ttft_ms),
  );
  setText('speed-tps', formatTps(m.speed_tps));
  setText(
    'speed-tokens',
    m.output_tokens > 0 ? formatNumber(m.output_tokens) : '—',
  );

  renderRecentSessions(m.recent, m.session_id);
}

/**
 * Render the recent-sessions comparison table.
 *
 * The first column is the session id, widened in CSS so a long id stays on one
 * line rather than wrapping into a second row height.
 */
function renderRecentSessions(recent, currentId) {
  const body = $('#speed-sessions-body');
  if (!body) return;
  const rows = Array.isArray(recent) ? recent : [];
  if (rows.length === 0) {
    body.innerHTML = '<tr><td colspan="4" class="speed-empty">暂无会话</td></tr>';
    return;
  }
  body.innerHTML = rows
    .map(s => {
      const isCurrent = s.session_id && s.session_id === currentId;
      return html`
        <tr${raw(isCurrent ? ' class="is-current"' : '')}>
          <td class="col-session"><span class="mono" title="${s.session_id || ''}">${s.session_id || '—'}</span></td>
          <td>${s.model_ms > 0 ? formatLongDuration(s.model_ms) : '—'}</td>
          <td>${s.avg_ttft_ms === null || s.avg_ttft_ms === undefined ? '—' : formatSeconds(s.avg_ttft_ms)}</td>
          <td>${formatTps(s.speed_tps)}</td>
        </tr>
      `;
    })
    .join('');
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

  // The 输出速度 card expands the per-turn generation detail. It re-reads on
  // open so the panel describes the turn that just finished rather than
  // whichever one was current at the last 10 s poll.
  $('#stat-card-speed')?.addEventListener('click', async () => {
    const card = $('#stat-card-speed');
    const detail = $('#speed-detail');
    if (!card || !detail) return;
    const open = detail.hasAttribute('hidden');
    setHidden(detail, !open);
    card.classList.toggle('active', open);
    if (open) await refreshSessionMetrics();
  });

  // The 今日 override 次数 card jumps to the request log, where each override's
  // reason is spelled out in the override column.
  $('#stat-card-overrides')?.addEventListener('click', () => {
    window.switchTab('logs');
  });

  // The 剩余积分 card refreshes on click. Debounced **per identity**: each
  // click costs one upstream billing request per identity, so hammering the
  // card would hammer the billing endpoint — but the window must not follow the
  // *card*. Otherwise refreshing account A, then switching to B, left B locked
  // out for the rest of A's window, which is what the 3 分钟 prompt was
  // complaining about. MATCHES_COOLDOWN_MS mirrors the server-side failover
  // memory TTL (3 min).
  const MATCHES_COOLDOWN_MS = 3 * 60 * 1000;
  /** Identity id → wall-clock ms of its last successful refresh. */
  const lastPointsRefreshById = new Map();
  let refreshingPoints = false;

  $('#stat-card-points')?.addEventListener('click', async () => {
    if (refreshingPoints) return;

    const identityId = appState.currentIdentity?.id || '';
    const lastMs = lastPointsRefreshById.get(identityId) || 0;
    const elapsed = Date.now() - lastMs;
    if (elapsed < MATCHES_COOLDOWN_MS) {
      const remainSec = Math.ceil((MATCHES_COOLDOWN_MS - elapsed) / 1000);
      toast(`该账号积分刚刚刷新过，${remainSec}s 后可再次刷新`);
      return;
    }

    const card = $('#stat-card-points');
    const value = $('#metric-points');
    refreshingPoints = true;
    if (value) value.textContent = '⏳ 刷新中…';
    if (card) card.classList.add('loading');
    try {
      await invoke('wb_refresh_points');
      lastPointsRefreshById.set(identityId, Date.now());
      // `refreshStatus` in the finally block repaints the card from
      // `get_status`, so the fresh balance flows through the same path as a
      // status poll — one way for the card to update.
      toast('剩余积分已刷新');
    } catch (err) {
      console.error('refresh points failed:', err);
      toast('刷新剩余积分失败: ' + err, 'error');
      // Allow a retry immediately: nothing was fetched, so the window would
      // otherwise pointlessly block the user for three minutes.
      lastPointsRefreshById.delete(identityId);
    } finally {
      refreshingPoints = false;
      if (card) card.classList.remove('loading');
      await refreshStatus();
    }
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
