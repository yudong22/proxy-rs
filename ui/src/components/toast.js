/**
 * Transient feedback.
 *
 * `toast(message)` for a plain notice; pass an `action` to render a button
 * inside the toast, which is how a confirmation is asked for when a full modal
 * would be too heavy (the console-clear flow uses this).
 *
 * The host element is created on first use.
 */

import { overlay } from './overlay.js';

const HOST_ID = 'toast-host';

/** The stacking host, created on first toast. */
function host() {
  return overlay(HOST_ID, () => `<div class="toast-host" id="${HOST_ID}"></div>`);
}

/**
 * Show a toast.
 *
 * @param {string} message
 * @param {'ok'|'error'} [kind]
 * @param {{label: string, onClick: (dismiss: () => void) => void, timeout?: number}} [action]
 *        Optional inline button. It receives a `dismiss` callback so the
 *        handler can close the toast after running.
 */
export function toast(message, kind = 'ok', action = null) {
  const el = document.createElement('div');
  el.className = `toast toast-${kind}`;

  const text = document.createElement('span');
  text.textContent = message;
  el.appendChild(text);

  let timer = null;
  const dismiss = () => {
    if (timer) clearTimeout(timer);
    el.remove();
  };

  if (action && action.label) {
    const btn = document.createElement('button');
    btn.type = 'button';
    btn.className = 'toast-action';
    btn.textContent = action.label;
    btn.addEventListener('click', () => {
      if (action.onClick) action.onClick(dismiss);
    });
    el.appendChild(btn);
    // A toast with a button is waiting on a decision, so it stays up longer
    // than a plain notice before it times out on its own.
    timer = setTimeout(dismiss, action.timeout || 5000);
  } else {
    timer = setTimeout(dismiss, 3000);
  }

  host().appendChild(el);
}
