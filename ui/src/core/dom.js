/**
 * DOM helpers.
 *
 * A thin layer over `querySelector` so views read as `$('#id')` / `$$('.thing')`
 * instead of repeating `document.querySelector`. Nothing here caches: an
 * element is looked up when it is needed, which keeps a view correct after its
 * markup is re-rendered.
 */

/** First element matching `selector`, or null. */
export function $(selector, root = document) {
  return root.querySelector(selector);
}

/** All elements matching `selector`, as a real Array. */
export function $$(selector, root = document) {
  return Array.from(root.querySelectorAll(selector));
}

/**
 * Look up an element by id and set its text.
 *
 * Missing elements are a no-op rather than a throw: a partial markup change
 * (or an older backend omitting an optional field) should degrade to a blank
 * cell, not break the whole refresh that called this.
 */
export function setText(id, value) {
  const el = document.getElementById(id);
  if (el) el.textContent = value;
}

/** Toggle a class by explicit boolean rather than by side effect. */
export function setClass(el, name, on) {
  if (el) el.classList.toggle(name, Boolean(on));
}

/** Show or hide an element via the `hidden` attribute. */
export function setHidden(el, hidden) {
  if (!el) return;
  if (hidden) el.setAttribute('hidden', '');
  else el.removeAttribute('hidden');
}

/**
 * Assign innerHTML from a string built by the `html` tagged template, which
 * escapes every interpolated value unless it is wrapped in {@link raw}.
 *
 * This exists so a view can be written as one readable markup literal and still
 * be safe by default: `${item.model}` is escaped, and the few places that
 * deliberately build nested markup opt in explicitly. Values are escaped for
 * both text and attribute context (see escapeHtml).
 */
export function html(strings, ...values) {
  return strings.reduce((out, chunk, i) => {
    if (i === 0) return chunk;
    const value = values[i - 1];
    if (value === null || value === undefined) return out + chunk;
    if (value instanceof Raw) return out + value.value + chunk;
    if (Array.isArray(value)) {
      return out + value.map(v => htmlEscape(String(v))).join('') + chunk;
    }
    return out + htmlEscape(String(value)) + chunk;
  }, '');
}

/** Wrapper marking a string as pre-escaped markup for {@link html}. */
export class Raw {
  constructor(value) {
    this.value = value;
  }
}

/** Mark a string as trusted markup. Callers are responsible for escaping. */
export function raw(value) {
  return new Raw(value);
}

/**
 * Escape a value for both element text and quoted-attribute context.
 *
 * `'` is included because this output is routinely placed inside
 * `title="..."` attributes, and an unescaped apostrophe in a model id would
 * otherwise terminate the attribute early.
 */
export function htmlEscape(value) {
  return String(value)
    .replace(/&/g, '&amp;')
    .replace(/</g, '&lt;')
    .replace(/>/g, '&gt;')
    .replace(/"/g, '&quot;')
    .replace(/'/g, '&#39;');
}

/**
 * Run `fn` after the user stops triggering it for `wait` ms.
 * Used by the log search boxes so typing does not re-query per keystroke.
 */
export function debounce(fn, wait = 250) {
  let timer = null;
  return (...args) => {
    if (timer) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      fn(...args);
    }, wait);
  };
}
