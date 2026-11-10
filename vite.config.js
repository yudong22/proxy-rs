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

/**
 * The token `task dev-server` starts the test gateway with.
 *
 * `Taskfile.yaml` hard-codes `PROXY_API_TOKEN=dev` for that instance; this file
 * has to agree with it, because the auto-login below submits it. Kept next to
 * the port constants for the same reason they are here: the two sides of the
 * dev loop must not each carry their own copy of the number.
 *
 * Only ever used against the **test** backend. The production app has no token
 * at all (the process is the operator), so there is nothing to submit.
 */
const DEV_API_TOKEN = process.env.DEV_API_TOKEN || 'dev';

/** `proxy_rs_backend=prod` in the cookie jar means the installed app. */
const wantsProd = (req) =>
  /(?:^|;\s*)proxy_rs_backend=prod(?:;|$)/.test(req.headers?.cookie || '');

/** The `proxy_rs_admin=<value>` the browser sent, or `''`. */
function adminCookie(req) {
  const m = /(?:^|;\s*)proxy_rs_admin=([^;]+)/.exec(req.headers?.cookie || '');
  return m ? m[1] : '';
}

/**
 * Auto-login the **test** backend, so `ui-dev` opens a working console.
 *
 * ## The problem this removes
 *
 * The headless gateway requires `PROXY_API_TOKEN` (it is a server, and
 * `require_session` refuses every RPC without a session). So every fresh Vite
 * origin — a new port, or a cleared cookie jar — lands on the login page and
 * asks for a token that only `Taskfile.yaml` knows: `dev`. That is a manual
 * step in a hot-reload loop, paid again on every `npm run dev` restart, and it
 * is what "dev 环境登录问题" means in practice.
 *
 * ## Why a login POST rather than minting a cookie directly
 *
 * The Vite process cannot fabricate a valid session: sessions live in the
 * gateway's memory (`AdminSessions`), and the only two ways to get one are the
 * login endpoint and the desktop-only `desktop-boot`. So this performs the real
 * login — one form POST with the dev token — and hands the gateway's own
 * `Set-Cookie` through to the browser. Nothing is bypassed; the dev loop simply
 * supplies the token it already knows.
 *
 * ## Why it is safe
 *
 *  * **dev backend only.** `wantsProd(req)` short-circuits first, so the
 *    production console is never auto-logged-in (it has no token to submit, and
 *    its `desktop-boot` path already exists).
 *  * **loopback only.** `/admin/` is reachable from the network when someone
 *    runs Vite with a wider bind; auto-login there would hand a session to a
 *    stranger. `host: '127.0.0.1'` above is the first gate, and this checks the
 *    same thing again rather than trusting config.
 *  * **opt-out.** `DEV_AUTO_LOGIN=0` disables it for someone who wants to see
 *    the real login page (e.g. to work on that page).
 *  * It never runs in a build. There is no `apply` here because this is a plain
 *    function, not a plugin: its only caller is the serve-only request wrapper
 *    in `backendSwitch` (`apply: 'serve'`), so a build has no code path to it.
 *    `npm run build:ui` output contains none of it.
 *
 * The cookie is only minted when the request carries **no** `proxy_rs_admin`:
 * once the browser has a session, this stays out of the way entirely, including
 * after a logout (`clear_cookie_header` removes the cookie, so the next load
 * logs in again — which is what a dev loop should do).
 */
function autoLoginDev(req, res, fallthrough) {
  // Opt-out is a plain env check, not a plugin toggle: the caller decides
  // whether to route here, so there is no half-registered state to reason about.
  if (process.env.DEV_AUTO_LOGIN === '0') return fallthrough();

  const target = `${BACKEND_HOST}:${DEV_PORT}`;
  // No `next`: the browser never leaves the URL it asked for, so the hash
  // (`#sessions/<uuid>`, `#logs`, …) is still in the address bar when the page
  // loads. Passing a tab through the form would *lose* it — a fragment is never
  // sent to the server, so the server cannot echo it back, and the redirect
  // would land on `/admin/` stripped of the very deep link the request was for.
  const body = `token=${encodeURIComponent(DEV_API_TOKEN)}`;

  // The POST is async but the request listener is not; the answer is written
  // when it resolves. Until then nothing else touches this `res`, which is what
  // keeps the ordering safe without making the whole stack async.
  fetch(`${target}/admin/login`, {
    method: 'POST',
    headers: { 'content-type': 'application/x-www-form-urlencoded' },
    body,
    redirect: 'manual', // we need the `Set-Cookie`, not the navigation
  })
    .then((upstream) => {
      const setCookie = upstream.headers.get('set-cookie');
      // **A successful login answers `303`, so `upstream.ok` is false.**
      //
      // That is the whole point of the endpoint: it is consumed by a real form
      // submission, and the browser needs a redirect, not a JSON body
      // (`login_handler` answers 303 for a form post and a JSON 200 only for a
      // script). Testing `ok` here silently discarded the cookie on every
      // attempt — the page then rendered the login page as if auto-login had
      // never been written, which reads exactly like "the feature does not
      // work" rather than "the status check was wrong".
      //
      // What is checked instead: the response carries our session cookie. That
      // is the actual evidence of success, and it is what gets forwarded.
      const accepted = upstream.status === 303 || upstream.status === 302 || upstream.ok;
      if (!accepted || !setCookie) return fallthrough();

      res.statusCode = 302;
      res.setHeader('set-cookie', setCookie);
      // Back to the exact URL requested; the freshly set cookie makes the retry
      // succeed, and the fragment survives because the browser never navigated
      // away from it.
      res.setHeader('location', req.url || '/admin/');
      res.end();
    })
    .catch(() => fallthrough());
}

/**
 * Whether this request is one the auto-login should answer.
 *
 * Deliberately narrow: a **GET of the console page itself**, on the dev
 * backend, from loopback. Anything else — an RPC call, an asset, a POST, the
 * login endpoint — is left to the normal proxy path.
 *
 * ## Why the session cookie is *validated*, not merely detected
 *
 * The first version skipped auto-login whenever a `proxy_rs_admin` cookie was
 * present. That is wrong in exactly the case this feature exists for: the dev
 * gateway keeps sessions in memory, so **restarting it invalidates every
 * cookie it ever issued** while the browser keeps sending the old one. The
 * candidate test then said "already logged in", auto-login never ran, the
 * gateway answered the login page, and the operator was asked for a token that
 * the loop had just been taught to supply — the same manual step, now with a
 * misleading reason.
 *
 * So the cookie is only a hint. Presence short-circuits nothing; validity is
 * asked of the gateway itself (`GET /admin/api/session`, which is 200 for a
 * live session and 401 otherwise). That is one cheap loopback request on a page
 * load, and it makes "restart the gateway, reload the page, you are in" true.
 */
function isDevAutoLoginCandidate(req) {
  if (req.method !== 'GET') return false;
  if (wantsProd(req)) return false;
  // Loopback only, re-checked rather than assumed from `server.host`.
  const remote = req.socket?.remoteAddress || '';
  if (!/^(::1|::ffff:127\.|127\.)/.test(remote)) return false;

  const url = (req.url || '').split('?')[0];
  // The console page is served at `/admin/` (and `/admin`), which the proxy
  // entry forwards to the gateway. `/admin/login` is excluded by the exact
  // match below — auto-logging-in *from* the login page would make it
  // impossible to look at that page, which is a worse trade than one manual
  // token entry in that one case.
  if (url !== '/admin/' && url !== '/admin') return false;

  // A cookie's *presence* is not evidence of a live session — see above. The
  // caller resolves that (async) via `hasLiveSession`; this only narrows the
  // request down to "a console page load that might need a login".
  return true;
}

/**
 * Ask the gateway whether the request's session cookie is still good.
 *
 * Returns `true` when it is, so auto-login stays out of the way; `false` when
 * there is no cookie, or the gateway rejects it (401) — which is the case after
 * a gateway restart, and the reason this exists at all.
 */
async function hasLiveSession(req) {
  if (!adminCookie(req)) return false;
  try {
    const probe = await fetch(`${BACKEND_HOST}:${DEV_PORT}/admin/api/session`, {
      headers: { cookie: adminCookie(req) ? `proxy_rs_admin=${adminCookie(req)}` : '' },
    });
    return probe.ok;
  } catch {
    // Gateway unreachable: report "no live session" so the normal path runs and
    // the browser sees the real error (a proxy failure), not a silent skip.
    return false;
  }
}

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

        // Send the bare root to the console before anything else.
        //
        // `ui-dev` prints `http://127.0.0.1:4300/` as its address, and that URL
        // used to serve Vite's own copy of `ui/index.html` — the *source* page,
        // which has no gateway bridge and is not under `/admin`. The console
        // decides its transport from `location.pathname.startsWith('/admin')`
        // (`core/ipc.js`), so from `/` every `invoke` fell through to the
        // "[Tauri Mock]" no-op and answered `null`: the page rendered, and every
        // panel was empty. That reads as "4300 is all mocks", which is exactly
        // what it was — the mock branch, reached by opening the wrong URL.
        //
        // There is no console at `/`: the gateway serves the real page at
        // `/admin/` only, and that path is also what carries the cookie
        // (`Path=/admin`) and the auto-login. So `/` is redirected instead of
        // being made to work, which keeps one entry point rather than two that
        // behave differently.
        const pathname = (req.url || '').split('?')[0];
        if (req.method === 'GET' && (pathname === '/' || pathname === '')) {
          res.statusCode = 302;
          res.setHeader('location', `/admin/${req.url.slice(pathname.length)}`);
          res.end();
          return;
        }

        // Two keys only; `proxy` is a literal in this file, not user config.
        proxy['/admin'].target = backend === 'prod' ? prodTarget('admin') : `${BACKEND_HOST}:${DEV_PORT}`;
        proxy['/api'].target = backend === 'prod' ? prodTarget('api') : `${BACKEND_HOST}:${DEV_PORT}`;

        // Auto-login has to be decided **here**, not in a plugin middleware.
        //
        // A plugin's `configureServer` middleware also runs inside this stack,
        // but `connectStack` is invoked by the line below — by the time that
        // middleware sees the request, the proxy entry with this request's
        // target has already been consulted and the page answered. (Measured:
        // registering a second plugin and logging inside it shows it running,
        // yet the page still comes back 200 with no `Set-Cookie`.) So the
        // decision lives on the same path that picks the upstream.
        if (backend === 'dev' && isDevAutoLoginCandidate(req)) {
          // A present-but-stale cookie is decided asynchronously (it needs one
          // loopback round trip); a missing one is already known to need login.
          // Either way nothing else touches `res` until one of them answers.
          const needsLogin = adminCookie(req) ? hasLiveSession(req).then((live) => !live) : true;
          Promise.resolve(needsLogin)
            .then((needs) => (needs ? autoLoginDev(req, res, () => connectStack(req, res)) : connectStack(req, res)))
            .catch(() => connectStack(req, res));
          return;
        }
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
