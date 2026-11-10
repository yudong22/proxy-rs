/**
 * 会话 detail pane: compact topbar, timeline, modern input-box footer, and modal metrics.
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
  formatTokens,
} from '../../lib/format.js';
import { pairResults, renderTimeline } from './entry.js';
import { isBrowserHostedConsole } from './list.js';

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
    <div class="sess-pop-section">
      <div class="sess-pop-section-title">模型使用分布</div>
      <table class="data-table sess-model-table">
        <thead><tr><th>模型</th><th>回合</th><th>输入</th><th>输出</th><th>缓存读</th></tr></thead>
        <tbody>${raw(rows)}</tbody>
      </table>
    </div>`;
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

/** Pick the most prominent model name for the pill display. */
function pickPrimaryModel(detail) {
  if (detail.models && detail.models.length > 0) {
    // Sort by turns descending to find the main model
    const sorted = [...detail.models].sort((a, b) => (b.turns || 0) - (a.turns || 0));
    if (sorted[0]?.model) return sorted[0].model;
  }
  return 'Claude 3.5';
}

/** Calculate overall cache hit percentage if available. */
function calculateCacheHitPct(tokens) {
  if (!tokens) return null;
  const read = Number(tokens.cache_read) || 0;
  const input = Number(tokens.input) || 0;
  const total = read + input;
  if (total <= 0) return null;
  return Math.round((read / total) * 100);
}

/**
 * Render one row's "open in browser" affordance.
 *
 * Mirrors `rowActionMarkup` in `list.js` — same two flavours, same
 * `sess-row-open` hook so one delegated handler can serve both — but sized to the
 * topbar's button style instead of floating over a row.
 */
function headerOpenMarkup(source, id) {
  if (isBrowserHostedConsole()) {
    return html`
      <button type="button" class="sess-topbar-btn sess-row-open" data-copy-id="${id}"
              data-copy-source="${source}"
              title="复制此会话的深链接" aria-label="复制此会话的深链接">
        <span class="sess-icon">⧉</span> 复制链接
      </button>`;
  }
  return html`
    <button type="button" class="sess-topbar-btn sess-row-open" data-open-id="${id}"
            data-open-source="${source}"
            title="在系统浏览器中打开此会话" aria-label="在浏览器打开此会话">
      <span class="sess-icon">↗</span> 打开
    </button>`;
}

/**
 * Compact topbar: brief title, path, quick branch/size tags and an info trigger.
 *
 * `source` is threaded through from the caller rather than read off `detail`: a
 * detail carries no source of its own (the same transcript could in principle be
 * reachable from two sources), and the view knows which one it asked for.
 */
export function renderCompactHeader(detail, { source = 'claude' } = {}) {
  const host = $('#session-detail-head');
  if (!host) return;
  const s = detail.summary;
  const path = s.cwd || s.encoded_project || '';

  host.innerHTML = html`
    <div class="sess-topbar-content">
      <div class="sess-topbar-info">
        <span class="sess-topbar-title" title="${s.title}">${s.title}</span>
        <span class="sess-topbar-sep">·</span>
        <span class="sess-topbar-path" title="${path}">${path}</span>
      </div>
      <div class="sess-topbar-meta">
        ${raw(s.git_branch ? html`<span class="badge-pill sess-badge-git">${s.git_branch}</span>` : '')}
        <span class="badge-pill">${formatBytes(s.size_bytes)}</span>
        ${raw(headerOpenMarkup(source, s.id))}
        <button type="button" class="sess-topbar-btn" id="btn-sess-topbar-stats" title="展开完整指标与详情">
          <span class="sess-icon">📊</span> 详情
        </button>
      </div>
    </div>
  `;
}

/** Render bottom input toolbar model name and bottom status line. */
export function renderFooterStats(detail) {
  const s = detail.summary;
  const primaryModel = pickPrimaryModel(detail);

  // Update model label in the input box
  const modelLabel = $('#sess-active-model-name');
  if (modelLabel) {
    modelLabel.textContent = primaryModel;
    modelLabel.title = `会话主要模型：${primaryModel}`;
  }

  // Update bottom status line (referencing the prompt screenshot):
  // ⏱️ 12 轮 1236 步 · 202 tok/s   🗄️ 425M tok · 缓存命中 99.7%   🕒 16%
  const turnsCount = s.turns || 0;
  const stepsCount = s.assistant_turns || 0;
  const toolCallsCount = s.tool_calls || 0;

  const totalTokens = (detail.tokens?.input || 0) + (detail.tokens?.output || 0) + (detail.tokens?.cache_read || 0);
  const cacheHit = calculateCacheHitPct(detail.tokens);

  const turnsText = `⏱️ ${turnsCount} 轮 ${stepsCount} 步${toolCallsCount ? ` · ${toolCallsCount} 工具` : ''}`;
  const tokensText = `🗄️ ${formatTokens(totalTokens)} tok${cacheHit !== null ? ` · 缓存命中 ${cacheHit}%` : ''}`;

  let extraText = '';
  if (detail.cost_state && detail.cost_state.total_cost_usd > 0) {
    extraText = `💰 $${detail.cost_state.total_cost_usd.toFixed(4)}`;
  } else if (detail.gateway?.requests) {
    extraText = `🌐 网关 ${detail.gateway.requests} 请求`;
  } else if (s.subagents > 0) {
    extraText = `🤖 ${s.subagents} 子代理`;
  } else {
    extraText = `📦 ${formatBytes(s.size_bytes)}`;
  }

  const statusBar = $('#sess-status-bar');
  if (statusBar) {
    statusBar.innerHTML = html`
      <span class="sess-status-col">${turnsText}</span>
      <span class="sess-status-col">${tokensText}</span>
      <span class="sess-status-col">${extraText}</span>
    `;
  }
}

/** Render the complete detail popup/modal content. */
export function renderDetailModal(detail) {
  const body = $('#sess-pop-body');
  if (!body) return;
  const s = detail.summary;
  const span =
    s.first_seen_ms && s.last_seen_ms
      ? `${formatLocalDateTime(s.first_seen_ms)} → ${formatLocalDateTime(s.last_seen_ms)}`
      : '';
  const cost = detail.cost_state
    ? html`<div class="sess-gateway-note">Claude Code 自报成本 $${detail.cost_state.total_cost_usd.toFixed(4)} · 用时 ${formatLongDuration(detail.cost_state.total_duration_ms)}</div>`
    : '';

  body.innerHTML = html`
    <!-- 会话基础信息 -->
    <div class="sess-pop-section">
      <div class="sess-detail-title">${s.title}</div>
      <div class="sess-detail-path" title="${s.cwd || s.encoded_project}">${s.cwd || s.encoded_project}</div>
      <div class="sess-detail-tags">
        ${raw(s.git_branch ? html`<span class="badge-pill">${s.git_branch}</span>` : '')}
        ${raw(s.version ? html`<span class="badge-pill">v${s.version}</span>` : '')}
        <span class="badge-pill" title="${s.id}">ID: ${s.id.slice(0, 8)}</span>
        <span class="badge-pill">${formatBytes(s.size_bytes)}</span>
        ${raw(span ? html`<span class="sess-muted">${span}</span>` : '')}
      </div>
    </div>

    <!-- 交互轮次网格 -->
    <div class="sess-pop-section">
      <div class="sess-pop-section-title">交互概览</div>
      <div class="sess-stats">
        <div class="sess-kv"><span>人工轮次</span><strong>${s.turns}</strong></div>
        <div class="sess-kv"><span>助手回合</span><strong>${s.assistant_turns}</strong></div>
        <div class="sess-kv"><span>工具调用</span><strong>${s.tool_calls}</strong></div>
        <div class="sess-kv"><span>子代理</span><strong>${s.subagents}</strong></div>
      </div>
    </div>

    <!-- 本地 Token 与成本 -->
    <div class="sess-pop-section">
      <div class="sess-pop-section-title">本地记录（来自会话文件）</div>
      <div class="sess-stats">
        ${raw(tokensRow(detail.tokens))}
        ${raw(detail.tokens_incl_subagents.output !== detail.tokens.output
          ? html`<div class="sess-gateway-note">含子代理：输出 ${formatNumber(detail.tokens_incl_subagents.output)}</div>`
          : '')}
        ${raw(cost)}
      </div>
    </div>

    <!-- 网关记录 -->
    <div class="sess-pop-section">
      <div class="sess-pop-section-title">代理网关记录</div>
      ${raw(gatewayBox(detail))}
    </div>

    <!-- 模型分布明细 -->
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
    // Prepend without moving the viewport
    host.insertAdjacentHTML('afterbegin', snippet.value);
  }

  // Update top load-more bar
  const moreBar = $('#session-load-more-bar');
  const topHint = $('#session-top-hint');
  const count = $('#session-timeline-count');

  const loadedCount = detail.timeline_offset + detail.timeline.length;
  const totalCount = detail.timeline_total;

  if (detail.has_more) {
    if (moreBar) moreBar.removeAttribute('hidden');
    if (topHint) topHint.setAttribute('hidden', '');
    if (count) {
      count.textContent = `已显示 ${loadedCount} / ${totalCount} 条`;
    }
  } else {
    if (moreBar) moreBar.setAttribute('hidden', '');
    if (topHint) {
      topHint.removeAttribute('hidden');
      const hintText = topHint.querySelector('span');
      if (hintText) hintText.textContent = `已到达会话开头（共 ${totalCount} 条记录）`;
    }
  }
}

/** First paint of a session: compact header, footer stats, modal data, timeline, scrolled to bottom. */
export function renderDetail(detail, { source = 'claude' } = {}) {
  renderCompactHeader(detail, { source });
  renderFooterStats(detail);
  renderDetailModal(detail);
  renderTimelineBody(detail, { replace: true });

  const scrollHost = $('#session-timeline-scroll');
  if (scrollHost) {
    // Ensure scroll jumps to bottom so newest conversation is visible
    requestAnimationFrame(() => {
      scrollHost.scrollTop = scrollHost.scrollHeight;
    });
  }

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
