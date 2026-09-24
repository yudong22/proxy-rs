/**
 * Value formatting for display.
 *
 * Pure string functions with no DOM access, so they are trivial to eyeball and
 * to reuse from any view.
 */

/**
 * Abbreviate a large count: 1234 → "1.2K", 2500000 → "2.50M".
 *
 * Below 10000 the K value keeps one decimal ("9.9K"), because that is the range
 * where the first decimal still carries information; above it the number is
 * wide enough that the decimal is noise.
 */
export function formatNumber(n) {
  const num = Number(n) || 0;
  if (num < 1000) return String(num);
  if (num < 1000000) return `${(num / 1000).toFixed(num < 10000 ? 1 : 0)}K`;
  return `${(num / 1000000).toFixed(2)}M`;
}

/** Coarse uptime, in the largest one or two units that stay readable. */
export function formatUptime(secs) {
  const s = Number(secs) || 0;
  if (s < 60) return `${s}秒`;
  const mins = Math.floor(s / 60);
  if (mins < 60) return `${mins}分 ${s % 60}秒`;
  const hours = Math.floor(mins / 60);
  return `${hours}时 ${mins % 60}分`;
}

/** Response time: milliseconds below one second, seconds above it. */
export function formatDuration(ms) {
  if (!ms || ms <= 0) return '0ms';
  if (ms < 1000) return `${ms}ms`;
  return `${(ms / 1000).toFixed(2)}s`;
}

/**
 * `2026-09-23 15:36:53.798` → `15:36:53`.
 *
 * Milliseconds and the date are noise in a request list: the date is almost
 * always today, and both stay available in the cell's `title` and the detail
 * modal. Dropping them is what lets the column be narrow enough to leave room
 * for the model column.
 */
export function shortTime(createdAt) {
  if (!createdAt) return '';
  const match = /(\d{2}:\d{2}:\d{2})/.exec(createdAt);
  return match ? match[1] : createdAt;
}

/**
 * `codex:01a0cc30-3318-74d2-b045-650a0b0c2e1c` → `01a0cc30`.
 *
 * The client prefix is dropped on purpose when the table already renders a
 * client pill beside this id, which would otherwise print the client name
 * twice. When the prefix differs from the pill's client it is kept, so a
 * mismatched pair stays visible. The full value lives in the cell's `title`
 * and in the log file.
 */
export function shortSessionId(sessionId, client) {
  if (!sessionId) return '';
  const idx = sessionId.indexOf(':');
  if (idx < 0) return sessionId;
  const prefix = sessionId.slice(0, idx);
  const rest = sessionId.slice(idx + 1);
  const head = rest.split('-')[0] || rest;
  const short = head.length < rest.length ? head : rest;
  if (client && prefix === client) return short;
  return `${prefix}:${short}`;
}
