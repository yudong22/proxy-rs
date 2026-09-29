/**
 * Shared status pills.
 *
 * The request table row and the request detail modal render the same pills over
 * the same fields, so the markup is defined here once. Duplicated copies are how
 * the two views drift: the CSS classes are shared, so a wording or threshold
 * change made in only one of them shows up as an inconsistency the stylesheets
 * cannot correct.
 */

import { html } from '../core/dom.js';

/**
 * True when a logged request failed: a 4xx/5xx status, or an error string the
 * backend recorded even though a status was never assigned.
 */
export function isFailedRequest(item) {
  return item.status >= 400 || Boolean(item.error);
}

/** Status pill for a 2xx/4xx/5xx (or failed) response. */
export function statusBadge(item) {
  if (item.status >= 200 && item.status < 300) {
    return html`<span class="badge-pill status-pill status-2xx">${item.status} OK</span>`;
  }
  if (item.status >= 400 && item.status < 500) {
    return html`<span class="badge-pill status-pill status-4xx">${item.status}</span>`;
  }
  return html`<span class="badge-pill status-pill status-5xx">${item.status || 'ERR'}</span>`;
}

/**
 * Modifier class for the override pill: green when an exception override
 * rerouted the request successfully, red when it failed anyway. Pairs with the
 * `.override-pill` base class.
 */
export function overridePillClass(item) {
  return isFailedRequest(item) ? 'override-failed' : 'override-ok';
}
