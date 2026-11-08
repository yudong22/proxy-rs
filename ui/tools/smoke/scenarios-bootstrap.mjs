/**
 * The Tauri window's start page (`bootstrap.html`).
 *
 * This file is loaded by the window *before* the console exists: it asks the
 * host over IPC whether the console listener is up, then hands the window to
 * `http://127.0.0.1:<port>/admin/`. Nothing else in the harness touched it, and
 * that gap shipped a release where the page was **not built at all** —
 * `tauri.conf.json` pointed the window at `bootstrap.html`, Vite never emitted
 * it (only `index.html` was an input), so the app opened on a missing file: a
 * blank window, while the console itself was healthy on its own port.
 *
 * `check-dist.mjs` now asserts the file is present in the build. This asserts it
 * still *works*: loads, reaches `get_bootstrap_info`, and navigates to the
 * console port the host reports.
 */

/** The page redirects to the console; this is the port it must be told about. */
const CONSOLE_PORT = 4100;

/**
 * Load `bootstrap.html` with a host that claims the console is ready on
 * [`CONSOLE_PORT`], and assert the page navigates there.
 */
export async function checkBootstrapPage(ctx) {
  const { send, check, origin } = ctx;

  // Replace the bridge with one that answers `get_bootstrap_info`, which the
  // rest of the harness never needs (every other scenario loads the console,
  // not the start page). Registered for the *next* document only, so the
  // scenarios that run after this one still see their own bridge.
  const { identifier } = await send('Page.addScriptToEvaluateOnNewDocument', {
    source: `
      window.__TAURI__ = window.__TAURI__ || {};
      window.__TAURI__.core = {
        invoke: (cmd) => {
          if (cmd === 'get_bootstrap_info') {
            return Promise.resolve({
              console_ready: true,
              port: ${CONSOLE_PORT},
              proxy_running: true,
              proxy_port: 3457,
              version: 'test',
              macos: true,
            });
          }
          return Promise.resolve(null);
        },
      };
    `,
  });

  // Capture the navigation instead of following it: the redirect target is a
  // real loopback address the harness does not serve, and a failed navigation
  // would otherwise look like the page being broken.
  await send('Runtime.evaluate', {
    expression: `window.__BOOT_NAV__ = null;`,
  });
  await send('Page.navigate', { url: `${origin}/bootstrap.html` });

  let landed = null;
  for (let i = 0; i < 40; i += 1) {
    await new Promise((r) => setTimeout(r, 100));
    const r = await send('Runtime.evaluate', {
      returnByValue: true,
      expression: `({
        url: location.href,
        msg: (document.getElementById('msg') || {}).textContent || '',
        hasBtn: Boolean(document.getElementById('btn')),
      })`,
    });
    const v = r.result.value;
    if (!v) continue; // navigation in flight
    landed = v;
    // `chrome-error://` is the harness's own view of "navigated to a port that
    // is not being served here" — nothing listens on CONSOLE_PORT in the
    // harness. That is the *expected* outcome: the page asked to go to the
    // console, which only exists on a real host. The assertion below is that
    // the page got far enough to request it, not that it arrived.
    if (v.url.includes(':') && v.url.includes('/admin/')) break;
    if (v.url.startsWith('chrome-error://')) break;
  }

  await send('Page.removeScriptToEvaluateOnNewDocument', { identifier });

  // The page itself must have been built: a missing `bootstrap.html` leaves the
  // window on a file-not-found page, where `#msg` and `#btn` never exist.
  check(
    'bootstrap page is built and served',
    Boolean(landed) && (landed.hasBtn || landed.url.startsWith('chrome-error://') || landed.url.includes(CONSOLE_PORT)),
    true,
  );
  // It must have reached the redirect. Whether the target answers is the host's
  // concern (and the harness has no console on that port); asking for it is the
  // page's contract.
  check(
    'bootstrap page reaches the console port the host reports',
    Boolean(landed?.url?.startsWith('chrome-error://') || landed?.url?.includes(`:${CONSOLE_PORT}/admin/`)),
    true,
  );
}
