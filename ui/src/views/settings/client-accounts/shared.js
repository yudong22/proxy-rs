/**
 * Values and helpers shared by the client-access-account modules.
 *
 * Split out of `client-accounts.js`: the request header name appears in the
 * reveal dialog, the empty-state help and the row tooltip, so it has to be one
 * constant rather than three copies of a string.
 */

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

export { KEY_HEADER, formatTimestamp, copyText };
