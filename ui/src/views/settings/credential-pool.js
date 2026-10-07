/**
 * Identity pool (账号 / 密钥).
 *
 * The list, the default-identity selector, the per-identity actions and the
 * daily schedule. OAuth credentials and plain API keys are merged into one
 * list, so a single "设为默认" path serves both.
 */

import { $, html, raw } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { refreshStatus } from '../overview.js';
import { toast } from '../../components/toast.js';
import { formatPoints } from '../../lib/format.js';
import { startQrCodeLogin, setOAuthPoolRefresh } from './oauth-qr.js';
import { initRelogin, setReloginPoolRefresh } from './relogin.js';
import { triggerBatchCheckin } from './checkin.js';

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

    const credRows = creds.map((c) => {
      const [stateLabel, stateClass] = WB_STATE_LABELS[c.state] || ['未知', ''];
      const isDefault = effDefault === c.id;
      const stickyNote = c.sticky_sessions > 0
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
      return html`<div class="wb-credential-row${raw(isDefault ? ' wb-credential-row--default' : '')}">
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
          <button type="button" class="btn btn-small" data-toggle-cred="${c.id}" data-enabled="${c.enabled ? '1' : '0'}">
            ${c.enabled ? '禁用' : '启用'}
          </button>
          <button type="button" class="btn btn-small btn-danger" data-delete-cred="${c.id}">删除</button>
        </div>
      </div>`;
    }).join('');

    const keyRows = keys.map((k) => {
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
      return html`<div class="wb-credential-row${raw(isDefault ? ' wb-credential-row--default' : '')}">
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
    }).join('');

    listEl.innerHTML = credRows + keyRows;

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

/** Load the persisted daily-check-in schedule into its controls. */
export async function loadSchedule() {
  try {
    const prefs = await invoke('wb_preferences');
    if (!prefs) return;
    const enabled = $('#wb-checkin-enabled');
    const time = $('#wb-checkin-time');
    if (enabled) enabled.checked = Boolean(prefs.daily_checkin_enabled);
    if (time) time.value = prefs.daily_checkin_time || '09:00';
    const hint = $('#wb-schedule-hint');
    if (hint) {
      hint.textContent = prefs.last_checkin_run_date
        ? `上次定时打卡：${prefs.last_checkin_run_date}`
        : '';
    }
  } catch (e) {
    console.warn('loadSchedule error:', e);
  }
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
  $('#btn-wb-qrcode')?.addEventListener('click', startQrCodeLogin);
  $('#btn-wb-checkin-all')?.addEventListener('click', triggerBatchCheckin);

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
    const btn = $('#btn-wb-schedule-save');
    if (btn) {
      btn.disabled = true;
      btn.textContent = '保存中...';
    }
    try {
      const res = await invoke('wb_set_schedule', { enabled, time });
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
