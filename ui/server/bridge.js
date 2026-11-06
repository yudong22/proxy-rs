/**
 * Server-mode bridge: fill `window.__TAURI__` so the **desktop console runs
 * unmodified in a browser**.
 *
 * ## Why this exists at all
 *
 * The gateway serves the exact front-end the Tauri window loads. Nothing in
 * `ui/src/**` was changed to support that. The single seam is
 * `core/ipc.js`, which routes every call through `invoke()` and every event
 * through `listen()`:
 *
 *   invoke  →  window.__TAURI__.core.invoke        (or __TAURI_INTERNALS__)
 *   listen  →  window.__TAURI__.event.listen
 *
 * So this file only has to define those two functions before `main.js` runs. No
 * view, stylesheet or component knows the difference — which is the whole point:
 * a layout fix or a copy change made for the desktop app reaches the server
 * console automatically, and there is no second UI to drift.
 *
 * ## Wire format
 *
 * A Tauri command is `invoke(name, argsObject)`; the server exposes the same
 * vocabulary as `POST /admin/api/rpc/<name>` with that object as the JSON body,
 * answered with the command's return value or a thrown message. The command
 * names are the transport contract precisely so no call site changes.
 *
 * ## What is deliberately not emulated
 *
 * Service start/stop and opening a file manager have no server-side meaning.
 * `start_service` / `stop_service` reject with an explanation instead of
 * pretending to succeed — a button that silently does nothing is worse than one
 * that reports why. Path-reveal commands resolve quietly, because their only
 * consumer is dismissing a toast.
 */

const RPC_BASE = '/admin/api/rpc';

/** Commands that cannot work in a browser, with the reason shown to the user. */
const UNSUPPORTED = {
  start_service:
    '服务器模式下服务由 systemd 托管，请用 systemctl start proxy-rs',
  stop_service:
    '服务器模式下服务由 systemd 托管，请用 systemctl stop proxy-rs',
};

/** Commands whose result is discarded by the UI; resolving quietly is correct. */
const NOOP = new Set(['open_logs_dir', 'reveal_path']);

/**
 * Call one backend command.
 *
 * Errors are thrown as plain strings, matching Tauri: the views catch them and
 * render `e` directly (`result.textContent = \`导出失败: ${e}\``), so wrapping them
 * in an Error object would print `[object Object]`.
 */
async function invoke(command, args = {}) {
  if (UNSUPPORTED[command]) throw UNSUPPORTED[command];
  if (NOOP.has(command)) return null;

  const response = await fetch(`${RPC_BASE}/${encodeURIComponent(command)}`, {
    method: 'POST',
    credentials: 'same-origin',
    cache: 'no-store',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(args ?? {}),
  });

  if (response.status === 401) {
    // The session expired (12h idle, or the gateway restarted). Reloading lands
    // on the login form, which is the only honest recovery — every view's data
    // is now unreachable.
    window.location.reload();
    throw '会话已过期，请重新登录';
  }

  const text = await response.text();
  let body = null;
  if (text) {
    try {
      body = JSON.parse(text);
    } catch {
      body = null;
    }
  }

  if (!response.ok) {
    throw body?.error?.message || `HTTP ${response.status}`;
  }
  // Tauri returns the command's value directly; the server wraps it so an error
  // envelope and a legitimate `null` stay distinguishable.
  return body?.result ?? null;
}

/**
 * Subscribe to a backend event.
 *
 * Two events exist in the desktop app — `service-state` (listener came up / went
 * down) and `growth-progress` (a growth run reporting as it goes). On a server
 * neither is pushed: the listener never stops (systemd owns it), and the growth
 * run is synchronous over HTTP. Returning a no-op unlisten keeps every caller
 * working; they subscribe and simply never receive anything.
 */
async function listen(_event, _handler) {
  return () => {};
}

// Defined synchronously, before `main.js` is evaluated: `ipc.js` reads
// `window.__TAURI__` inside `invoke`, so a later definition would work too — but
// `isTauri()` is consulted during boot in some views, and a false answer there
// would suppress work the server actually needs.
window.__TAURI__ = {
  core: { invoke },
  event: { listen },
};

// A marker the console can read to adjust wording, if a future view needs to
// know it is not in the desktop shell. Nothing reads it today; it exists so the
// question has an answer that is not "sniff the user agent".
window.__PROXY_RS_SERVER__ = true;
