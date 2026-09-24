/**
 * Generic detail modal.
 *
 * One instance for the whole app, created on first `openModal` and reused
 * after. Callers pass pre-escaped markup (build it with the `html` template).
 */

import { $, setText } from '../core/dom.js';
import { overlay, modalShell } from './overlay.js';

const ID = 'request-modal-overlay';
const TITLE_ID = 'modal-req-title';
const BODY_ID = 'modal-req-body';

/** Create the modal if it does not exist yet, and return it. */
function ensure() {
  return overlay(ID, () => modalShell({
    id: ID,
    title: '请求详情',
    titleId: TITLE_ID,
    bodyId: BODY_ID,
    footer: '<button type="button" class="btn btn-small btn-primary" data-close>关闭</button>',
  }));
}

/**
 * Open the modal.
 *
 * @param {string} title     Plain text; set via textContent.
 * @param {string} bodyHtml  Pre-escaped markup. Use the `html` template.
 */
export function openModal(title, bodyHtml) {
  const el = ensure();
  setText(TITLE_ID, title);
  const body = $(`#${BODY_ID}`);
  if (body) body.innerHTML = bodyHtml;
  el.classList.add('active');
}

/** Close the modal. */
export function closeModal() {
  $(`#${ID}`)?.classList.remove('active');
}

/** Wire the modal's own controls. Called once, from main.js. */
export function initModal() {
  const el = ensure();

  // Every `[data-close]` inside the overlay dismisses it, so adding a footer
  // button does not need a new listener.
  el.addEventListener('click', (e) => {
    if (e.target === el || e.target.closest('[data-close]')) closeModal();
  });
}

/**
 * The modal's element ids, so callers can target its parts without knowing the
 * markup that produced them.
 */
export const MODAL_TITLE_ID = TITLE_ID;
export const MODAL_BODY_ID = BODY_ID;
