/**
 * Identity pool (账号 / 密钥).
 *
 * The list, the default-identity selector, the per-identity actions and the
 * daily schedule. OAuth credentials and plain API keys are merged into one
 * list, so a single "设为默认" path serves both.
 */

import { $, html, raw } from '../../core/dom.js';
import { invoke, listen } from '../../core/ipc.js';
import { refreshStatus } from '../overview.js';
import { toast } from '../../components/toast.js';
import { formatPoints } from '../../lib/format.js';
import { startQrCodeLogin, setOAuthPoolRefresh } from './oauth-qr.js';
import { initRelogin, setReloginPoolRefresh } from './relogin.js';
import { triggerBatchCheckin } from './checkin.js';
import { overlay, modalShell } from '../../components/overlay.js';

/** Whether a growth run is in flight (drives the progress line). */
let growthRunActive = false;
/** Set once the run's summary is written, so late events cannot overwrite it. */
let growthRunFinished = false;
/** Recent progress lines, newest last. */
let growthProgressLines = [];

/**
 * Paint the growth progress box.
 *
 * `scroll` scrolls it into view — the run button lives in the toolbar above the
 * list, so a user who clicked there is not looking at this area yet. Only on
 * start: scrolling on every update would fight the user scrolling the log.
 *
 * @param {boolean} scroll
 */
function paintGrowthProgress(scroll) {
  const el = $('#wb-growth-progress');
  if (!el) return;
  el.hidden = false;
  el.textContent = growthProgressLines.join('\n');
  if (scroll) el.scrollIntoView({ block: 'nearest' });
}

/** State label → badge class, so a cooled credential reads differently. */
const WB_STATE_LABELS = {
  ok: ['正常', 'wb-state-ok'],
  cooldown: ['冷却中', 'wb-state-cooldown'],
  expired: ['已过期', 'wb-state-expired'],
  disabled: ['已禁用', 'wb-state-disabled'],
};

/** Repaint the credential list from the backend. */
export async function renderIdentityPool() {
  const listEl = $('#wb-identity-list');
  if (!listEl) return;
  try {
    const [credsRes, keysRes] = await Promise.all([
      invoke('wb_credentials_list'),
      invoke('api_keys_list'),
    ]);
    const creds = credsRes?.credentials || [];
    const keys = keysRes?.keys || [];
    // Credentials and keys are one ordered list in the GUI (a drag can put a key
    // between two accounts), but they come from two commands backed by two
    // files. `identity_order` is the interleaved order the backend will try them
    // in; render in exactly that order so the screen never disagrees with the
    // failover sequence.
    const identityOrder = credsRes?.identity_order || [];
    // The unified default identity in force (credential id, key id, "__none__",
    // or "" meaning first-usable fallback). Used to mark the "默认" badge.
    const defaultIdentityId = credsRes?.default_identity_id
      || keysRes?.default_identity_id
      || '';
    const effDefault = defaultIdentityId || proxyRsEffectiveDefault(creds, keys);

    const hasAny = creds.length || keys.length;

    // Keep the unified default-identity selector in sync. The first two options
    // (fallback, and "no account pool") are static; the rest are every identity
    // in the pool, so the user can pick any account or key as the default.
    const sel = $('#wb-default-identity-select');
    if (sel) {
      const options = [
        '<option value="">（默认：账号优先，否则密钥 / 单 Key）</option>',
        '<option value="__none__">不使用账号池（仅用密钥 / 单 Key）</option>',
      ];
      for (const c of creds) {
        const chosen = effDefault === c.id;
        options.push(html`<option value="${c.id}"${raw(chosen ? ' selected' : '')}>账号：${c.label || c.nickname || c.id}${c.enabled ? '' : '（已禁用）'}</option>`);
      }
      for (const k of keys) {
        const chosen = effDefault === k.id;
        options.push(html`<option value="${k.id}"${raw(chosen ? ' selected' : '')}>密钥：${k.label || k.id}${k.enabled ? '' : '（已禁用）'}</option>`);
      }
      sel.innerHTML = options.join('');
    }

    if (!hasAny) {
      listEl.innerHTML = html`<div class="hint">
        身份池为空。点击「📱 扫码添加账号」完成微信/QQ 扫码，或在上方「模型提供商」中粘贴上游 API Key 添加密钥。
      </div>`;
      return;
    }

    const credRowList = creds.map((c) => {
      const [stateLabel, stateClass] = WB_STATE_LABELS[c.state] || ['未知', ''];
      const isDefault = effDefault === c.id;
      // Only a relocated, still-healthy account is worth calling out: it is the
      // one serving sessions that are *not* on the default. A cooled/disabled
      // account that still holds bindings is not serving anyone, and the
      // default account is where sessions normally live, so labelling it
      // "sticky" read as if every account were sticky.
      const stickyNote = c.sticky_sessions > 0 && c.state === 'ok' && !isDefault
        ? raw(`<span class="wb-sticky-note">${c.sticky_sessions} 个会话粘滞</span>`)
        : '';
      const defaultBadge = isDefault
        ? raw('<span class="badge-pill wb-default-pill">默认</span>')
        : '';
      const typePill = raw('<span class="badge-pill wb-type-pill wb-type-account">账号</span>');
      const pointsChip = raw(
        `<span class="wb-points" title="剩余积分">💎 ${formatPoints(c.points)}</span>`,
      );
      const checkinNote = c.checked_in_today
        ? raw('<span class="wb-checkin-note">今日已打卡</span>')
        : '';
      // Two un-self-healing cases, and the row explains which one applies:
      // no refresh token at all, or a refresh the endpoint rejected (revoked).
      // Without the second the badge would miss an account that is permanently
      // stranded despite looking perfectly healthy.
      //
      // Its own class (not the state pill's) keeps "how healthy is it" and "does
      // it need you" as two distinguishable things. The reason goes in the
      // tooltip: it is the detail that used to live only in the live log.
      const reloginWhy = c.has_refresh_token
        ? '刷新登录态已被上游拒绝（refreshToken 可能已失效），只能重新登录'
        : '该账号没有 refreshToken，无法自动续期';
      const reloginNote = c.needs_relogin
        ? raw(`<span class="badge-pill wb-relogin-pill" title="${reloginWhy}${c.last_error ? `：${c.last_error}` : ''}；请点「重新绑定」">需重新绑定</span>`)
        : '';
      // The row is the drag handle. Only credentials are draggable: they are the
      // ordered failover list, whereas API keys are a separate store with its own
      // resolution rules, so dragging a key among accounts would imply an order
      // relationship that does not exist.
      return html`<div class="wb-credential-row wb-credential-row--draggable${raw(isDefault ? ' wb-credential-row--default' : '')}"
        data-ident-row="${c.id}"
        title="拖动左侧手柄可调整优先顺序：越靠上越先被使用，失败后按此顺序依次尝试">
        <span class="wb-drag-handle" data-drag-handle aria-hidden="true">⋮⋮</span>
        <div class="wb-credential-main">
          ${typePill}
          <span class="wb-credential-label">${c.label || c.nickname || c.id}</span>
          ${defaultBadge}
          <span class="badge-pill wb-state-pill ${stateClass}">${stateLabel}</span>
          ${reloginNote}
          ${pointsChip}
          ${checkinNote}
          ${stickyNote}
          <span class="wb-credential-id mono" title="${c.masked_token}">${c.id}</span>
        </div>
        <div class="wb-credential-actions">
          <button type="button" class="btn btn-small${raw(isDefault ? ' btn-active' : '')}"
            data-default="${c.id}" data-is-default="${isDefault ? '1' : '0'}"
            title="设为默认使用的身份">${isDefault ? '默认身份' : '设为默认'}</button>
          <button type="button" class="btn btn-small" data-relogin="${c.id}"
            title="重新扫码或粘贴登录态，覆盖此账号（保留标签、积分与默认身份）">重新绑定</button>
          <button type="button" class="btn btn-small" data-points="${c.id}"
            title="刷新此账号的剩余积分">刷新积分</button>
          <button type="button" class="btn btn-small" data-checkin="${c.id}" title="为此账号每日打卡领积分">打卡</button>
          <button type="button" class="btn btn-small" data-growth="${c.id}"
            title="为此账号自动完成成长任务并领取积分">增长</button>
          <button type="button" class="btn btn-small" data-toggle-cred="${c.id}" data-enabled="${c.enabled ? '1' : '0'}">
            ${c.enabled ? '禁用' : '启用'}
          </button>
          <button type="button" class="btn btn-small btn-danger" data-delete-cred="${c.id}">删除</button>
        </div>
      </div>`;
    });

    const keyRowList = keys.map((k) => {
      const isDefault = effDefault === k.id;
      const statePill = k.enabled
        ? raw('<span class="badge-pill wb-state-pill wb-state-ok">已启用</span>')
        : raw('<span class="badge-pill wb-state-pill wb-state-disabled">已禁用</span>');
      const defaultBadge = isDefault
        ? raw('<span class="badge-pill wb-default-pill">默认</span>')
        : '';
      const typePill = raw('<span class="badge-pill wb-type-pill wb-type-key">密钥</span>');
      const pointsChip = raw(
        `<span class="wb-points" title="剩余积分">💎 ${formatPoints(k.points)}</span>`,
      );
      return html`<div class="wb-credential-row wb-credential-row--draggable${raw(isDefault ? ' wb-credential-row--default' : '')}"
        data-ident-row="${k.id}"
        title="拖动左侧手柄可调整优先顺序：越靠上越先被使用，失败后按此顺序依次尝试">
        <span class="wb-drag-handle" data-drag-handle aria-hidden="true">⋮⋮</span>
        <div class="wb-credential-main">
          ${typePill}
          <span class="wb-credential-label">${k.label || k.id}</span>
          ${defaultBadge}
          ${statePill}
          ${pointsChip}
          <span class="wb-credential-id mono" title="${k.masked}">${k.id}</span>
        </div>
        <div class="wb-credential-actions">
          <button type="button" class="btn btn-small${raw(isDefault ? ' btn-active' : '')}"
            data-default="${k.id}" data-is-default="${isDefault ? '1' : '0'}"
            title="设为默认使用的身份">${isDefault ? '默认身份' : '设为默认'}</button>
          <button type="button" class="btn btn-small" data-toggle-key="${k.id}"
            data-enabled="${k.enabled ? '1' : '0'}">${k.enabled ? '禁用' : '启用'}</button>
          <button type="button" class="btn btn-small btn-danger" data-delete-key="${k.id}">删除</button>
        </div>
      </div>`;
    });

    // Build one row per identity, then emit them in `identity_order`. Anything
    // the order does not name (a brand-new import) goes after, preserving the
    // relative order it arrived in.
    const rowsById = new Map();
    creds.forEach((c, i) => rowsById.set(c.id, credRowList[i]));
    keys.forEach((k, i) => rowsById.set(k.id, keyRowList[i]));
    const orderedIds = [
      ...identityOrder.filter((id) => rowsById.has(id)),
      ...[...rowsById.keys()].filter((id) => !identityOrder.includes(id)),
    ];
    listEl.innerHTML = orderedIds.map((id) => rowsById.get(id)).join('');

    wireCredentialDrag(listEl);

    // Unified "set default" — selects the default identity regardless of type.
    for (const btn of listEl.querySelectorAll('[data-default]')) {
      btn.addEventListener('click', async () => {
        if (btn.dataset.isDefault === '1') return;
        btn.disabled = true;
        try {
          await invoke('wb_set_default_identity', { id: btn.dataset.default });
          showWbStatus('已设置默认身份，立即生效（无需重启）');
          await renderIdentityPool();
          // The overview's 当前账号/剩余积分 cards follow the identity in
          // force; without this they kept showing the previous account until
          // the next unrelated status push happened to fire.
          await refreshStatus();
        } catch (e) {
          showWbStatus('设置默认身份失败: ' + e, true);
          btn.disabled = false;
        }
      });
    }
    for (const btn of listEl.querySelectorAll('[data-points]')) {
      btn.addEventListener('click', async () => {
        // Refresh only this row. It used to call the pool-wide command and
        // ignore `data-points` entirely, so the button did something other than
        // what its own tooltip promised.
        const id = btn.dataset.points;
        btn.disabled = true;
        btn.textContent = '...';
        try {
          await invoke('wb_refresh_points_single', { id });
          await renderIdentityPool();
          await refreshStatus();
          showWbStatus(`已刷新 ${id} 的积分`);
        } catch (e) {
          showWbStatus('刷新积分失败: ' + e, true);
          btn.disabled = false;
          btn.textContent = '刷新积分';
        }
      });
    }
    for (const btn of listEl.querySelectorAll('[data-growth]')) {
    btn.addEventListener('click', async () => {
      const id = btn.dataset.growth;
      btn.disabled = true;
      btn.textContent = '...';
      try {
        const run = await invoke('wb_growth_run_single', { id, dryRun: false });
        await renderIdentityPool();
        await refreshStatus();
        const n = (run?.claimed || []).length;
        showWbStatus(
          n > 0
            ? `增长完成：领取 ${n} 个任务，获得 ${run.credits_earned} 积分`
            : '增长已运行：本次没有可领取的积分（详见日志）',
        );
      } catch (e) {
        showWbStatus('增长失败: ' + e, true);
        btn.disabled = false;
        btn.textContent = '增长';
      }
    });
  }

  for (const btn of listEl.querySelectorAll('[data-checkin]')) {
      btn.addEventListener('click', async () => {
        const id = btn.dataset.checkin;
        btn.disabled = true;
        btn.textContent = '...';
        try {
          const res = await invoke('wb_checkin_single', { id });
          const isOk = res.status === 'success' || res.status === 'already_checked_in';
          toast(`${res.label || id}: ${res.message}`, isOk ? 'ok' : 'error');
          await renderIdentityPool();
        } catch (e) {
          toast(`打卡失败: ${e}`, 'error');
        } finally {
          btn.disabled = false;
          btn.textContent = '打卡';
        }
      });
    }
    for (const btn of listEl.querySelectorAll('[data-toggle-cred]')) {
      btn.addEventListener('click', async () => {
        const id = btn.dataset.toggleCred;
        const enable = btn.dataset.enabled !== '1';
        try {
          await invoke('wb_credentials_toggle', { id, enabled: enable });
          await renderIdentityPool();
        } catch (e) {
          showWbStatus('操作失败: ' + e, true);
        }
      });
    }
    for (const btn of listEl.querySelectorAll('[data-delete-cred]')) {
      btn.addEventListener('click', async () => {
        const id = btn.dataset.deleteCred;
        try {
          await invoke('wb_credentials_delete', { id });
          await renderIdentityPool();
          showWbStatus('账号已删除，立即生效');
        } catch (e) {
          showWbStatus('删除失败: ' + e, true);
        }
      });
    }
    for (const btn of listEl.querySelectorAll('[data-toggle-key]')) {
      btn.addEventListener('click', async () => {
        const id = btn.dataset.toggleKey;
        const enable = btn.dataset.enabled !== '1';
        try {
          await invoke('api_keys_toggle', { id, enabled: enable });
          await renderIdentityPool();
        } catch (e) {
          showWbStatus('操作失败: ' + e, true);
        }
      });
    }
    for (const btn of listEl.querySelectorAll('[data-delete-key]')) {
      btn.addEventListener('click', async () => {
        try {
          await invoke('api_keys_delete', { id: btn.dataset.deleteKey });
          await renderIdentityPool();
          showWbStatus('密钥已删除');
        } catch (e) {
          showWbStatus('删除失败: ' + e, true);
        }
      });
    }
  } catch (e) {
    listEl.innerHTML = html`<div class="hint">身份池加载失败: ${String(e)}</div>`;
  }
}

/**
 * Fallback for the "default" badge when the unified selector hasn't been set:
 * first usable credential, else first enabled key. Mirrors the proxy's request
 * path resolution, so the badge matches what actually serves requests.
 */
function proxyRsEffectiveDefault(creds, keys) {
  const now = Date.now();
  const usable = creds.find((c) => c.enabled && c.state === 'ok');
  if (usable) return usable.id;
  const key = keys.find((k) => k.enabled);
  return key ? key.id : '';
}

export function showWbStatus(message, isError = false) {
  const el = $('#wb-status');
  if (!el) return;
  el.textContent = message;
  el.classList.toggle('error', isError);
}

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
async function refreshAutomationSummary() {
  try {
    const prefs = await invoke('wb_preferences');
    if (prefs) paintAutomationHints(prefs);
  } catch (e) {
    console.warn('refreshAutomationSummary error:', e);
  }
}

/** Wire the automation dialog: open, save, and close. */
function initAutomationDialog() {
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

/** Wire the credential-pool controls and paint the initial list. */
export function initCredentialPool() {
  // The QR flow repaints the list through this hook (see oauth-qr.js), and the
  // re-bind dialog does the same through its own (see relogin.js). Both are
  // injected rather than imported back, so neither module has to depend on this
  // one — importing this file from them would close a cycle.
  setOAuthPoolRefresh(renderIdentityPool);
  setReloginPoolRefresh(renderIdentityPool);
  initRelogin();
  initAutomationDialog();
  // Paint the summary immediately: the page must state whether anything runs
  // unattended before the user opens the dialog.
  void refreshAutomationSummary();
  $('#btn-wb-qrcode')?.addEventListener('click', startQrCodeLogin);
  $('#btn-wb-checkin-all')?.addEventListener('click', triggerBatchCheckin);

  // 增长积分：跑一遍成长任务并领奖。结果里「跳过」和「未推进」必须显式告知，
  // 否则用户会以为"跑了=全拿到了"。
  // Growth progress stream. Bound once per view init; the handler only updates
  // the status line, so re-rendering the pool does not lose the listener.
  // `initCredentialPool` is called synchronously from the view router, so the
  // subscription is fired and forgotten rather than awaited: binding a listener
  // must not delay first paint of the pool.
  listen('growth-progress', (p) => {
    if (!p || !p.phase) return;
    // Accept events whenever a run has been started and its summary has not yet
    // been written. Gating on a strict "still in flight" flag lost the tail of
    // very fast runs: with the mock backend (and any run that finishes inside a
    // single tick) `growthRunActive` was already false by the time the final
    // events arrived, so the box showed only "开始运行成长任务…".
    if (growthRunFinished) return;
    const lines = growthProgressLines;
    if (p.phase === 'account_start') {
      lines.push(`⏳ 第 ${p.index}/${p.total} 个账号：${p.label}`);
    } else if (p.phase === 'task_claimed') {
      lines.push(`  ✓ ${p.code} +${p.credit} 分`);
    } else if (p.phase === 'task_stalled') {
      lines.push(`  · ${p.code} 未推进，已跳过`);
    } else if (p.phase === 'account_done') {
      lines.push(
        p.claimed > 0
          ? `  → ${p.label}：${p.claimed} 个任务 +${p.credits} 分`
          : `  → ${p.label}：本次无可领取`,
      );
    } else if (p.phase === 'finished') {
      lines.push(`全部完成：${p.claimed} 个任务 +${p.credits} 分`);
    }
    // Keep only the tail: a long pool run would otherwise fill the line.
    if (lines.length > 6) lines.splice(0, lines.length - 6);
    paintGrowthProgress(false);
  });

  $('#btn-wb-growth')?.addEventListener('click', async () => {
    const btn = $('#btn-wb-growth');
    if (btn) {
      btn.disabled = true;
      btn.textContent = '⏳ 增长中...';
    }
    // Progress state: the event handler appends lines while `growthRunActive`.
    growthRunActive = true;
    growthRunFinished = false;
    growthProgressLines = ['开始运行成长任务…'];
    paintGrowthProgress(true);

    // Heartbeat: a whole-pool run staggers accounts 2–5 s apart, so even with
    // events there can be multi-second gaps with nothing new to say. Without a
    // ticking clock the UI looks frozen exactly when the user is watching it.
    const startedAt = Date.now();
    const heartbeat = setInterval(() => {
      if (!growthRunActive) return;
      const secs = Math.floor((Date.now() - startedAt) / 1000);
      const el = $('#wb-growth-progress');
      if (!el) return;
      const last = growthProgressLines[growthProgressLines.length - 1] || '';
      el.textContent = `${growthProgressLines.join('\n')}\n… 运行中 ${secs}s`;
      // Keep the box where the user's eye is: the button is in the toolbar above.
      if (el.scrollIntoView && secs % 5 === 0 && last) {
        el.scrollIntoView({ block: 'nearest' });
      }
    }, 1000);
    try {
      const report = await invoke('wb_growth_run', { dryRun: false });
      await renderIdentityPool();
      await refreshStatus();

      const claimed = report?.total_claimed ?? 0;
      const credits = report?.total_credits ?? 0;
      const manual = (report?.accounts || [])
        .flatMap((a) => a.skipped || [])
        .filter((s) => s.hint);
      const stalled = (report?.accounts || []).reduce(
        (n, a) => n + (a.stalled?.length || 0),
        0,
      );

      // One consolidated message. Previously two `showWbStatus` calls ran back to
      // back and the second overwrote the first, so a successful run could end
      // up showing only "有 N 个任务需要你手动完成" and the earned credits were
      // never mentioned.
      const parts = [];
      if (claimed > 0) {
        parts.push(`领取 ${claimed} 个任务，获得 ${credits} 积分`);
      } else if (stalled === 0 && manual.length === 0) {
        // Nothing left to automate and nothing failed: this is the normal
        // "all claimed" state, not an error. Saying "没有可领取的积分" alone read
        // as a failure to users.
        parts.push('所有可自动化的任务都已领完');
      } else {
        parts.push('本次没有新领取的积分');
      }
      if (stalled > 0) parts.push(`${stalled} 个任务上报后未推进`);
      if (manual.length > 0) parts.push(`${manual.length} 个任务需你手动完成`);
      showWbStatus(`增长任务完成：${parts.join('；')}`);

      // A toast makes the outcome visible even if the status line is off-screen.
      // Only a real failure gets the error tone; "already all claimed" is a
      // success with zero delta.
      const allDone = claimed === 0 && stalled === 0;
      toast(
        claimed > 0
          ? `增长完成：+${credits} 积分（${claimed} 个任务）`
          : allDone
            ? '增长完成：可自动化的任务都已领完'
            : '增长已运行：本次没有新领取的积分',
        'ok',
      );
    } catch (e) {
      showWbStatus('增长任务失败: ' + e, true);
      toast('增长任务失败: ' + e, 'error');
    } finally {
      growthRunActive = false;
      clearInterval(heartbeat);
      // Let the last queued events land before freezing the box; the backend
      // emits `finished` just before the command resolves, and Tauri delivers it
      // on the same event loop.
      await new Promise((r) => setTimeout(r, 150));
      growthRunFinished = true;
      paintGrowthProgress(false);
      if (btn) {
        btn.disabled = false;
        btn.textContent = '📈 增长积分';
      }
      // Leave the progress line visible briefly, then clear it so a later run
      // starts clean instead of appending to stale output.
      setTimeout(() => {
        const el = $('#wb-growth-progress');
        if (el && !growthRunActive) {
          el.textContent = growthProgressLines.join('\n');
        }
      }, 0);
    }
  });

  $('#btn-wb-refresh-points')?.addEventListener('click', async () => {
    const btn = $('#btn-wb-refresh-points');
    if (btn) {
      btn.disabled = true;
      btn.textContent = '⏳ 刷新中...';
    }
    try {
      await invoke('wb_refresh_points');
      await renderIdentityPool();
      showWbStatus('已刷新全部身份的剩余积分（账号 + 密钥）');
    } catch (e) {
      showWbStatus('刷新积分失败: ' + e, true);
    } finally {
      if (btn) {
        btn.disabled = false;
        btn.textContent = '💎 刷新全部积分';
      }
    }
  });

  $('#btn-wb-schedule-save')?.addEventListener('click', async () => {
    const enabled = Boolean($('#wb-checkin-enabled')?.checked);
    const time = $('#wb-checkin-time')?.value || '09:00';
    const growth = Boolean($('#wb-growth-enabled')?.checked);
    const btn = $('#btn-wb-schedule-save');
    if (btn) {
      btn.disabled = true;
      btn.textContent = '保存中...';
    }
    try {
      const res = await invoke('wb_set_schedule', { enabled, time, growthEnabled: growth });
      const hint = $('#wb-schedule-hint');
      if (hint) {
        hint.textContent = res.daily_checkin_enabled
          ? `已开启：每天 ${res.daily_checkin_time} 自动打卡（应用启动时若当天未打卡会立即补打卡）`
          : '已关闭定时打卡';
      }
      toast(
        res.daily_checkin_enabled ? `已开启每日 ${res.daily_checkin_time} 定时打卡` : '已关闭定时打卡',
        'ok',
      );
    } catch (e) {
      showWbStatus('保存定时打卡失败: ' + e, true);
    } finally {
      if (btn) {
        btn.disabled = false;
        btn.textContent = '保存定时';
      }
    }
  });

  // Key pool: visibility toggle mirrors the provider API-key field.
  $('#btn-wb-key-visibility')?.addEventListener('click', () => {
    const input = $('#wb-new-key');
    const btn = $('#btn-wb-key-visibility');
    const show = input?.type === 'password';
    if (input) input.type = show ? 'text' : 'password';
    if (btn) btn.textContent = show ? '隐藏' : '显示';
  });

  $('#btn-wb-key-add')?.addEventListener('click', async () => {
    const key = $('#wb-new-key')?.value.trim();
    if (!key) {
      showWbStatus('请先粘贴上游 API Key', true);
      return;
    }
    try {
      await invoke('api_keys_add', { key, label: $('#wb-new-key-label')?.value || '' });
      if ($('#wb-new-key')) $('#wb-new-key').value = '';
      if ($('#wb-new-key-label')) $('#wb-new-key-label').value = '';
      showWbStatus('密钥已加入身份池');
      await renderIdentityPool();
    } catch (e) {
      showWbStatus('添加失败: ' + e, true);
    }
  });

  // Unified default-identity selector: choose a credential, a key, or "no
  // account pool" (`__none__`). Picking hot-applies via the backend, so the
  // account/key switch takes effect without a restart.
  $('#wb-default-identity-select')?.addEventListener('change', async (e) => {
    const id = e.target.value || '';
    try {
      await invoke('wb_set_default_identity', { id });
      showWbStatus(
        id === '__none__'
          ? '已切换：不使用账号池，仅用密钥 / 单 Key（立即生效）'
          : id
            ? '已设置默认身份，立即生效（无需重启）'
            : '已清除默认身份，回退到「账号优先，否则密钥 / 单 Key」',
      );
      await renderIdentityPool();
      await refreshStatus();
    } catch (err) {
      showWbStatus('设置默认身份失败: ' + err, true);
    }
  });

  $('#btn-wb-sticky-reset')?.addEventListener('click', async () => {
    try {
      await invoke('wb_sticky_reset');
      showWbStatus('已重置会话粘滞');
      await renderIdentityPool();
    } catch (e) {
      showWbStatus('重置失败: ' + e, true);
    }
  });

  renderIdentityPool();
  loadSchedule();
}

/**
 * Make the identity rows reorderable by dragging, and persist the result.
 *
 * The order is not cosmetic: the pool tries identities top-down, so this list
 * is the failover priority. Dragging commits immediately (no 保存 button) — the
 * row order on screen and the stored order must never disagree, and a
 * pending-edit state would let the user navigate away believing it was saved.
 *
 * **Pointer events, not HTML5 drag-and-drop.** `dragstart`/`dragover`/`drop`
 * never fired inside the packaged app: Tauri's webview handles native drag and
 * drop itself (`dragDropEnabled`, on by default), so the DOM never sees the
 * events. A headless-Chrome harness cannot catch that class of bug — it passed
 * there and did nothing in the real window. Pointer events are generated by the
 * webview itself and are unaffected by the native drag layer.
 *
 * @param {HTMLElement} listEl  container holding the rows
 */
function wireCredentialDrag(listEl) {
  const rows = () => [...listEl.querySelectorAll('[data-ident-row]')];
  let drag = null;

  /** Row under the pointer, by hit-testing the container's children. */
  const rowAt = (clientY) => {
    for (const r of rows()) {
      const box = r.getBoundingClientRect();
      if (clientY >= box.top && clientY <= box.bottom) return r;
    }
    return null;
  };

  const cleanup = () => {
    for (const r of rows()) {
      r.classList.remove('wb-credential-row--dragging', 'wb-credential-row--drop-target');
    }
    document.body.classList.remove('wb-dragging-ident');
    drag = null;
  };

  for (const row of rows()) {
    const handle = row.querySelector('[data-drag-handle]');
    // Only the grip starts a drag. Starting from anywhere in the row would
    // swallow clicks on the row's own buttons, which is exactly what a user
    // aiming at 重新绑定 must not have happen.
    if (!handle) continue;

    handle.addEventListener('pointerdown', (e) => {
      if (e.button !== 0) return;
      e.preventDefault();
      drag = { id: row.dataset.identRow, moved: false };
      // Pointer capture keeps the events coming to the handle even as the
      // pointer travels over other rows.
      handle.setPointerCapture?.(e.pointerId);
      row.classList.add('wb-credential-row--dragging');
      document.body.classList.add('wb-dragging-ident');
    });

    handle.addEventListener('pointermove', (e) => {
      if (!drag) return;
      drag.moved = true;
      const over = rowAt(e.clientY);
      for (const r of rows()) {
        r.classList.toggle(
          'wb-credential-row--drop-target',
          Boolean(over) && r === over && r.dataset.identRow !== drag.id,
        );
      }
    });

    const finish = async (e) => {
      if (!drag) return;
      const sourceId = drag.id;
      const moved = drag.moved;
      const over = rowAt(e.clientY);
      const targetId = over?.dataset.identRow;
      cleanup();
      if (!moved || !targetId || targetId === sourceId) return;

      // Insert the dragged row *at* the target's position, keeping every other
      // row's relative order. Recomputed from the live DOM so a drag after an
      // earlier reorder still reflects what the user sees.
      const next = rows().map((r) => r.dataset.identRow);
      const from = next.indexOf(sourceId);
      const to = next.indexOf(targetId);
      if (from < 0 || to < 0) return;
      next.splice(to, 0, ...next.splice(from, 1));

      try {
        await invoke('wb_reorder_identities', { ids: next });
        showWbStatus('已调整优先顺序：越靠上越先被使用');
        // Repaint from the backend rather than reordering the DOM by hand: the
        // stored order is the source of truth, and a repaint also refreshes the
        // 默认 badge, which follows "first usable" when no explicit default is
        // set — reordering can therefore change which identity shows as 默认.
        await renderIdentityPool();
      } catch (err) {
        showWbStatus('调整顺序失败: ' + err, true);
        // Put the DOM back where the backend says it is; leaving the visual
        // order ahead of the stored one is the exact drift this guards against.
        await renderIdentityPool();
      }
    };

    handle.addEventListener('pointerup', finish);
    handle.addEventListener('pointercancel', () => cleanup());
  }
}
