/**
 * Request detail modal.
 *
 * Renders one request-log row as a key/value table. Session identity belongs
 * here rather than in the list: it is the key that ties this row to the other
 * requests in the same conversation.
 */

import { html, raw } from '../core/dom.js';
import { appState } from '../core/state.js';
import { formatDuration } from '../lib/format.js';
import { openModal } from './modal.js';
import { statusBadge, isFailedRequest, overridePillClass } from './pills.js';

/** Open the detail view for the row with this id, if it is still loaded. */
export function showRequestModal(id) {
  const item = (appState.requestLogs || []).find(r => r.id === id);
  if (!item) return;

  const inputTokens = Number(item.input_tokens) || 0;
  const outputTokens = Number(item.output_tokens) || 0;
  const cacheRead = Number(item.cache_read_tokens) || 0;
  const cacheWrite = Number(item.cache_write_tokens) || 0;

  const sessionRows = item.session_id
    ? html`
      <tr><td class="detail-key">会话 ID</td><td class="detail-val"><span class="mono">${item.session_id}</span></td></tr>
      <tr><td class="detail-key">客户端</td><td class="detail-val">${item.client || 'unknown'}</td></tr>`
    : html`<tr><td class="detail-key">会话 ID</td><td class="detail-val">未识别</td></tr>`;

  const errorRow = item.error
    ? html`<tr>
        <td class="detail-key">错误信息</td>
        <td class="detail-val"><div class="error-block">${item.error}</div></td>
      </tr>`
    : '';

  const isErr = isFailedRequest(item);

  const overrideRows = (item.override_key || item.override_model)
    ? html`
      <tr>
        <td class="detail-key">Override 结果</td>
        <td class="detail-val">
          <span class="badge-pill override-pill ${overridePillClass(item)}">
            ${isErr ? '✗ 纠错尝试后仍失败' : '✓ 纠错切换成功'}
          </span>
        </td>
      </tr>
      <tr><td class="detail-key">有效账号 / 模型</td><td class="detail-val"><span class="mono">${[item.override_key, item.override_model].filter(Boolean).join(' @ ') || '—'}</span></td></tr>
      <tr><td class="detail-key">纠错与切换轨迹</td><td class="detail-val">${item.override_reason || '—'}</td></tr>`
    : '';

  const tokenInputLabel = (isErr && inputTokens > 0)
    ? html`输入 Tokens <span class="detail-estimated">(估算值)</span>`
    : '输入 Tokens';

  openModal('请求详情', html`
    <table class="detail-table">
      <tr><td class="detail-key">请求 ID</td><td class="detail-val">#${item.id}</td></tr>
      <tr><td class="detail-key">时间</td><td class="detail-val">${item.created_at}</td></tr>
      <tr><td class="detail-key">模型 ID</td><td class="detail-val"><strong>${item.model}</strong></td></tr>
      <tr><td class="detail-key">请求路径</td><td class="detail-val">${item.route}</td></tr>
      ${raw(sessionRows)}
      <tr><td class="detail-key">状态</td><td class="detail-val">${raw(statusBadge(item))}</td></tr>
      <tr><td class="detail-key">传输模式</td><td class="detail-val">${item.streamed ? '是 (Stream)' : '否 (Non-stream)'}</td></tr>
      <tr><td class="detail-key">响应耗时</td><td class="detail-val">${formatDuration(item.duration_ms)} (${item.duration_ms} ms)</td></tr>
      <tr><td class="detail-key">${raw(tokenInputLabel)}</td><td class="detail-val">${inputTokens.toLocaleString()}</td></tr>
      <tr><td class="detail-key">缓存读取</td><td class="detail-val">${cacheRead.toLocaleString()}</td></tr>
      <tr><td class="detail-key">缓存写入</td><td class="detail-val">${cacheWrite.toLocaleString()}</td></tr>
      <tr><td class="detail-key">输出 Tokens</td><td class="detail-val">${outputTokens.toLocaleString()}</td></tr>
      <tr><td class="detail-key">总计消耗</td><td class="detail-val"><strong>${(inputTokens + outputTokens).toLocaleString()}</strong></td></tr>
      ${raw(errorRow)}
      ${raw(overrideRows)}
    </table>
  `);
}
