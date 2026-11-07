/**
 * The one-line status readout under the identity pool (`#wb-status`).
 *
 * Split out of `credential-pool.js` because every pool module writes to it: the
 * table's row actions, the drag reorder and the growth run. Keeping it in one
 * leaf module means none of them has to import the list renderer just to report
 * a result.
 */

import { $ } from '../../../core/dom.js';

export function showWbStatus(message, isError = false) {
  const el = $('#wb-status');
  if (!el) return;
  el.textContent = message;
  el.classList.toggle('error', isError);
}
