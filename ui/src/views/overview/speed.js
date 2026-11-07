/**
 * The 输出速度 card and its drill-down table.
 *
 * Split out of `overview.js`. The drill-down is the largest piece of rendering on
 * this view — a transposed table with its own label formatters — and it shares
 * nothing with the rest of the page beyond the mount point.
 */

import { $, setText, html, raw } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { appState } from '../../core/state.js';
import {
  formatNumber,
  formatTps,
  formatLongDuration,
  formatSeconds,
  shortSessionId,
} from '../../lib/format.js';

/**
 * Paint the 输出速度 card and its drill-down from the current session's metrics.
 *
 * The values are session-level aggregates, not the latest turn: `speed_tps` is
 * summed tokens over summed generation time, TTFT is the mean across measured
 * turns, and the tool figure is the total attributed inside the window.
 *
 * **The window is today.** The backend restricts the whole aggregate — the
 * current session and every comparison column — to the local date, so the panel
 * answers "how is today going" rather than mixing days into one rate.
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
      ? `${parts.join(' · ')}（今日）— 点击查看会话生成详情`
      : '今日暂无可测量的生成记录';
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
/**
 * Where a session's last turn sits in time, for the 上次运行 row.
 *
 * Today's turns render as a bare clock (`14:32`), anything older keeps its date
 * (`10-26 14:32`). Showing the date for today too would spend four characters
 * on a constant; dropping it for an older day would make yesterday's session
 * look like it just ran.
 *
 * @param {string} lastAt - `YYYY-MM-DD HH:MM:SS` from the stats row.
 * @returns {string} A short, unambiguous label; the raw value when unparseable.
 */
function lastRunLabel(lastAt) {
  const raw = String(lastAt || '').trim();
  // `YYYY-MM-DD HH:MM:SS` — split rather than `new Date()`: the value is a
  // local wall-clock string with no zone, and `new Date` would reinterpret it
  // as UTC and shift it by the machine's offset.
  const m = /^(\d{4})-(\d{2})-(\d{2})[ T](\d{2}):(\d{2})/.exec(raw);
  if (!m) return raw || '—';
  const clock = `${m[4]}:${m[5]}`;
  const today = new Date();
  const isToday =
    Number(m[1]) === today.getFullYear() &&
    Number(m[2]) === today.getMonth() + 1 &&
    Number(m[3]) === today.getDate();
  return isToday ? clock : `${m[2]}-${m[3]} ${clock}`;
}

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
      label: '请求次数',
      // Today's requests for this session. `requests` is the count of turns the
      // window folded in, and the window is the current day, so this reads as
      // "how many requests today" without a second query.
      value: s => (s.requests > 0 ? formatNumber(s.requests) : '—'),
    },
    {
      label: '缓存命中',
      // `null` when the window carried no prompt traffic, which is not the same
      // as 0% — an unmeasured session must not claim it cached nothing.
      value: s =>
        s.cache_hit_pct === null || s.cache_hit_pct === undefined
          ? '—'
          : `${s.cache_hit_pct}%`,
    },
    {
      label: '模型用时',
      value: s => (s.model_ms > 0 ? formatLongDuration(s.model_ms) : '—'),
    },
    {
      label: '工具调用用时',
      // Cumulative over the session's whole window, so it routinely reaches
      // hours — a long agent session spends far more time waiting on tools than
      // generating. Formatted to minutes like 模型用时 above: the raw second
      // count was unreadable at this scale (`14190.1秒` is nearly 4 hours).
      value: s =>
        s.tool_wait_ms === null || s.tool_wait_ms === undefined
          ? '—'
          : `${formatLongDuration(s.tool_wait_ms)}${s.tool_waits > 1 ? `（${s.tool_waits} 次）` : ''}`,
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
      label: '上次运行',
      // `last_at` is the most recent turn's finish time in this window. It has
      // always been in the payload and was never rendered, which made a column
      // of today's aggregates read as if it were live: two sessions showing the
      // same tokens per second looked comparable even when one had been idle
      // for hours.
      //
      // Formatted as clock time rather than a relative "3 小时前": the row sits
      // beside cumulative figures for *today*, so the absolute time is what
      // lets the reader decide whether those figures describe recent behaviour.
      value: s => (s.last_at ? lastRunLabel(s.last_at) : '—'),
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
      // Subagent lineage. The numbers in this column already include the
      // children, so this line is what explains why a conversation's totals
      // exceed what its own turns produced — without it the column reads as
      // inflated. `0` is rendered as nothing: it means "none recorded", which
      // is also what every pre-lineage row says, so claiming "0 个子智能体"
      // would assert something the data cannot support.
      const subagents =
        s.subagent_sessions > 0
          ? html`<span class="session-subagents">含 ${s.subagent_sessions} 个子智能体</span>`
          : '';
      return html`
        <th class="col-session-cell${raw(isCurrent ? ' is-current' : '')}" title="${s.session_id || ''}">${sessionColumnLabel(s.session_id)}${raw(subagents)}</th>
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
