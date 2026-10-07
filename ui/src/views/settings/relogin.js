/**
 * Re-binding one account to a fresh login state.
 *
 * An account whose refresh token is gone (or has been rejected) cannot renew
 * itself — the only way back is a new login. This dialog is that path, and it is
 * deliberately *not* the same as "add account": it targets one existing row and
 * overwrites it in place.
 *
 * Two things make it an update rather than a duplicate:
 *
 *   * the backend matches by account identity (uid), not by access token, so a
 *     refreshed or re-issued token still lands on the same account;
 *   * the stored id is preserved, so the configured default identity, the
 *     request-log history and any sticky session keep pointing at this account.
 *
 * Scanning the QR code achieves the same thing (the OAuth poll also upserts by
 * identity), so this dialog offers pasting and points at the QR button instead of
 * duplicating that flow.
 */

import { $, html } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { openModal, closeModal } from '../../components/modal.js';
import { toast } from '../../components/toast.js';
import { showWbStatus } from './credential-pool.js';

/** The account currently being re-bound, and the callback that repaints after. */
let rebindTarget = null;
let repaint = async () => {};

/** The module that owns the list injects its repaint here (avoids a cycle). */
export function setReloginPoolRefresh(fn) {
  repaint = fn;
}

/**
 * Open the re-bind dialog for one account.
 *
 * @param {{id: string, label?: string, nickname?: string}} account
 */
export function openReloginDialog(account) {
  rebindTarget = account;
  const name = account.label || account.nickname || account.id;

  openModal('🔁 重新绑定登录态', '');
  const body = $('#modal-req-body');
  if (body) {
    body.innerHTML = html`
      <div class="wb-transfer-modal">
        <p class="wb-transfer-lead">
          为账号 <b>${name}</b>（<span class="mono">${account.id}</span>）重新绑定登录态。
          绑定成功后<b>沿用同一条记录</b>：默认身份、积分与请求日志都保持不变，不会新增重复账号。
        </p>

        <div class="wb-transfer-warning">
          什么时候需要重新绑定：该账号没有 refreshToken，或刷新接口已拒绝它——
          此时只有一次新的登录能恢复，代理无法自行续期。
        </div>

        <div class="form-group">
          <label for="wb-relogin-text">粘贴新的登录态 JSON</label>
          <textarea id="wb-relogin-text" class="log-input wb-transfer-textarea"
            placeholder="粘贴桌面端登录态（.info / ~/.codebuddy-session.json）或仅含 accessToken / refreshToken 的 JSON"
            spellcheck="false"></textarea>
        </div>

        <p class="hint">
          也可以用「📱 扫码添加账号」：扫码同一个账号时，后端按账号身份（uid）匹配，
          同样会覆盖这条记录而不是新增一条。
        </p>

        <div class="hint" id="wb-relogin-result"></div>
      </div>
    `;
  }

  const footer = $('#request-modal-overlay .modal-footer');
  if (footer) {
    footer.innerHTML = `
      <button type="button" class="btn btn-small" data-wb-relogin-cancel>取消</button>
      <button type="button" class="btn btn-small btn-primary" id="btn-wb-relogin-run">重新绑定</button>
    `;
    footer.querySelector('[data-wb-relogin-cancel]')?.addEventListener('click', closeModal);
    $('#btn-wb-relogin-run')?.addEventListener('click', runRelogin);
  }
}

/** Submit the pasted login state for the targeted account. */
async function runRelogin() {
  const btn = $('#btn-wb-relogin-run');
  const result = $('#wb-relogin-result');
  const text = $('#wb-relogin-text')?.value || '';
  const target = rebindTarget;

  if (!target) return;
  if (!text.trim()) {
    if (result) {
      result.textContent = '请先粘贴登录态 JSON（或用「扫码添加账号」）';
      result.classList.add('error');
    }
    return;
  }

  if (btn) {
    btn.disabled = true;
    btn.textContent = '绑定中...';
  }
  try {
    const saved = await invoke('wb_relogin', { body: { id: target.id, login_state: text } });
    closeModal();
    await repaint();
    toast(`账号「${saved?.label || target.id}」已重新绑定`, 'ok');
    showWbStatus(`账号 ${saved?.label || target.id} 登录态已更新`);
  } catch (e) {
    if (result) {
      result.textContent = `重新绑定失败: ${e}`;
      result.classList.add('error');
    }
    if (btn) {
      btn.disabled = false;
      btn.textContent = '重新绑定';
    }
  }
}

/** Wire the list's 重新绑定 buttons. Called once, from credential-pool.js. */
export function initRelogin() {
  const list = $('#wb-identity-list');
  if (!list) return;
  list.addEventListener('click', (e) => {
    const btn = e.target.closest('[data-relogin]');
    if (!btn) return;
    // The row's own label is what the user recognises; read it rather than
    // looking the account up again, so the dialog opens instantly.
    const row = btn.closest('.wb-credential-row');
    openReloginDialog({
      id: btn.dataset.relogin,
      label: row?.querySelector('.wb-credential-label')?.textContent?.trim() || '',
    });
  });
}
