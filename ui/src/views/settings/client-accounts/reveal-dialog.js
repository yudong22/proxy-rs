/**
 * The one-time plaintext key disclosure.
 *
 * Split out of `client-accounts.js`. `client_account_create` returns the raw key
 * on creation and never again, so this dialog is the only place a user can ever
 * obtain it — which is why it states that in the body rather than in a tooltip.
 *
 * The repaint is injected rather than imported back from `list.js`: the list
 * calls `showRevealedKey`, so importing the list from here would close a cycle.
 * Same idiom as `setOAuthPoolRefresh` / `setReloginPoolRefresh`.
 */

import { $, html } from '../../../core/dom.js';
import { overlay, modalShell } from '../../../components/overlay.js';
import { toast } from '../../../components/toast.js';
import { KEY_HEADER, copyText } from './shared.js';

/** Repaints the access-account list; injected by `index.js` at init. */
let repaintList = () => {};

/** @param {() => void} fn */
export function setAccountsRepaint(fn) {
  repaintList = fn;
}

/** One-time plaintext reveal. */
const REVEAL_ID = 'client-account-reveal-overlay';
const REVEAL_TITLE_ID = 'client-account-reveal-title';
const REVEAL_BODY_ID = 'client-account-reveal-body';

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
    void repaintList();
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
export function showRevealedKey({ label, apiKey }) {
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
