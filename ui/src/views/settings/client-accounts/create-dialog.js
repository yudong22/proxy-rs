/**
 * The "add access account" dialog.
 *
 * Split out of `client-accounts.js`: creating an account and revealing the key it
 * returns are two dialogs with different lifetimes — this one is a plain form,
 * the reveal is a one-shot disclosure — and keeping them apart is what keeps the
 * "shown exactly once" rule visible.
 */

import { $, html } from '../../../core/dom.js';
import { invoke } from '../../../core/ipc.js';
import { overlay, modalShell } from '../../../components/overlay.js';
import { toast } from '../../../components/toast.js';
import { showRevealedKey } from './reveal-dialog.js';

/** Create dialog (label entry). */
const CREATE_ID = 'client-account-create-overlay';
const CREATE_TITLE_ID = 'client-account-create-title';
const CREATE_BODY_ID = 'client-account-create-body';

/** The create dialog, built on first use. */
function ensureCreateDialog() {
  return overlay(CREATE_ID, () => modalShell({
    id: CREATE_ID,
    title: '添加访问账户',
    titleId: CREATE_TITLE_ID,
    bodyId: CREATE_BODY_ID,
    footer: `
      <button type="button" class="btn btn-small" data-close>取消</button>
      <button type="button" class="btn btn-small btn-primary" id="btn-client-account-create-ok">创建</button>
    `,
  }));
}

/** Open the create dialog and create the account on confirm. */
export function openCreateDialog() {
  const el = ensureCreateDialog();
  const body = $(`#${CREATE_BODY_ID}`);
  if (body) {
    body.innerHTML = html`
      <div class="wb-transfer-modal">
        <p class="hint">
          访问账户用于<b>别人访问你的网关</b>时证明身份（入站鉴权），
          与「身份池」里<b>网关访问上游</b>的账号/密钥方向相反，互不影响。
        </p>
        <div class="form-group">
          <label for="client-account-new-label">备注名</label>
          <input type="text" id="client-account-new-label" class="log-input"
            placeholder="例如 家里的笔记本 / CI" autocomplete="off">
        </div>
        <p class="hint">
          创建后会立即显示一次完整 key，之后只能看到前缀。请准备好复制。
        </p>
      </div>
    `;
  }

  const input = $('#client-account-new-label');
  if (input) {
    input.value = '';
    // The body is rebuilt on every open, so this listener belongs to the freshly
    // created element — binding it in `initCreateDialog` would attach it to the
    // input of the first open only, and Enter would silently stop working after
    // the dialog was closed once.
    input.addEventListener('keydown', (e) => {
      if (e.key === 'Enter') {
        e.preventDefault();
        $('#btn-client-account-create-ok')?.click();
      }
    });
    input.focus();
  }

  el.classList.add('active');
}

/** Wire the create dialog's own controls, once. */
export function initCreateDialog() {
  const el = ensureCreateDialog();
  if (!el) return;
  const close = () => el.classList.remove('active');

  el.addEventListener('click', (e) => {
    if (e.target === el || e.target.closest('[data-close]')) close();
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && el.classList.contains('active')) close();
  });

  $('#btn-client-account-create-ok')?.addEventListener('click', async () => {
    const btn = $('#btn-client-account-create-ok');
    const label = ($('#client-account-new-label')?.value || '').trim();
    if (btn) btn.disabled = true;
    try {
      const res = await invoke('client_account_create', { label });
      if (!res?.api_key) throw new Error('后端未返回访问 key');
      close();
      // The reveal dialog is the only chance to read the key, so it is opened
      // before the list repaints — a repaint would otherwise put the new row in
      // view and invite the user to move on without copying anything.
      showRevealedKey({ label: res.label || label, apiKey: res.api_key });
    } catch (err) {
      toast('创建访问账户失败: ' + err, 'error');
    } finally {
      // Always re-enable: the dialog element is cached and reused, so a button
      // left disabled by a failure (or by a success) would make the next open
      // silently unclickable.
      if (btn) btn.disabled = false;
    }
  });
}
