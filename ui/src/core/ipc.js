/**
 * IPC / transport seam.
 *
 * Two entry points only: `invoke` for commands and `listen` for events. Views
 * import these instead of touching `window.__TAURI__` or `fetch` directly, so
 * the one place that knows which transport is live is this file.
 *
 * ## Three transports, one call site
 *
 * The same page runs in three homes, and the choice is structural, not a
 * preference:
 *
 * 1. **Same-origin console** (`/admin/` served by the gateway itself — the
 *    desktop webview after bootstrap, and any browser an operator points at
 *    the server). Commands go over `POST /admin/api/rpc/<cmd>` with the
 *    session cookie. This is the primary transport: it is what makes the
 *    desktop and the server console byte-identical, and it is why the Rust
 *    side has exactly one implementation per command (see `src/admin_rpc.rs`).
 * 2. **Tauri webview not on `/admin/`** (the bootstrap page, and pages opened
 *    for CSS work before the gateway is up). Commands go through
 *    `window.__TAURI__.core.invoke`. Only the desktop-only commands
 *    (`start_service`, native dialogs, …) still exist there; everything
 *    else is answered by the same RPC surface the console uses.
 * 3. **Plain browser preview** (no Tauri, no same-origin gateway): degrade to
 *    a logged no-op so a page can be opened for styling without a backend.
 *
 * The server-hosted console (`window.__PROXY_RS_SERVER__`) already ran a bridge
 * with this exact wire shape (see `ui/server/bridge.js`); that bridge keeps
 * installing `window.__TAURI__` before this module loads, so it needs no
 * special case here — its `invoke` is reached through the Tauri branch, and
 * `NOOP`/`UNSUPPORTED` there cover the commands a server cannot mean.
 */

const RPC_BASE = '/admin/api/rpc';

/**
 * Commands that only ever exist as real Tauri commands, never as RPC arms.
 *
 * They touch process/OS state the HTTP surface must not expose: service
 * lifecycle (controlling the listener this very page may have been served
 * from), native file dialogs, and the file manager. Everything else — the
 * whole business surface — lives in `admin_rpc` and is transport-independent.
 */
const DESKTOP_ONLY = new Set([
  'get_bootstrap_info',
  'start_service',
  'stop_service',
  'open_logs_dir',
  'reveal_path',
  'pool_export', // desktop: native save panel (HTTP equivalent: /admin/api/pool/export download)
]);

/** True when this page was served by the gateway itself (same origin). */
function isSameOriginConsole() {
  if (window.__PROXY_RS_SERVER__) return false; // server bridge owns the transport
  return window.location.pathname.startsWith('/admin');
}

/**
 * Call one backend command.
 *
 * Same-origin pages talk HTTP; the Tauri shell (and the server bridge's shim)
 * is used for what remains. In a plain browser preview there is no backend at
 * all: a logged no-op keeps styling work from aborting.
 */
export async function invoke(cmd, args = {}) {
  const tauri = window.__TAURI__;
  const tauriInvoke = tauri?.core?.invoke
    ? tauri.core.invoke.bind(tauri.core)
    : window.__TAURI_INTERNALS__?.invoke?.bind(window.__TAURI_INTERNALS__);

  // Desktop-only commands must reach the process even from the same-origin
  // console (the console page *is* allowed to ask for a native dialog).
  if (DESKTOP_ONLY.has(cmd) && tauriInvoke) {
    return await tauriInvoke(cmd, args);
  }

  if (isSameOriginConsole()) {
    return await httpInvoke(cmd, args);
  }
  if (tauriInvoke) {
    return await tauriInvoke(cmd, args);
  }
  console.warn(`[Tauri Mock] Invoke '${cmd}':`, args);
  return null;
}

/**
 * One RPC call over the session cookie. Errors are thrown as plain strings,
 * matching Tauri: views render `e` directly, so an Error object would print
 * as `[object Object]`.
 */
async function httpInvoke(cmd, args = {}) {
  const response = await fetch(`${RPC_BASE}/${encodeURIComponent(cmd)}`, {
    method: 'POST',
    credentials: 'same-origin',
    cache: 'no-store',
    headers: { 'content-type': 'application/json' },
    body: JSON.stringify(args ?? {}),
  });
  if (response.status === 401) {
    // Session gone (expired, or the desktop listener restarted with a fresh
    // session store). Re-mint if this console can (desktop-boot is
    // desktop-only; on a server it answers 404), then surface the failure —
    // the caller decides whether to retry.
    if (!window.__PROXY_RS_REDIRECTING__) {
      window.__PROXY_RS_REDIRECTING__ = true;
      try {
        const boot = await fetch('/admin/api/desktop-boot', { cache: 'no-store' });
        if (boot.ok) {
          window.__PROXY_RS_REDIRECTING__ = false;
          // Cookie re-minted; the UI polls on timers, so the next call will
          // succeed. Report the first failure to keep the call's contract.
          throw '会话已重连，请重试';
        }
        window.location.replace(`/admin/login${window.location.hash || ''}`);
      } catch (e) {
        if (typeof e === 'string') throw e;
        window.location.replace(`/admin/login${window.location.hash || ''}`);
      }
    }
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
  return body?.result ?? null;
}

/**
 * Subscribe to a backend event.
 *
 * Events no longer carry data over the console: `service-state` is covered by
 * the status poll, and growth progress moved to the polled
 * `get_growth_progress` RPC. Both fire `hashchange`-style updates through
 * their pollers; this returns a no-op unlisten so every existing subscriber
 * keeps working without receiving anything.
 *
 * In the Tauri webview (bootstrap page, tray-driven reloads) the real event
 * bridge is kept — the tray pushes `service-state` there.
 */
export async function listen(event, handler) {
  const tauri = window.__TAURI__;
  if (!tauri?.event?.listen) return () => {};
  try {
    const unlisten = await tauri.event.listen(event, (e) => handler(e?.payload));
    return typeof unlisten === 'function' ? unlisten : () => {};
  } catch (err) {
    console.error(`listen('${event}') failed:`, err);
    return () => {};
  }
}

/**
 * True when running inside the Tauri webview. Views use this to skip work that
 * is meaningless in a plain browser (e.g. the drag-region behaviour).
 */
export function isTauri() {
  return Boolean(window.__TAURI__ || window.__TAURI_INTERNALS__);
}
