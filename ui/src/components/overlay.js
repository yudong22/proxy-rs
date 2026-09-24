/**
 * Overlay factory.
 *
 * The overlay elements (request detail, confirm, palette, toast host) are built
 * from script rather than sitting in index.html. Keeping their markup next to
 * the code that drives them means a change to one cannot leave the other
 * behind — the old markup had drifted from its handlers more than once.
 *
 * Each overlay is created once, on first use, and kept.
 */

import { html, raw } from '../core/dom.js';

/** The single host element every overlay appends itself to. */
function host() {
  let el = document.getElementById('overlays');
  if (!el) {
    el = document.createElement('div');
    el.id = 'overlays';
    document.body.appendChild(el);
  }
  return el;
}

/** Cached overlays, keyed by name, so a rebuild never duplicates one. */
const built = new Map();

/**
 * Build (or return) an overlay by name.
 *
 * @param {string} name   Cache key, also used for the element id.
 * @param {() => string} render  Returns the overlay's markup. Called once.
 * @returns {HTMLElement}
 */
export function overlay(name, render) {
  const existing = document.getElementById(name);
  if (existing) return existing;
  if (built.has(name)) return built.get(name);

  const template = document.createElement('template');
  template.innerHTML = render().trim();
  const el = template.content.firstElementChild;
  if (!el) throw new Error(`overlay('${name}') produced no element`);
  if (!el.id) el.id = name;
  host().appendChild(el);
  built.set(name, el);
  return el;
}

/** Markup for a modal shell: header, body, footer slots. */
export function modalShell({ id, title, bodyId, footer, titleId }) {
  return html`
    <div class="modal-overlay" id="${id}">
      <div class="modal-card">
        <div class="modal-header">
          <h3 class="modal-title" id="${titleId}">${title}</h3>
          <button type="button" class="btn-close" data-close aria-label="关闭">&times;</button>
        </div>
        <div class="modal-body" id="${bodyId}"></div>
        <div class="modal-footer">${raw(footer)}</div>
      </div>
    </div>
  `;
}
