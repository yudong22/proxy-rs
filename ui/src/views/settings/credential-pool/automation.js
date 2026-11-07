/**
 * The automation dialog: 每日定时打卡 and 自动跑成长任务 in one place.
 *
 * Split out of `credential-pool.js`. The two switches are one decision (what
 * this machine does unattended), and the dialog plus its always-visible summary
 * line is a self-contained piece of UI.
 */

import { $, html } from '../../../core/dom.js';
import { invoke } from '../../../core/ipc.js';
import { refreshStatus } from '../../overview.js';
import { toast } from '../../../components/toast.js';
import { overlay, modalShell } from '../../../components/overlay.js';

/**
 * Refresh the automation state shown in the page.
 *
 * The switches themselves now live in the lazy dialog, so this only repaints the
 * always-visible summary line — see [`paintAutomationHints`]. Kept as a named
 * export because the settings view calls it after any pool mutation that could
 * change these preferences.
 */
export async function loadSchedule() {
  await refreshAutomationSummary();
}

/** Id of the unified automation dialog. */
const AUTOMATION_ID = 'wb-automation-overlay';

/**
 * The one dialog holding every "runs by itself" switch.
 *
 * Two separate inline rows were the problem: 每日定时打卡 and 自动跑成长任务 are
 * the same decision (what this machine does unattended), yet they lived in
 * different parts of the section with no shared save, so it was easy to switch
 * one on and forget the other. One dialog makes the set visible at once.
 *
 * Built lazily through `components/overlay.js` like every other dialog here, so
 * the markup cannot drift from its handlers.
 */
function ensureAutomationDialog() {
  return overlay(AUTOMATION_ID, () => modalShell({
    id: AUTOMATION_ID,
    title: '自动化设置',
    titleId: 'wb-automation-title',
    bodyId: 'wb-automation-body',
    footer: `
      <button type="button" class="btn" data-close>取消</button>
      <button type="button" class="btn btn-primary" id="btn-wb-automation-save">保存</button>
    `,
  }));
}

/**
 * Paint the dialog's current values from stored preferences.
 *
 * Reads the same `wb_preferences` the inline rows used, so no new IPC surface is
 * introduced for a pure move.
 */
async function paintAutomationDialog() {
  const prefs = await invoke('wb_preferences');
  if (!prefs) return;
  const body = $('#wb-automation-body');
  if (!body) return;
  body.innerHTML = html`
    <p class="hint">
      这两项都<b>只在本机后台运行</b>：退出 Proxy RS 即停止，重新打开后按各自节奏补跑。
      与 DSH 的会话定时无关。
    </p>

    <div class="wb-schedule-row">
      <label class="wb-checkbox-inline">
        <input type="checkbox" id="wb-checkin-enabled">
        <span>每日定时打卡</span>
      </label>
      <input type="time" id="wb-checkin-time" class="log-input wb-time-input" value="${prefs.daily_checkin_time || '09:00'}">
      <span class="wb-schedule-hint" id="wb-schedule-hint"></span>
    </div>

    <div class="wb-schedule-row">
      <label class="wb-checkbox-inline">
        <input type="checkbox" id="wb-growth-enabled">
        <span>自动跑成长任务</span>
      </label>
      <span class="wb-schedule-hint" id="wb-growth-hint"></span>
    </div>

    <p class="hint">
      手动运行不占用定时槽位：手动补跑一次后，当天的定时批次仍会照常执行。
    </p>
  `;
  const enabled = $('#wb-checkin-enabled');
  const time = $('#wb-checkin-time');
  if (enabled) enabled.checked = Boolean(prefs.daily_checkin_enabled);
  if (time) time.value = prefs.daily_checkin_time || '09:00';
  paintAutomationHints(prefs);
}

/**
 * Paint both hint lines and the always-visible summary.
 *
 * The summary is the load-bearing part: closing a dialog hides the switches, so
 * without a line in the page an agent running unattended becomes invisible — the
 * exact failure this dialog must not introduce.
 *
 * @param {object} prefs - `wb_preferences` payload.
 */
function paintAutomationHints(prefs) {
  const checkinHint = $('#wb-schedule-hint');
  if (checkinHint) {
    checkinHint.textContent = prefs.last_checkin_run_date
      ? `上次定时打卡：${prefs.last_checkin_run_date}`
      : '';
  }
  const growthHint = $('#wb-growth-hint');
  if (growthHint) {
    growthHint.textContent = prefs.growth_enabled
      ? '已开启：每天自动跑一次成长任务；23:00–08:00 额外跑「夜猫子」'
      : '关闭中：只在你点「增长积分」时运行';
  }
  const summary = $('#wb-automation-summary');
  if (summary) {
    const on = [];
    if (prefs.daily_checkin_enabled) {
      on.push(`每日打卡 ${prefs.daily_checkin_time || '09:00'}`);
    }
    if (prefs.growth_enabled) on.push('成长任务');
    // Name what is running, not merely that something is: "已开启 2 项" tells a
    // reader nothing about which background job will touch their accounts.
    summary.textContent = on.length > 0
      ? `已开启：${on.join(' · ')}`
      : '两项均关闭（不会自动运行）';
    summary.classList.toggle('is-on', on.length > 0);
  }
}

/**
 * Re-read preferences and repaint only the summary line.
 *
 * Split from [`paintAutomationDialog`] so the page can refresh its one visible
 * state without building the dialog (which is lazy) or touching the switches.
 */
export async function refreshAutomationSummary() {
  try {
    const prefs = await invoke('wb_preferences');
    if (prefs) paintAutomationHints(prefs);
  } catch (e) {
    console.warn('refreshAutomationSummary error:', e);
  }
}

/** Wire the automation dialog: open, save, and close. */
export function initAutomationDialog() {
  const el = ensureAutomationDialog();
  if (!el) return;
  const close = () => el.classList.remove('active');

  $('#btn-wb-automation')?.addEventListener('click', async () => {
    await paintAutomationDialog();
    el.classList.add('active');
  });

  // The dialog reuses the shared modal classes, whose close button and backdrop
  // behaviour are already wired by other views; binding them here keeps the
  // dialog self-contained instead of depending on another view's init order.
  el.addEventListener('click', (e) => {
    if (e.target === el || e.target.closest('[data-close]')) close();
  });

  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && el.classList.contains('active')) close();
  });

  $('#btn-wb-automation-save')?.addEventListener('click', async () => {
    const btn = $('#btn-wb-automation-save');
    const enabled = Boolean($('#wb-checkin-enabled')?.checked);
    const time = $('#wb-checkin-time')?.value || '09:00';
    const growth = Boolean($('#wb-growth-enabled')?.checked);
    if (btn) btn.disabled = true;
    try {
      // One call for both switches — the backend already folds them into a
      // single write (`wb_set_schedule`'s optional `growth_enabled`), so the two
      // cannot end up half-saved.
      const prefs = await invoke('wb_set_schedule', { enabled, time, growthEnabled: growth });
      paintAutomationHints(prefs || {});
      close();
      toast('自动化设置已保存', 'ok');
      refreshStatus();
    } catch (e) {
      toast(`保存自动化设置失败: ${e}`, 'error');
    } finally {
      if (btn) btn.disabled = false;
    }
  });
}
