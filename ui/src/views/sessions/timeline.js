/**
 * 会话 detail pane: header, usage, gateway join, and the timeline.
 *
 * Pure rendering over a `SessionDetail` payload; the fetching and the
 * "load older" bookkeeping live in `sessions/index.js`. Every string that comes
 * from the file is interpolated through `html` (auto-escaped) — see `entry.js`
 * for why that matters.
 */

import { $, html, raw } from '../../core/dom.js';
import {
  formatBytes,
  formatLocalDateTime,
  formatLongDuration,
  formatNumber,
} from '../../lib/format.js';
import { pairResults, renderTimeline } from './entry.js';

function tokensRow(t) {
  return html`
    <div class="sess-kv"><span>输入</span><strong>${formatNumber(t.input)}</strong></div>
    <div class="sess-kv"><span>输出</span><strong>${formatNumber(t.output)}</strong></div>
    <div class="sess-kv"><span>缓存读</span><strong>${formatNumber(t.cache_read)}</strong></div>
    <div class="sess-kv"><span>缓存写</span><strong>${formatNumber(t.cache_write)}</strong></div>`;
}

function modelsTable(models) {
  if (!models || !models.length) return '';
  const rows = models
    .map(
      (m) => html`
        <tr>
          <td>${m.model}</td>
          <td class="num">${formatNumber(m.turns)}</td>
          <td class="num">${formatNumber(m.tokens.input)}</td>
          <td class="num">${formatNumber(m.tokens.output)}</td>
          <td class="num">${formatNumber(m.tokens.cache_read)}</td>
        </tr>`,
    )
    .join('');
  return html`
    <table class="data-table sess-model-table">
      <thead><tr><th>模型</th><th>回合</th><th>输入</th><th>输出</th><th>缓存读</th></tr></thead>
      <tbody>${raw(rows)}</tbody>
    </table>`;
}

/**
 * The gateway box.
 *
 * Three distinct sentences, because they mean different things:
 *  - recorded        → totals
 *  - not recorded    → the request never reached this proxy (or carried no
 *                      session header); this is a fact, not an error
 *  - unavailable     → the stats DB could not be read, so we do not know
 */
function gatewayBox(detail) {
  const gw = detail.gateway;
  if (gw) {
    return html`
      <div class="sess-gateway">
        <div class="sess-gateway-title">网关记录 <code>${gw.session_id}</code></div>
        <div class="sess-kv"><span>请求</span><strong>${formatNumber(gw.requests)}</strong></div>
        <div class="sess-kv"><span>输入</span><strong>${formatNumber(gw.input_tokens)}</strong></div>
        <div class="sess-kv"><span>输出</span><strong>${formatNumber(gw.output_tokens)}</strong></div>
        <div class="sess-kv"><span>缓存读</span><strong>${formatNumber(gw.cache_read_tokens)}</strong></div>
        ${gw.first_at ? html`<div class="sess-gateway-note">首次 ${gw.first_at} · 最近 ${gw.last_at}</div>` : ''}
      </div>`;
  }
  if (detail.gateway_error) {
    return html`<div class="sess-gateway sess-gateway--muted">${detail.gateway_error}</div>`;
  }
  return html`<div class="sess-gateway sess-gateway--muted">网关未记录此会话（请求未经过本代理，或未携带会话头）</div>`;
}

/** The header + usage block, repainted whenever a session is opened. */
export function renderDetailHeader(detail) {
  const host = $('#session-detail-head');
  if (!host) return;
  const s = detail.summary;
  const span =
    s.first_seen_ms && s.last_seen_ms
      ? `${formatLocalDateTime(s.first_seen_ms)} → ${formatLocalDateTime(s.last_seen_ms)}`
      : '';
  const cost = detail.cost_state
    ? html`<div class="sess-gateway-note">Claude Code 自报成本 $${detail.cost_state.total_cost_usd.toFixed(4)} · 用时 ${formatLongDuration(detail.cost_state.total_duration_ms)}</div>`
    : '';

  host.innerHTML = html`
    <div class="sess-detail-title">${s.title}</div>
    <div class="sess-detail-path" title="${s.cwd || s.encoded_project}">${s.cwd || s.encoded_project}</div>
    <div class="sess-detail-tags">
      ${raw(s.git_branch ? html`<span class="badge-pill">${s.git_branch}</span>` : '')}
      ${raw(s.version ? html`<span class="badge-pill">v${s.version}</span>` : '')}
      <span class="badge-pill" title="${s.id}">${s.id.slice(0, 8)}</span>
      <span class="badge-pill">${formatBytes(s.size_bytes)}</span>
      ${raw(span ? html`<span class="sess-muted">${span}</span>` : '')}
    </div>

    <div class="sess-stats">
      <div class="sess-kv"><span>人工轮次</span><strong>${s.turns}</strong></div>
      <div class="sess-kv"><span>助手回合</span><strong>${s.assistant_turns}</strong></div>
      <div class="sess-kv"><span>工具调用</span><strong>${s.tool_calls}</strong></div>
      <div class="sess-kv"><span>子代理</span><strong>${s.subagents}</strong></div>
    </div>

    <div class="sess-stats">
      <div class="sess-stats-label">本地记录（来自会话文件）</div>
      ${raw(tokensRow(detail.tokens))}
      ${raw(detail.tokens_incl_subagents.output !== detail.tokens.output
        ? html`<div class="sess-gateway-note">含子代理：输出 ${formatNumber(detail.tokens_incl_subagents.output)}</div>`
        : '')}
      ${raw(cost)}
    </div>

    ${raw(gatewayBox(detail))}
    ${raw(modelsTable(detail.models))}
  `;
}

/** Append (or replace) the timeline body. */
export function renderTimelineBody(detail, { replace } = {}) {
  const host = $('#session-timeline');
  if (!host) return;
  const snippet = renderTimeline(detail.timeline, pairResults(detail.timeline));
  if (replace) {
    host.innerHTML = snippet.value;
  } else {
    // Prepend without moving the viewport: the reader keeps the entry they
    // were looking at instead of being scrolled to the top of the page.
    host.insertAdjacentHTML('afterbegin', snippet.value);
  }
  const more = $('#btn-sessions-older');
  if (more) more.hidden = !detail.has_more;
  const count = $('#session-timeline-count');
  if (count) {
    count.textContent = `已显示 ${detail.timeline_offset + detail.timeline.length} / ${detail.timeline_total} 条`;
  }
}

/** First paint of a session: header, then the newest page, scrolled to its end. */
export function renderDetail(detail) {
  renderDetailHeader(detail);
  renderTimelineBody(detail, { replace: true });
  const host = $('#session-timeline');
  if (host) host.scrollTop = host.scrollHeight;
  $('#session-pane-empty')?.setAttribute('hidden', '');
  $('#session-pane')?.removeAttribute('hidden');
}

/** Explain why nothing could be shown, in place of a blank pane. */
export function renderDetailMessage(message) {
  const head = $('#session-detail-head');
  if (head) head.innerHTML = '';
  const host = $('#session-timeline');
  if (host) host.innerHTML = '';
  const empty = $('#session-pane-empty');
  if (empty) {
    empty.removeAttribute('hidden');
    const msg = $('.sess-empty', empty);
    if (msg) msg.textContent = message;
  }
  $('#session-pane')?.setAttribute('hidden', '');
}
