/**
 * Settings form markup primitives.
 *
 * Pure builders: they return markup through the escaping `html` template and
 * touch no DOM, so the form's structure can be read without scrolling past the
 * wiring that fills it.
 */

import { html, raw } from '../../core/dom.js';

/**
 * A labelled input inside a `.form-group`.
 *
 * `group` is the extra class on the wrapper — `half` is what makes a field share
 * a `.form-row` with its neighbour. It must go on the wrapper rather than the
 * input: the flex item is the `.form-group`, not the `<input>` inside it.
 */
export function input(id, label, placeholder, { type = 'text', group = '', attrs = '' } = {}) {
  return html`
    <div class="form-group${raw(group ? ` ${group}` : '')}">
      <label for="${id}">${label}</label>
      <input type="${type}" id="${id}" placeholder="${placeholder}"${raw(attrs)}>
    </div>
  `;
}

/**
 * A settings group.
 *
 * By default a collapsible `<details>` + `<summary>` + body. `section` doubles as
 * the `data-section` value the badge code matches on, and as the accented
 * caret's anchor. A group with no tracked fields (codex) still gets a caret so
 * every summary reads the same.
 *
 * `collapsible: false` renders a fixed card instead: same heading row and same
 * `data-section` hook, but no `<details>` and no caret, so the group cannot be
 * folded away. Used for 身份池, which is the page's primary control — burying it
 * behind a disclosure is how a user misses the thing they came to change.
 */
export function section(section, legend, body, { open = false, collapsible = true } = {}) {
  if (!collapsible) {
    return html`
      <div class="form-section form-section--fixed" data-section="${section}">
        <div class="form-section-heading">${legend}</div>
        <div class="form-section-body">${raw(body)}</div>
      </div>
    `;
  }
  return html`
    <details class="form-section" data-section="${section}"${raw(open ? ' open' : '')}>
      <summary><span class="form-section-caret"></span>${legend}</summary>
      <div class="form-section-body">${raw(body)}</div>
    </details>
  `;
}

/** A `<label class="toggle">` switch, as used by the two boolean settings. */
export function toggle(id, label, desc) {
  return html`
    <div class="setting-item-inline">
      <div>
        <div class="setting-label">${label}</div>
        <div class="setting-desc">${desc}</div>
      </div>
      <label class="toggle">
        <input type="checkbox" id="${id}">
        <span class="toggle-slider"></span>
      </label>
    </div>
  `;
}
