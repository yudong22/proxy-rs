/**
 * The identity pool list: rendering, the default-identity selector and the
 * per-row actions.
 *
 * OAuth credentials and plain API keys are merged into one ordered list here, so
 * a single "设为默认" path serves both. Split out of `credential-pool.js`; the
 * module is still reached through that file's re-export, so no caller changed.
 */

import { $, html, raw } from '../../../core/dom.js';
import { invoke } from '../../../core/ipc.js';
import { refreshStatus } from '../../overview.js';
import { toast } from '../../../components/toast.js';
import { formatPoints } from '../../../lib/format.js';
import { showWbStatus } from './status.js';
import { wireCredentialDrag } from './drag.js';

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
