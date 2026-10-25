/**
 * Dashboard data sources.
 *
 * The overview's statistics row and the request-log table are served by the
 * proxy listener itself (`GET /api/stats/today`, `GET /api/request-logs`), so
 * this module is the one place that knows where those two payloads come from.
 * Views import `getStats` / `getRequestLogs` from here instead of calling
 * `invoke` directly.
 *
 * **Why HTTP is tried first.** The listener is the process that owns the data
 * and is what a headless deployment or an external tool would talk to, so the
 * window uses the same public interface rather than a privileged side channel.
 * The identical payloads remain available as Tauri commands (`get_stats`,
 * `get_request_logs`), which is the fallback below.
 *
 * **Why the fallback is real, not decorative.** The listener only exists while
 * the service is running, and "how much did I use today" is a question asked
 * *before* starting it. When nothing is listening, the loopback fetch is refused
 * immediately, the call degrades to IPC, and the panel still fills. That also
 * covers a webview that refuses plain-HTTP requests: the dashboard keeps working
 * over IPC instead of going blank.
 *
 * **Why the backend keeps both.** `src/api.rs` defines each payload once; the
 * Tauri commands and the HTTP handlers both call that definition, so the two
 * transports cannot drift into different field names or number formatting. Both
 * transports return the same shape, which is what makes this a pure transport
 * switch.
 */

import { invoke } from './ipc.js';
import { appState } from './state.js';

/**
 * How long an HTTP read may take before the IPC fallback takes over.
 *
 * Loopback answers in single-digit milliseconds, so this is only reached when
 * the listener accepted the connection but is wedged (a writer holding the
 * SQLite lock, say). Without a bound, one wedged query would leave a card
 * showing a stale number forever instead of falling back.
 */
const HTTP_TIMEOUT_MS = 4000;

/**
 * Set once a fetch has failed, cleared by the next success; keeps the console to
 * one message per outage instead of one per poll (the log table polls every two
 * seconds).
 */
let httpDown = false;

/** Loopback base URL of the observation endpoints. */
function apiBase() {
  // Test hook for `ui/tools/smoke.mjs`, which serves the page and the two
  // endpoints from one harness origin so the HTTP path is exercised without
  // depending on whether a real instance happens to own the configured port.
  // Unset in the app, where the port from `get_status` is the only correct base.
  const override = window.__PROXY_API_BASE__;
  if (typeof override === 'string' && override) return override.replace(/\/+$/, '');
  return `http://127.0.0.1:${appState.port || 3456}`;
}

/**
 * One JSON read from the listener, or a thrown error.
 *
 * `cache: 'no-store'` mirrors the handler's `Cache-Control: no-store`: these are
 * live counters, and a cached copy would be a number that silently stopped
 * moving.
 */
async function fetchJson(path) {
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), HTTP_TIMEOUT_MS);
  try {
    const res = await fetch(`${apiBase()}${path}`, {
      cache: 'no-store',
      signal: controller.signal,
    });
    if (!res.ok) throw new Error(`HTTP ${res.status}`);
    return await res.json();
  } finally {
    clearTimeout(timer);
  }
}

/**
 * Run one observation read: HTTP when the listener answers, IPC otherwise.
 *
 * A failure here is never fatal — the fallback is the pre-existing code path, so
 * an unreachable listener costs latency, not a blank panel.
 */
async function read(path, command, args) {
  try {
    const payload = await fetchJson(path);
    if (httpDown) {
      httpDown = false;
      console.info(`[api] ${path} reachable again`);
    }
    return payload;
  } catch (err) {
    if (!httpDown) {
      httpDown = true;
      // Info, not error: a stopped service is an ordinary state, and logging it
      // as an error would paint a red line in the console on every cold start.
      console.info(`[api] ${path} unavailable (${err.message}); using IPC`);
    }
    return await invoke(command, args);
  }
}

/**
 * Today's counters — the overview's 请求总数 / Token 总量 / 缓存命中率 / 失败请求.
 *
 * Shape: `{ date, requests_total, requests_success, requests_failed,
 * tokens_total, tokens_input, tokens_cache_read, tokens_cache_write,
 * tokens_output, cache_hit_pct, overrides_total }`.
 */
export function getStats() {
  return read('/api/stats/today', 'get_stats');
}

/**
 * One page of the request log plus its facet lists.
 *
 * The filter object is the same one the Tauri command accepted (`model`,
 * `client_model`, `client`, `session_id`, `status_group`, `streamed`, `search`,
 * `limit`, `offset`). Empty values are dropped rather than sent as `all`/`''`:
 * the backend treats both as "no filter", but omitting them keeps the URL
 * readable and avoids encoding an empty boolean.
 */
export function getRequestLogs(filter = {}) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(filter)) {
    if (value === undefined || value === null || value === '') continue;
    query.set(key, String(value));
  }
  const qs = query.toString();
  return read(`/api/request-logs${qs ? `?${qs}` : ''}`, 'get_request_logs', { filter });
}
