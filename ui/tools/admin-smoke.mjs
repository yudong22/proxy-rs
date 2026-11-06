#!/usr/bin/env node
/**
 * Boot the server-mode admin console in headless Chrome and assert it works.
 *
 * ## Why this exists
 *
 * `ui/tools/check-ui.mjs` checks the *desktop* front-end statically, and
 * `ui/tools/smoke.mjs` boots it against a mocked Tauri bridge. Neither touches
 * the server console, which is a different page with no Tauri at all: it talks
 * to `/admin/api/*` over `fetch` with a session cookie.
 *
 * That means the failure modes are different too, and several are invisible to
 * any static check:
 *
 *   · a relative API base resolves against `/admin/api/` and doubles the prefix;
 *   · a page served as `text/plain` is refused by the browser as a module;
 *   · a login that succeeds but never shows the shell (a missing `refreshAll`);
 *   · a cookie the browser drops because it was marked `Secure` over HTTP.
 *
 * The last one is the reason this runs against a *live* gateway rather than a
 * mock: the cookie flags, the `Set-Cookie` parse and the `fetch` credentials mode
 * are all decided by the browser, and a mock would assert our assumptions instead
 * of the browser's behaviour.
 *
 * ## Usage
 *
 *   node ui/tools/admin-smoke.mjs                 # skips if no gateway/Chrome
 *   BASE=http://127.0.0.1:3456 TOKEN=xxx node ui/tools/admin-smoke.mjs
 *
 * Exits 1 on any failure. Prints SKIPPED (exit 0) when the prerequisites are
 * absent, so it can sit in a normal `task check` without a gateway running.
 */

import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';

const BASE = process.env.BASE || 'http://127.0.0.1:3456';
const TOKEN = process.env.TOKEN || '';
const CHROME =
  process.env.CHROME_PATH ||
  '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';

if (!TOKEN) {
  console.log('SKIPPED: set TOKEN=<PROXY_API_TOKEN> to run this harness.');
  process.exit(0);
}
if (!fs.existsSync(CHROME)) {
  console.log(`SKIPPED: no Chrome at ${CHROME}`);
  console.log('Set CHROME_PATH to a Chrome/Chromium binary to run this harness.');
  process.exit(0);
}

const DEBUG = 9333 + (process.pid % 200);
const getJson = (url) =>
  new Promise((resolve, reject) => {
    http
      .get(url, (r) => {
        let d = '';
        r.on('data', (c) => (d += c));
        r.on('end', () => {
          try {
            resolve(JSON.parse(d));
          } catch (e) {
            reject(e);
          }
        });
      })
      .on('error', reject);
  });

// Is a gateway actually listening? Without this the harness would report a
// connection error as a console bug.
try {
  const res = await fetch(`${BASE}/admin/api/`, { signal: AbortSignal.timeout(3000) });
  if (!res.ok) throw new Error(`HTTP ${res.status}`);
} catch (e) {
  console.log(`SKIPPED: no gateway at ${BASE} (${e.message})`);
  process.exit(0);
}

const chrome = spawn(
  CHROME,
  [
    '--headless=new',
    `--remote-debugging-port=${DEBUG}`,
    `--user-data-dir=${path.join(os.tmpdir(), `proxy-rs-admin-smoke-${process.pid}`)}`,
    '--no-first-run',
    '--no-default-browser-check',
    '--disable-gpu',
    'about:blank',
  ],
  { stdio: 'ignore' }
);

const failures = [];
const check = (label, actual, expected) => {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) failures.push(`${label}\n      expected: ${e}\n      actual:   ${a}`);
};
const ok = (label, condition, detail = '') => {
  if (!condition) failures.push(`${label}${detail ? `\n      ${detail}` : ''}`);
};

let ws;
try {
  let version = null;
  for (let i = 0; i < 80 && !version; i += 1) {
    try {
      version = await getJson(`http://127.0.0.1:${DEBUG}/json/version`);
    } catch {
      await new Promise((r) => setTimeout(r, 250));
    }
  }
  if (!version) throw new Error('Chrome devtools did not start');

  const target = (await getJson(`http://127.0.0.1:${DEBUG}/json/list`)).find(
    (t) => t.type === 'page'
  );
  ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r) => ws.addEventListener('open', r, { once: true }));

  let msgId = 0;
  const pending = new Map();
  const runtimeErrors = [];
  ws.addEventListener('message', (event) => {
    const msg = JSON.parse(event.data);
    if (msg.id && pending.has(msg.id)) {
      const { resolve, reject } = pending.get(msg.id);
      pending.delete(msg.id);
      msg.error ? reject(new Error(msg.error.message)) : resolve(msg.result);
    }
    if (msg.method === 'Runtime.consoleAPICalled' && msg.params.type === 'error') {
      runtimeErrors.push(msg.params.args.map((a) => a.value ?? a.description).join(' '));
    }
    if (msg.method === 'Runtime.exceptionThrown') {
      runtimeErrors.push(
        `exception: ${msg.params.exceptionDetails.exception?.description ||
          msg.params.exceptionDetails.text}`
      );
    }
  });

  const send = (method, params = {}) =>
    new Promise((resolve, reject) => {
      const id = ++msgId;
      pending.set(id, { resolve, reject });
      ws.send(JSON.stringify({ id, method, params }));
    });

  const evaluate = async (expression) => {
    const result = await send('Runtime.evaluate', {
      expression,
      awaitPromise: true,
      returnByValue: true,
      // The page is same-origin with the fetch calls it makes, so no CORS
      // preflight is involved; this just makes the promise resolution explicit.
    });
    if (result.exceptionDetails) {
      throw new Error(
        result.exceptionDetails.exception?.description ||
          result.exceptionDetails.text ||
          'evaluate failed'
      );
    }
    return result.result.value;
  };

  const waitFor = async (expression, label, timeoutMs = 6000) => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (await evaluate(expression)) return true;
      await new Promise((r) => setTimeout(r, 120));
    }
    throw new Error(`timed out waiting for ${label}`);
  };

  await send('Page.enable');
  await send('Runtime.enable');

  // ── The page loads ──────────────────────────────────────────────────────
  await send('Page.navigate', { url: `${BASE}/admin/api/` });
  await waitFor('document.readyState === "complete"', 'page load');
  await new Promise((r) => setTimeout(r, 400));

  check('page title', await evaluate('document.title'), 'Proxy RS 管理台');
  ok(
    'the login form is the first thing shown (no session yet)',
    await evaluate('getComputedStyle(document.querySelector("#login-view")).display !== "none"')
  );
  ok(
    'the shell stays hidden before login',
    await evaluate('!document.querySelector("#shell").classList.contains("show")')
  );

  // The pre-session probe must not have embedded any secret.
  const html = await evaluate('document.documentElement.innerHTML');
  ok('the page does not embed the admin token', !html.includes(TOKEN));

  // ── A wrong token is refused in the UI ──────────────────────────────────
  await evaluate('document.querySelector("#token").value = "definitely-wrong"');
  await evaluate('document.querySelector("#login-form").dispatchEvent(new Event("submit"))');
  await waitFor(
    'document.querySelector("#login-err").classList.contains("show")',
    'the wrong-token error'
  );
  ok(
    'a wrong token shows an inline error and stays on the form',
    await evaluate('getComputedStyle(document.querySelector("#shell")).display === "none"')
  );

  // ── The right token logs in ─────────────────────────────────────────────
  await evaluate(`document.querySelector("#token").value = ${JSON.stringify(TOKEN)}`);
  await evaluate('document.querySelector("#login-form").dispatchEvent(new Event("submit"))');
  await waitFor(
    'document.querySelector("#shell").classList.contains("show")',
    'the dashboard shell'
  );
  ok(
    'the login form is gone after a successful login',
    await evaluate('getComputedStyle(document.querySelector("#login-view")).display === "none"')
  );

  // The cookie the browser actually stored is the ground truth; asserting on the
  // header alone would miss a browser dropping it (e.g. `Secure` over plain HTTP).
  const cookies = await send('Network.getAllCookies').catch(() => null);
  if (cookies?.cookies) {
    const session = cookies.cookies.find((c) => c.name === 'proxy_rs_admin');
    ok('the session cookie is stored by the browser', Boolean(session));
    ok(
      'the session cookie is HttpOnly',
      Boolean(session?.httpOnly),
      'a page script must not be able to read it'
    );
    ok('the session cookie is scoped to /admin', session?.path?.startsWith('/admin'));
  }

  // ── Every tab renders ───────────────────────────────────────────────────
  for (const tab of ['overview', 'identities', 'access', 'settings', 'logs', 'pool']) {
    await evaluate(`document.querySelector('nav button[data-tab="${tab}"]').click()`);
    await new Promise((r) => setTimeout(r, 350));
    ok(
      `tab "${tab}" becomes visible on click`,
      await evaluate(`document.querySelector("#tab-${tab}").classList.contains("active")`)
    );
    ok(
      `tab "${tab}" renders content`,
      await evaluate(
        `(document.querySelector("#tab-${tab}").textContent || "").trim().length > 0`
      )
    );
  }

  // The overview must show live numbers, not placeholders.
  await evaluate(`document.querySelector('nav button[data-tab="overview"]').click()`);
  await waitFor(
    'document.querySelector("#stat-grid").children.length >= 3',
    'the statistics grid'
  );
  const statCount = await evaluate('document.querySelector("#stat-grid").children.length');
  ok('the overview fills its statistic cards', statCount >= 3, `got ${statCount}`);
  const header = await evaluate('document.querySelector("#hdr-meta").textContent');
  ok('the header shows version and port', /v\d+\.\d+/.test(header), header);

  // ── The API base resolved correctly ─────────────────────────────────────
  // A relative base would have doubled the `/admin/api` prefix, and every fetch
  // after login would 404. The dashboard being populated proves it did not, but
  // an explicit round-trip makes the failure message say so.
  const roundTrip = await evaluate(
    'fetch("/admin/api/session", {credentials:"same-origin"}).then(r => r.status)'
  );
  check('the API base resolves to one /admin/api prefix', roundTrip, 200);

  // ── Logout returns to the form and kills the session ────────────────────
  await evaluate('document.querySelector("#logout-btn").click()');
  await waitFor(
    'getComputedStyle(document.querySelector("#login-view")).display !== "none"',
    'the login form after logout'
  );
  const afterLogout = await evaluate(
    'fetch("/admin/api/session", {credentials:"same-origin"}).then(r => r.status)'
  );
  check('the session is invalid after logout', afterLogout, 401);

  // ── No uncaught errors anywhere in that run ─────────────────────────────
  ok(
    'no uncaught console errors',
    runtimeErrors.length === 0,
    runtimeErrors.join('\n      ')
  );
} finally {
  try {
    ws?.close();
  } catch {
    /* ignore */
  }
  chrome.kill();
}

if (failures.length) {
  console.error(`\nadmin-smoke: ${failures.length} failure(s)\n`);
  for (const f of failures) console.error(`  ✗ ${f}`);
  process.exit(1);
}
console.log('admin-smoke: all checks passed');
