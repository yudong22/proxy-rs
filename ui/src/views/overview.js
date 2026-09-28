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
  shortSessionId,
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

    <!-- Expanded by the 输出速度 card: one transposed table, metrics as rows and
         the three most recent conversations as columns, so the same measure can
         be read across sessions at a glance. A cell with no measurement shows
         "—" rather than a plausible-looking guess. -->
    <div class="section stat-detail" id="speed-detail" hidden>
      <table class="speed-sessions">
        <tbody id="speed-sessions-body"></tbody>
      </table>
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

  renderSpeedTable(m);
}

/**
 * The label for one session column header.
 *
 * The request table always abbreviates a session id to its first segment, but
 * that is wrong here: a Claude/Codex id is a 43-char UUID and needs shortening,
 * while `dsh:0.1.6-alpha.2` is already short — abbreviating it to `dsh:0.1.6`
 * would drop the version that distinguishes one DSH build's conversations from
 * another. So only genuinely long ids are shortened, and always keeping the
 * client prefix so the dialect stays visible.
 */
function sessionColumnLabel(sessionId) {
  if (!sessionId) return '—';
  const MAX = 22;
  if (sessionId.length <= MAX) return sessionId;
  return shortSessionId(sessionId);
}

/**
 * Render the 输出速度 drill-down as one transposed table.
 *
 * Layout: metrics are **rows**, the three most recent conversations are
 * **columns**, so the same measure reads straight across sessions. The header
 * row names the sessions; the current session's column is highlighted.
 *
 * A cell with no measurement is "—", never a fabricated number — a session that
 * predates timing, or whose replies were plain non-streamed JSON, genuinely has
 * no speed to report.
 */
function renderSpeedTable(m) {
  const body = $('#speed-sessions-body');
  if (!body) return;

  const sessions = (Array.isArray(m.recent) ? m.recent : []).slice(0, 3);
  if (sessions.length === 0) {
    body.innerHTML = '<tr><td class="speed-empty">暂无会话</td></tr>';
    return;
  }

  // Metric rows, in display order. Each returns display text for one session;
  // `—` is reserved for "not measured" so a real 0 stays tellable apart.
  const rows = [
    {
      label: '模型用时',
      value: s => (s.model_ms > 0 ? formatLongDuration(s.model_ms) : '—'),
    },
    {
      label: '工具调用用时',
      value: s =>
        s.tool_wait_ms === null || s.tool_wait_ms === undefined
          ? '—'
          : `${formatSeconds(s.tool_wait_ms)}${s.tool_waits > 1 ? `（${s.tool_waits} 次）` : ''}`,
    },
    {
      label: '首 token 平均（TTFT）',
      value: s =>
        s.avg_ttft_ms === null || s.avg_ttft_ms === undefined
          ? '—'
          : formatSeconds(s.avg_ttft_ms),
    },
    {
      label: '输出 tokens',
      value: s => (s.output_tokens > 0 ? formatNumber(s.output_tokens) : '—'),
    },
    {
      label: '输出速度（TPS）',
      value: s => formatTps(s.speed_tps),
    },
  ];

  // Header row: the conversations. The full id stays in `title`; the label is
  // abbreviated only when it is actually long (a 43-char `claude:<uuid>`), so a
  // short id like `dsh:0.1.6-alpha.2` is shown whole rather than cut down to a
  // version prefix two sessions could share.
  const head = sessions
    .map(s => {
      const isCurrent = Boolean(s.session_id) && s.session_id === m.session_id;
      return html`
        <th class="col-session-cell${raw(isCurrent ? ' is-current' : '')}" title="${s.session_id || ''}">${sessionColumnLabel(s.session_id)}</th>
      `;
    })
    .join('');

  // Metric rows. Each cell is built by `html`, so every interpolated value
  // (model names, formatted text) is escaped before being joined as raw markup.
  // The current session's cells carry `is-current` so the whole column can be
  // highlighted, not just its header.
  const metricRows = rows
    .map(
      r => html`
        <tr>
          <th class="col-metric">${r.label}</th>
          ${raw(
            sessions
              .map(s => {
                const isCurrent = Boolean(s.session_id) && s.session_id === m.session_id;
                return html`<td class="${raw(isCurrent ? 'is-current' : '')}">${r.value(s)}</td>`;
              })
              .join(''),
          )}
        </tr>
      `,
    )
    .join('');

  body.innerHTML = html`
    <tr>
      <th class="col-metric">会话</th>
      ${raw(head)}
    </tr>
    ${raw(metricRows)}
  `;
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
