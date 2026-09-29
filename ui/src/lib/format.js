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

/**
 * Format a remaining-points balance.
 *
 * `null`/`undefined` means the balance has never been queried, which must read
 * differently from a genuine 0 — hence the em dash rather than "0".
 */
export function formatPoints(points) {
  if (points === null || points === undefined) return '—';
  const n = Number(points);
  if (!Number.isFinite(n)) return '—';
  // Integral balances read better without a decimal tail.
  return Number.isInteger(n) ? String(n) : n.toFixed(2);
}

/**
 * Output speed, `xx.x tok/s`.
 *
 * A missing measurement is "—", never "0": an unmeasured request (a plain
 * non-streamed reply, or one served before timing existed) must not read as a
 * stalled model.
 */
export function formatTps(tps) {
  if (tps === null || tps === undefined) return '—';
  const n = Number(tps);
  if (!Number.isFinite(n) || n <= 0) return '—';
  // Above 100 tok/s the decimal is noise; below it, it is the useful digit.
  return n >= 100 ? `${Math.round(n)} tok/s` : `${n.toFixed(1)} tok/s`;
}

/**
 * Long-form duration for the speed drill-down: `x分 xx秒` / `x.x秒`.
 *
 * Distinct from `formatDuration` (tuned for the dense request table): these are
 * read one at a time in a detail panel, so the minutes form is spelled out
 * rather than shown as a large second count.
 */
export function formatLongDuration(ms) {
  if (ms === null || ms === undefined) return '—';
  const n = Number(ms);
  if (!Number.isFinite(n) || n < 0) return '—';
  if (n < 1000) return `${Math.round(n)}毫秒`;
  const secs = n / 1000;
  // Compare the *rounded* value: 59.97 s would otherwise print "60.0秒", which
  // is both ugly and wrong in spirit (it is a minute).
  if (secs.toFixed(1) !== '60.0' && secs < 60) return `${secs.toFixed(1)}秒`;
  const mins = Math.floor(secs / 60);
  const rest = Math.round(secs - mins * 60);
  // A rounded 60 here would print "1分 60秒".
  if (rest === 60) return `${mins + 1}分 00秒`;
  return `${mins}分 ${String(rest).padStart(2, '0')}秒`;
}

/**
 * Seconds with one decimal, for a *per-event* figure such as the mean TTFT:
 * `x.x秒`.
 *
 * Integer milliseconds below a second keep more information, so they are shown
 * as milliseconds instead of "0.0秒".
 *
 * Not for a cumulative total: a session's total tool wait reaches hours, where
 * a single decimal on a five-digit second count is unreadable. Use
 * [`formatLongDuration`] for anything that accumulates.
 */
export function formatSeconds(ms) {
  if (ms === null || ms === undefined) return '—';
  const n = Number(ms);
  if (!Number.isFinite(n) || n < 0) return '—';
  if (n < 1000) return `${Math.round(n)}毫秒`;
  return `${(n / 1000).toFixed(1)}秒`;
}
