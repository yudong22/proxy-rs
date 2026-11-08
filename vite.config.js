import { defineConfig } from 'vite';
import { resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));

// ── Backend targets for `ui-dev` ────────────────────────────────────────────
// The installed desktop app listens on PROD_PORT for its data plane and
// PROD_PORT + 1 for its console (`start_console_listener`).
//
// The test gateway sits `DEV_PORT_OFFSET` above the app, not beside it: the app
// also owns `+ 1`, and a test instance one port away made the two pairs
// indistinguishable in `lsof` output. `Taskfile.yaml` computes the same number
// from the same offset; this file only needs it when Vite is started by hand.
const PROD_PORT = Number(process.env.PROD_PROXY_PORT) || 3457;
const PROD_CONSOLE_PORT = Number(process.env.PROD_CONSOLE_PORT) || PROD_PORT + 1;
const DEV_PORT_OFFSET = Number(process.env.DEV_PORT_OFFSET) || 1000;
const DEV_PORT = Number(process.env.DEV_PROXY_PORT) || PROD_PORT + DEV_PORT_OFFSET;
const BACKEND_HOST = (process.env.BACKEND || 'http://127.0.0.1').replace(/\/+$/, '').replace(/:\d+$/, '');

/**
 * Target a proxy entry carries until the first request retargets it.
 *
 * Only reached if a proxied request somehow arrives without the plugin's
 * `request` wrapper having run; the dev gateway is the safe choice.
 */
const PLACEHOLDER = `${BACKEND_HOST}:${DEV_PORT}`;

/** The installed app's listener for one prefix. */
const prodTarget = (kind) => `${BACKEND_HOST}:${kind === 'admin' ? PROD_CONSOLE_PORT : PROD_PORT}`;

/** `proxy_rs_backend=prod` in the cookie jar means the installed app. */
const wantsProd = (req) =>
  /(?:^|;\s*)proxy_rs_backend=prod(?:;|$)/.test(req.headers?.cookie || '');

/**
 * Retarget the two proxy entries per request, from the `proxy_rs_backend`
 * cookie.
 *
 * **Serve only** — this is the `ui-dev` switch; a build has no proxy to
 * retarget. It runs as a plugin (not a middleware in `configureServer`) so its
 * `request` wrapper is registered after Vite has mounted its own middleware
 * stack, and therefore runs first.
 *
 * It wraps the first `request` listener — Vite's connect stack — rather than
 * adding a listener of its own: a second listener would race the stack, and
 * whether it won would depend on registration order. Wrapping keeps the whole
 * decision inside one listener, before the proxy middleware reads `target`.
 *
 * What it must **not** do is retarget via http-proxy's `proxyReq` hook: that
 * fires after http-proxy's `stream` pass has already parsed `target` and
 * opened the socket, so it would change the next request's target, not this
 * one's. See the comment on `server.proxy` for the two hooks that don't work.
 */
function backendSwitch() {
  return {
    name: 'proxy-rs:proxy-target',
    apply: 'serve',
    configureServer(server) {
      const httpServer = server.httpServer;
      if (!httpServer) return; // middleware mode: nothing is listening to fix
      const [connectStack] = httpServer.listeners('request');
      if (typeof connectStack !== 'function') return;

      httpServer.removeListener('request', connectStack);
      httpServer.on('request', (req, res) => {
        const proxy = server.config.server.proxy;
        const backend = wantsProd(req) ? 'prod' : 'dev';
        // Two keys only; `proxy` is a literal in this file, not user config.
        proxy['/admin'].target = backend === 'prod' ? prodTarget('admin') : `${BACKEND_HOST}:${DEV_PORT}`;
        proxy['/api'].target = backend === 'prod' ? prodTarget('api') : `${BACKEND_HOST}:${DEV_PORT}`;
        connectStack(req, res);
      });
    },
  };
}

/**
 * The `ui-dev`-only header control, injected by the `proxy-rs:backend-switch`
 * plugin and never part of a build.
 *
 * Self-contained (inline style + script) on purpose: it is not a product
 * feature, it has no design-token home, and giving it a stylesheet or a module
 * in `ui/src/` would make it something the app has to keep working. It sits
 * fixed in the corner, out of the layout's way.
 */
const DEV_BADGE_MARKUP = `
<div id="dev-backend-bar" style="
  position: fixed; right: 12px; bottom: 12px; z-index: 99999;
  display: flex; align-items: center; gap: 8px;
  font: 12px/1.4 -apple-system, 'PingFang SC', 'Segoe UI', sans-serif;
  background: rgba(0,0,0,.82); color: #fff; padding: 6px 10px; border-radius: 8px;
  box-shadow: 0 4px 16px rgba(0,0,0,.3);
">
  <span style="font-weight:700;letter-spacing:.06em;background:#f59e0b;color:#111;
    padding:2px 6px;border-radius:4px;">DEV</span>
  <span id="dev-backend-note" style="opacity:.75;"></span>
  <button id="dev-backend-toggle" type="button" style="
    font:inherit;padding:3px 10px;border-radius:5px;cursor:pointer;
    border:1px solid rgba(255,255,255,.35);background:transparent;color:#fff;"></button>
</div>
<script>
(function () {
  var KEY = 'proxy_rs_backend';
  function current() {
    var m = document.cookie.match(/(?:^|;\\s*)proxy_rs_backend=(dev|prod)(?:;|$)/);
    return m ? m[1] : 'dev';
  }
  var btn = document.getElementById('dev-backend-toggle');
  var note = document.getElementById('dev-backend-note');
  function paint() {
    var isProd = current() === 'prod';
    // The button offers the *other* backend, so the label says what a click does.
    btn.textContent = isProd ? '切到测试' : '切到生产';
    btn.title = isProd
      ? '当前读取已安装的桌面 App（生产端口）'
      : '当前读取 task dev-server 起的网关（测试端口）';
    note.textContent = isProd ? '生产' : '测试';
  }
  btn.addEventListener('click', function () {
    var next = current() === 'prod' ? 'dev' : 'prod';
    document.cookie = KEY + '=' + next + '; path=/; max-age=31536000';
    // Reload, not a soft refresh: the cookie changes which upstream the proxy
    // talks to, so every in-flight panel must be re-read from the new backend.
    location.reload();
  });
  paint();
})();
</script>
`;

/**
 * Vite build for the console front-end.
 *
 * The output layout is not a free choice. `src/console.rs` compiles `ui/` into
 * the binary via `build.rs` and serves it at three fixed prefixes —
 * `/admin/src/*`, `/admin/style/*`, `/admin/assets/*` — and the settings below
 * keep every built file under those names so the routes and `build.rs`'s roots
 * need no change.
 *
 * ## One thing the console cannot do without
 *
 * `console.rs` injects the server bridge by replacing the literal
 * `<script type="module" src="src/main.js"></script>` in the page. A Vite
 * build rewrites that tag (hash, `crossorigin`, moved into `<head>`) and the
 * replacement would silently find nothing — a server console with no IPC at
 * all. So the plugin at the bottom re-emits the tag in exactly that shape, with
 * the hashed file as its `src`; the console's string match then works
 * unchanged. `ui/tools/check-dist.mjs` asserts the tag is there.
 *
 * `ui/` is never modified: it stays the source tree the checks and `ui-dev`
 * read. `ui-dist/` is the build output (gitignored).
 */
export default defineConfig({
  root: resolve(here, 'ui'),
  base: './', // relative URLs, so the page works from any /admin/ mount
  publicDir: false, // `ui/` has no public/ dir; assets come from imports

  build: {
    outDir: resolve(here, 'ui-dist'),
    emptyOutDir: true,
    // Never inline assets as data URIs: `build.rs` embeds files into the
    // binary, and a data URI would be a string inside a JS chunk instead.
    assetsInlineLimit: 0,
    modulePreload: { polyfill: false },
    rollupOptions: {
      input: {
        main: resolve(here, 'ui/index.html'),
        // The Tauri window's start page. `tauri.conf.json` sets it as the
        // window `url`, so it must exist in `frontendDist` — and `frontendDist`
        // is `ui-dist/` now, not `ui/`. Without this entry Vite never emits it,
        // and the window opens on a missing file: a blank app, with the console
        // itself perfectly healthy on its own port. It has no imports, but it
        // does reference `assets/logo.svg`, which the HTML pipeline rewrites.
        bootstrap: resolve(here, 'ui/bootstrap.html'),
      },
      output: {
        // Chunks: `src/` — the console's `/admin/src/*` route.
        entryFileNames: 'src/[name]-[hash].js',
        chunkFileNames: 'src/[name]-[hash].js',
        // Stylesheets: `style/`; everything else (images, fonts): `assets/`.
        // HTML inputs keep their own name at the root: the window loads
        // `bootstrap.html` by that exact path.
        assetFileNames: (info) => {
          const name = info.name || '';
          if (name.endsWith('.css')) return 'style/[name]-[hash][extname]';
          return 'assets/[name]-[hash][extname]';
        },
      },
    },
  },

  plugins: [
    backendSwitch(),
    {
      name: 'proxy-rs:console-script-tag',
      // Build only. In dev there is no hashed entry to restore, and the source
      // `index.html` already carries the tag in the shape `console.rs` matches;
      // running this there would (and did) fail every page load.
      apply: 'build',
      /**
       * Re-emit the entry script in the exact form `console.rs` searches for.
       *
       * Runs after Vite has produced its HTML, so it can pick the hashed entry
       * out of the generated tag rather than guessing the name.
       */
      transformIndexHtml: {
        order: 'post',
        // `bootstrap.html` is also an HTML input now, and it has no module
        // entry — only the console page is rewritten here.
        handler(html, ctx) {
          if (!/index\.html$/.test(ctx.filename || ctx.path || '')) return html;
          const m = /<script type="module" crossorigin src="\.\/(src\/main-[^"]+\.js)"><\/script>/.exec(html);
          if (!m) {
            throw new Error(
              'proxy-rs:console-script-tag: could not find the built entry script in index.html. '
              + 'console.rs injects the server bridge by matching this tag; update both together.',
            );
          }
          return html.replace(
            m[0],
            `<script type="module" src="${m[1]}"></script>`,
          );
        },
      },
    },
    {
      name: 'proxy-rs:backend-switch',
      /**
       * The `ui-dev` header control: a DEV badge and a 测试/生产 backend switch.
       *
       * **Serve only.** This must never reach a build: the desktop app and the
       * server console both ship `ui-dist/`, and a "switch backend" button in
       * production would either do nothing or, worse, claim to. Injecting it
       * from the config — rather than writing it into `ui/index.html` — is what
       * guarantees it exists only where it works.
       *
       * The switch writes the `proxy_rs_backend` cookie and reloads. The cookie
       * is read by the proxy above, so the *whole* page (page, RPC, `/api/*`)
       * moves to the other backend together — a per-request toggle that only
       * changed one transport would show data from two backends at once.
       */
      apply: 'serve',
      transformIndexHtml(html) {
        return html.replace('</body>', `${DEV_BADGE_MARKUP}\n</body>`);
      },
    },
  ],

  server: {
    // The dev loop (`task ui-dev`) runs beside the gateway, so it must not share
    // the gateway's port. `ui/tools/dev.mjs` starts Vite with `createServer`
    // and passes this same config, so the port lives here, in one place.
    host: '127.0.0.1',
    port: Number(process.env.PORT) || 4300,
    strictPort: true,
    /**
     * Backend targets, and the switch between them.
     *
     * Two named targets, chosen per request by a cookie so the page can flip
     * between them without restarting Vite. The cookie is set by the header
     * control `ui-dev` injects (see `proxy-rs:backend-switch`).
     *
     *   dev (default) — the headless `proxy-rs-server` that `task dev-server`
     *     starts. Its console is mounted on the data plane
     *     (`router::with_admin`), so `/admin/*` and `/api/*` share one port.
     *
     *   prod — the installed desktop app. Its console is a separate listener on
     *     `port + 1` (`start_console_listener`), so `/admin/*` goes one port up.
     *
     * Ports come from the environment so `task` can keep `dev-server` and
     * `ui-dev` in step; the defaults are the documented ones.
     *
     * ## How a request picks its upstream
     *
     * `/api` and `/admin` are two proxy entries, each with a **placeholder**
     * target. The `proxy-rs:proxy-target` plugin below rewrites that entry's
     * `target` in place before each proxied request, per the cookie.
     *
     * The placeholder is not cosmetic: http-proxy reads `target` *before* it
     * emits `proxyReq` (its `stream` pass starts the request), so retargeting
     * from `configure`/`proxyReq` is already too late. Vite builds the proxy
     * once and passes http-proxy the same options object on every request, so
     * mutating `target` still has effect — and is safe because the wrapper
     * sets all four targets and then hands the request straight on, in the
     * same synchronous turn, with no `await` in between (which is also why the
     * wrapper must not become async later).
     *
     * Two earlier attempts are worth remembering, because both failed silently:
     *
     *  - `router: (req) => target` on the entry. **Vite 5 has no `router`**:
     *    the option is accepted and ignored, so every request went to `target`
     *    (the dev port) and the switch did nothing.
     *  - One entry per (backend, prefix) with `bypass` hopping between them.
     *    `bypass` returning a string means *"a middleware rewrote the URL"* —
     *    Vite calls `next()` and its own middleware answers — not "try the next
     *    proxy entry". The hop rewrote the URL to a proxy key, nothing matched
     *    it, and the SPA fallback served `index.html` as a 200.
     *
     * A proxy key is a plain `startsWith` test, so the two must not overlap:
     * `/api/*` and `/admin/*` are disjoint, and `/admin` (no trailing slash)
     * still matches `POST /admin/api/rpc/<cmd>`.
     */
    proxy: {
      '/api': { target: PLACEHOLDER, changeOrigin: false },
      '/admin': { target: PLACEHOLDER, changeOrigin: false },
    },
  },
});
