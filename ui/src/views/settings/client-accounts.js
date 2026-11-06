/**
 * Client access accounts (入站凭据).
 *
 * ## Why this is not 身份池
 *
 * 身份池 (credential-pool.js) holds **outbound** credentials: the identities the
 * gateway presents *to* the upstream. This module holds the opposite thing —
 * **inbound** keys that callers must present *to* the gateway. Both exist, both
 * are lists of secrets, and confusing them would either leak a caller's key to
 * the upstream or let an unauthorised caller spend the pool's quota. So the two
 * live in separate files, separate stores and separate sections, and the panel
 * says which direction it is in its first line.
 *
 * ## The plaintext key is shown exactly once
 *
 * `client_account_create` returns the raw key on creation and never again; the
 * backend only stores a hash. The dialog that shows it is therefore the only
 * place a user can ever obtain it, which is why it states that in the body
 * rather than in a tooltip.
 */

import { $, html, raw } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { overlay, modalShell } from '../../components/overlay.js';
import { confirmAction } from '../../components/confirm.js';
import { toast } from '../../components/toast.js';

/** Create dialog (label entry). */
const CREATE_ID = 'client-account-create-overlay';
const CREATE_TITLE_ID = 'client-account-create-title';
const CREATE_BODY_ID = 'client-account-create-body';

/** One-time plaintext reveal. */
const REVEAL_ID = 'client-account-reveal-overlay';
const REVEAL_TITLE_ID = 'client-account-reveal-title';
const REVEAL_BODY_ID = 'client-account-reveal-body';

/** The request header callers must send. Kept in one constant: it appears in the
 *  reveal dialog, the empty-state help and the row tooltip. */
const KEY_HEADER = 'x-proxy-rs-key';

/**
 * Render a `created_at_ms` / `last_used_ms` value as local wall time.
 *
 * Milliseconds since the epoch arrive from the backend; the panel only needs
 * minute precision. A missing or nonsensical value renders as an em dash rather
 * than "1970-01-01", so "never used" is not mistaken for "used in 1970".
 */
function formatTimestamp(ms) {
  const n = Number(ms);
  if (!Number.isFinite(n) || n <= 0) return '—';
  const d = new Date(n);
  if (Number.isNaN(d.getTime())) return '—';
  const pad = (v) => String(v).padStart(2, '0');
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} `
    + `${pad(d.getHours())}:${pad(d.getMinutes())}`;
}

/**
 * Copy text to the clipboard, reporting the outcome to the caller.
 *
 * `navigator.clipboard` can reject (permissions, non-secure context). Callers
 * show the value anyway, so a failure must degrade to "copy it yourself" rather
 * than throwing out of the click handler.
 */
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

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

/**
 * The key currently on screen in the reveal dialog.
 *
 * Held here rather than in a closure per call: the overlay element is created
 * once and cached (`components/overlay.js`), so per-call listeners would stack up
 * on the same element and every reveal would fire the previous reveals' handlers
 * too. The footer buttons therefore read the *current* value from this variable.
 */
let revealedKey = '';
let revealedLabel = '';

/**
 * Build the reveal dialog once and wire its controls once.
 *
 * The footer is static markup, so its buttons exist from the first build and are
 * bound a single time; only the body is repainted per reveal.
 */
function ensureRevealDialog() {
  const el = overlay(REVEAL_ID, () => modalShell({
    id: REVEAL_ID,
    title: '访问 key（仅显示这一次）',
    titleId: REVEAL_TITLE_ID,
    bodyId: REVEAL_BODY_ID,
    footer: `
      <button type="button" class="btn btn-small" id="btn-client-account-copy-key">复制 key</button>
      <button type="button" class="btn btn-small" id="btn-client-account-copy-header">复制请求头</button>
      <button type="button" class="btn btn-small btn-primary" data-close>我已保存</button>
    `,
  }));

  const close = () => {
    el.classList.remove('active');
    // The plaintext is unrecoverable from this point on, so drop the copy the
    // page was holding rather than keeping it in a module variable.
    revealedKey = '';
    revealedLabel = '';
    // Repaint so the new row appears without the user wondering whether the
    // creation actually happened.
    void renderClientAccounts();
  };

  el.addEventListener('click', (e) => {
    if (e.target === el || e.target.closest('[data-close]')) close();
  });
  document.addEventListener('keydown', (e) => {
    if (e.key === 'Escape' && el.classList.contains('active')) close();
  });

  $('#btn-client-account-copy-key')?.addEventListener('click', async () => {
    if (!revealedKey) return;
    toast(
      await copyText(revealedKey) ? '已复制访问 key' : '复制失败，请手动选中复制',
      'ok',
    );
  });
  $('#btn-client-account-copy-header')?.addEventListener('click', async () => {
    if (!revealedKey) return;
    toast(
      await copyText(`${KEY_HEADER}: ${revealedKey}`) ? '已复制请求头' : '复制失败，请手动选中复制',
      'ok',
    );
  });

  return el;
}

/**
 * Show the generated key once.
 *
 * Dismissing by backdrop or Escape is allowed on purpose: the key is already
 * unrecoverable server-side, so making it hard to close would not protect
 * anything — the body states the consequence instead.
 */
function showRevealedKey({ label, apiKey }) {
  const el = ensureRevealDialog();
  revealedKey = apiKey;
  revealedLabel = label;

  const body = $(`#${REVEAL_BODY_ID}`);
  if (body) {
    body.innerHTML = html`
      <div class="wb-transfer-modal">
        <p class="wb-transfer-hint">
          <b>关闭后不再显示，请立即复制保存。</b>
          服务端保留该 key 以便写入客户端配置，因此也可以再次查看；但如果这把 key 泄露，'
          应在列表中「重新签发」一把新的，旧 key 会立即失效。
        </p>
        <div class="form-group">
          <label>账户备注</label>
          <div>${revealedLabel || '（未命名）'}</div>
        </div>
        <div class="form-group">
          <label>访问 key</label>
          <div class="mono">${apiKey}</div>
        </div>
        <div class="form-group">
          <label>请求时带上这个头</label>
          <div class="mono">${KEY_HEADER}: ${apiKey}</div>
        </div>
        <p class="hint">
          这是<b>入站</b>凭据，只用于访问本网关；与「身份池」里那些<b>上游</b>密钥互不影响，
          也不会被转发给上游。
        </p>
      </div>
    `;
  }

  el.classList.add('active');
}

/** Open the create dialog and create the account on confirm. */
function openCreateDialog() {
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
function initCreateDialog() {
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

/** Wire the panel's controls. Called once, from the settings view init. */
export function initClientAccounts() {
  initCreateDialog();
  $('#btn-client-account-add')?.addEventListener('click', openCreateDialog);
  // Paint immediately so the section is never an empty box on first view.
  void renderClientAccounts();
}
