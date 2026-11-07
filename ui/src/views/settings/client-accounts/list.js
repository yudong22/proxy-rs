/**
 * The access-account list and its row actions.
 *
 * Split out of `client-accounts.js` so the two dialogs and the list can each be
 * read on their own; this one owns the panel, they own the moment a key appears.
 */

import { $, html, raw } from '../../../core/dom.js';
import { invoke } from '../../../core/ipc.js';
import { confirmAction } from '../../../components/confirm.js';
import { toast } from '../../../components/toast.js';
import { KEY_HEADER, formatTimestamp } from './shared.js';
import { showRevealedKey } from './reveal-dialog.js';

/**
 * Repaint the access-account list from the backend.
 *
 * Exported because the settings view calls it when the tab is entered, and the
 * create flow calls it after the key has been shown.
 */
export async function renderClientAccounts() {
  const listEl = $('#client-account-list');
  if (!listEl) return;

  let accounts = [];
  try {
    const res = await invoke('client_accounts_list');
    accounts = res?.accounts || [];
  } catch (err) {
    listEl.innerHTML = html`<div class="hint err">读取访问账户失败: ${String(err)}</div>`;
    return;
  }

  if (!accounts.length) {
    listEl.innerHTML = html`
      <div class="hint">
        还没有访问账户。点击「＋ 添加访问账户」创建一个；启用入站鉴权后，
        调用方必须带上 <code>${KEY_HEADER}</code> 请求头才能访问 <code>/v1/*</code>。
      </div>
    `;
    return;
  }

  listEl.innerHTML = accounts.map((a) => {
    const enabled = Boolean(a.enabled);
    const statePill = enabled
      ? raw('<span class="badge-pill wb-state-pill wb-state-ok">已启用</span>')
      : raw('<span class="badge-pill wb-state-pill wb-state-disabled">已禁用</span>');
    return html`
      <div class="wb-credential-row" data-client-account-row="${a.id}">
        <div class="wb-credential-main">
          ${raw('<span class="badge-pill wb-type-pill wb-type-account">访问账户</span>')}
          <span class="wb-credential-label">${a.label || '(未命名)'}</span>
          ${statePill}
          <span class="wb-credential-id mono" title="key 前缀；完整 key 只在创建时显示一次">${a.key_prefix || '—'}</span>
          <span class="wb-credential-id">创建于 ${formatTimestamp(a.created_at_ms)}</span>
          <span class="wb-credential-id">最近使用 ${formatTimestamp(a.last_used_ms)}</span>
        </div>
        <div class="wb-credential-actions">
          <button type="button" class="btn btn-small"
            data-client-account-toggle="${a.id}" data-enabled="${enabled ? '1' : '0'}"
            title="${enabled ? '禁用后该 key 立即失效（无需重启）' : '启用后该 key 立即恢复可用'}">
            ${enabled ? '禁用' : '启用'}
          </button>
          <button type="button" class="btn btn-small"
            data-client-account-reissue="${a.id}" data-label="${a.label || ''}"
            title="重新签发一把新 key（旧 key 立即失效）。用于需要把 key 写进客户端配置的场合。">重新签发</button>
          <button type="button" class="btn btn-small btn-danger"
            data-client-account-delete="${a.id}" data-label="${a.label || ''}">删除</button>
        </div>
      </div>
    `;
  }).join('');

  for (const btn of listEl.querySelectorAll('[data-client-account-toggle]')) {
    btn.addEventListener('click', async () => {
      const id = btn.dataset.clientAccountToggle;
      const enabled = btn.dataset.enabled !== '1';
      btn.disabled = true;
      try {
        await invoke('client_account_toggle', { id, enabled });
        await renderClientAccounts();
        toast(enabled ? '已启用该访问账户' : '已禁用该访问账户', 'ok');
      } catch (err) {
        toast('切换失败: ' + err, 'error');
        btn.disabled = false;
      }
    });
  }

  for (const btn of listEl.querySelectorAll('[data-client-account-reissue]')) {
    btn.addEventListener('click', async () => {
      const id = btn.dataset.clientAccountReissue;
      const label = btn.dataset.label || id;
      const ok = await confirmAction(
        `重新签发「${label}」的 key？旧 key 会立即失效，用它的客户端需更新配置。`
          + '\n\n用于「写入客户端配置」：只有持有明文的账户才能被一键写入。',
        '重新签发 key',
      );
      if (!ok) return;
      btn.disabled = true;
      try {
        const res = await invoke('client_account_reissue', { id });
        await renderClientAccounts();
        if (res?.api_key) {
          showRevealedKey({
            label: res.account?.label || label,
            apiKey: res.api_key,
          });
        }
        toast('已重新签发 key', 'ok');
      } catch (err) {
        toast('重新签发失败: ' + err, 'error');
        btn.disabled = false;
      }
    });
  }

  for (const btn of listEl.querySelectorAll('[data-client-account-delete]')) {
    btn.addEventListener('click', async () => {
      const id = btn.dataset.clientAccountDelete;
      const label = btn.dataset.label || id;
      // Deleting a key that a client is still configured with breaks that
      // client, and the key cannot be recovered — so this asks first, like every
      // other destructive action in the app (window.confirm does not exist in
      // the Tauri webview).
      const ok = await confirmAction(
        `确定删除访问账户「${label}」？删除后该 key 立即失效，且无法恢复（明文只在创建时显示过）。`,
        '删除访问账户',
      );
      if (!ok) return;
      btn.disabled = true;
      try {
        await invoke('client_account_delete', { id });
        await renderClientAccounts();
        toast('已删除访问账户', 'ok');
      } catch (err) {
        toast('删除失败: ' + err, 'error');
        btn.disabled = false;
      }
    });
  }
}
