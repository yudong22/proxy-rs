/**
 * Daily check-in: the batch run across every pool account, and its report modal.
 */

import { $, html, raw } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { openModal } from '../../components/modal.js';
import { toast } from '../../components/toast.js';

/** Trigger batch checkin across all pool accounts and show report modal. */
export async function triggerBatchCheckin() {
  const btn = $('#btn-wb-checkin-all');
  if (btn) {
    btn.disabled = true;
    btn.textContent = '⏳ 正在打卡...';
  }

  try {
    // The manual button forces: the local "already claimed today" record must
    // not stop a run the user explicitly asked for.
    const report = await invoke('wb_checkin_all', { force: true });
    if (report.total === 0) {
      toast('账号池中没有已启用的账号', 'error');
      return;
    }

    const rows = (report.details || []).map((d) => {
      let badge = '';
      if (d.status === 'success') {
        badge = '<span class="badge-pill wb-state-pill wb-state-ok">打卡成功</span>';
      } else if (d.status === 'already_checked_in') {
        badge = '<span class="badge-pill wb-state-pill" style="background:rgba(59,130,246,0.15);color:#3b82f6;">今日已打卡</span>';
      } else {
        badge = '<span class="badge-pill wb-state-pill wb-state-expired">失败</span>';
      }

      return html`
        <tr>
          <td style="font-weight:600;">${d.label} <span class="mono" style="color:var(--muted);font-size:11px;">(${d.id})</span></td>
          <td>${raw(badge)}</td>
          <td style="font-size:12px;">${d.message}</td>
        </tr>
      `;
    }).join('');

    openModal(
      '🎁 账号池每日打卡结果',
      html`
        <div class="wb-checkin-report-modal">
          <div class="wb-checkin-summary-cards">
            <div class="wb-stat-card">
              <span class="wb-stat-val">${report.total}</span>
              <span class="wb-stat-label">总账号数</span>
            </div>
            <div class="wb-stat-card">
              <span class="wb-stat-val" style="color:var(--success, #2e8b45);">${report.success}</span>
              <span class="wb-stat-label">本次领取</span>
            </div>
            <div class="wb-stat-card">
              <span class="wb-stat-val" style="color:#3b82f6;">${report.already_checked_in}</span>
              <span class="wb-stat-label">今日已打卡</span>
            </div>
            <div class="wb-stat-card">
              <span class="wb-stat-val" style="color:var(--error, #e53e3e);">${report.failed}</span>
              <span class="wb-stat-label">失败</span>
            </div>
          </div>

          <table class="wb-checkin-table">
            <thead>
              <tr>
                <th>账号</th>
                <th>状态</th>
                <th>详情</th>
              </tr>
            </thead>
            <tbody>
              ${raw(rows)}
            </tbody>
          </table>
        </div>
      `
    );

    toast(`打卡完成：${report.success} 个成功，${report.already_checked_in} 个已打卡`, 'ok');
  } catch (err) {
    toast(`每日打卡失败: ${err}`, 'error');
  } finally {
    if (btn) {
      btn.disabled = false;
      btn.textContent = '🎁 账号池一键打卡';
    }
  }
}
