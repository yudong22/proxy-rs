/**
 * Tauri IPC.
 *
 * Two entry points only: `invoke` for commands and `listen` for events. Views
 * import these instead of touching `window.__TAURI__` directly, so the one
 * place that knows how the bridge is shaped is this file.
 */

/**
 * Call a `#[tauri::command]`.
 *
 * `window.__TAURI__.core.invoke` is a thin passthrough to
 * `window.__TAURI_INTERNALS__.invoke` (see Tauri's bundle.global.js), so the
 * second branch is not a real fallback — the bundle is only present when
 * `withGlobalTauri` is on. It is kept so a page loaded outside the Tauri shell
 * (a browser, for CSS work) degrades to a logged no-op instead of a
 * `TypeError` that would abort the caller.
 */
export async function invoke(cmd, args = {}) {
  const tauri = window.__TAURI__;
  if (tauri?.core?.invoke) {
    return await tauri.core.invoke(cmd, args);
  }
  if (window.__TAURI_INTERNALS__?.invoke) {
    return await window.__TAURI_INTERNALS__.invoke(cmd, args);
  }
  console.warn(`[Tauri Mock] Invoke '${cmd}':`, args);
  return null;
}

/**
 * Subscribe to a backend event. Returns an unlisten function, or a no-op when
 * the event bridge is unavailable.
 *
 * Listeners registered at startup live for the life of the window, so callers
 * do not normally need the returned function; it is returned for the cases
 * that rebind (a view re-initialised after a remount).
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
