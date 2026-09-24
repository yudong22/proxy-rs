/**
 * Destructive-action confirmation.
 *
 * `window.confirm` is not implemented in the Tauri webview: it returns without
 * showing anything, so a guarded action silently never ran. This is the
 * replacement — an in-page dialog that resolves a promise.
 *
 * The dialog builds itself on first use; see components/overlay.js.
 */

import { $, setText } from '../core/dom.js';
import { overlay, modalShell } from './overlay.js';

const ID = 'confirm-overlay';
const TITLE_ID = 'confirm-title';
const MESSAGE_ID = 'confirm-message';

/** Resolver for the dialog currently on screen, or null when none is open. */
let pendingResolve = null;

/** Create the dialog if needed, and return it. */
function ensure() {
  return overlay(ID, () => modalShell({
    id: ID,
    title: '确认',
    titleId: TITLE_ID,
    bodyId: MESSAGE_ID,
    footer: `
      <button type="button" class="btn btn-small" data-confirm-cancel>取消</button>
      <button type="button" class="btn btn-small btn-danger" data-confirm-ok>确定</button>
    `,
  }));
}

/**
 * Ask the user to confirm. Resolves `true` on 确定, `false` on 取消, a backdrop
 * click, or Escape.
 */
export function confirmAction(message, title = '确认') {
  const el = ensure();
  if (!el) {
    // An overlay that cannot be built must not run a destructive action the
    // user never saw, and must not leave the caller hanging either.
    console.error('confirmAction: could not build #confirm-overlay; refusing action');
    return Promise.resolve(false);
  }

  setText(TITLE_ID, title);
  setText(MESSAGE_ID, message);
  el.classList.add('active');
  el.querySelector('[data-confirm-ok]')?.focus();

  return new Promise((resolve) => {
    pendingResolve = resolve;
  });
}

/** Close the dialog, resolving it with `result`. */
export function settleConfirm(result) {
  $(`#${ID}`)?.classList.remove('active');
  const resolve = pendingResolve;
  pendingResolve = null;
  if (resolve) resolve(result);
}

/** True while a confirmation is on screen. */
export function isConfirmOpen() {
  return pendingResolve !== null;
}

/** Wire the dialog's own controls. Called once, from main.js. */
export function initConfirm() {
  const el = ensure();
  el.addEventListener('click', (e) => {
    // Backdrop click cancels, matching the detail modal's behaviour.
    if (e.target === el) return settleConfirm(false);
    if (e.target.closest('[data-confirm-ok]')) return settleConfirm(true);
    if (e.target.closest('[data-confirm-cancel]')) return settleConfirm(false);
  });
}
