#!/usr/bin/env node
/**
 * Front-end regression harness.
 *
 * Boots the real page in headless Chrome against a mock Tauri bridge, so the
 * whole boot path runs — render, init, load — without a Rust build or a window.
 * This is what makes behaviour-preservation checkable after a refactor: the
 * assertions below are the values the UI produced before the module split, and a
 * regression shows up as a diff rather than as "looks fine".
 *
 * Run: node ui/tools/smoke.mjs
 *
 * It covers the two things a file split is most likely to break:
 *   - the boot path renders real data into every view (counts + filled values)
 *   - cross-module integration points still fire: the command palette's dynamic
 *     import of the settings view, the account pool's repaint after OAuth, and
 *     the pool import/export dialogs (preview enables 确认导入, apply repaints).
 *
 * Since the overview statistics and the request-log table read `GET /api/*`
 * from the listener (falling back to IPC), this harness also serves those two
 * endpoints and asserts they were hit — so a change that silently routes the
 * dashboard back through an unregistered command fails here instead of only in
 * a built app.
 *
 * ## Layout
 *
 * This file owns the harness: the fixture server, Chrome, the CDP connection and
 * the failure report. The scenarios live in `smoke/` and read as behaviour:
 *
 *   fixtures.mjs               MOCK, EXPECTED and the two /api/* payloads
 *   scenarios-boot.mjs         the boot path and the automation dialog
 *   scenarios-views.mjs        request logs, 会话 transcript, hash routing
 *   scenarios-integration.mjs  palette + OAuth, pool transfer, login recovery
 *
 * They share a `ctx` of `{ send, check, failures, apiHits, EXPECTED, MOCK }`
 * rather than module-level state, so each one can be read without asking what
 * else has already run. The one exception is the boot payload: it is captured
 * once and returned, because the growth run below repaints the page it
 * describes — which is why the run order in `main` is load-bearing.
 */

import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

import { MOCK, EXPECTED, API } from './smoke/fixtures.mjs';
import {
  checkBoot,
  checkBootDerived,
  checkAutomationDialog,
  checkGrowthProgress,
} from './smoke/scenarios-boot.mjs';
import {
  checkRequestLogs,
  checkSessions,
  checkHashRouting,
  returnToOverview,
} from './smoke/scenarios-views.mjs';
import {
  checkPaletteAndOAuth,
  checkPoolTransfer,
  checkLoginRecovery,
} from './smoke/scenarios-integration.mjs';
import { checkOverviewLayout } from './smoke/scenarios-overview.mjs';
import { checkBootstrapPage } from './smoke/scenarios-bootstrap.mjs';
import { checkEveryPaneRenders } from './smoke/scenarios-pages.mjs';
import { checkClaudePermissionMode, checkClassifierSwitchPlacement } from './smoke/scenarios-claude-permission.mjs';
import { checkEmptyStates, checkEmptySessionList } from './smoke/scenarios-states.mjs';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
// Source by default. `UI_DIR=ui-dist` runs the same assertions against the Vite
// build instead, which is the only way to know the *shipped* page behaves — a
// build can drop a module, reorder the cascade, or rewrite the entry tag while
// the source tree stays green.
const UI = path.resolve(REPO, process.env.UI_DIR || 'ui');
const PORT = 4197;
const DEBUG = 9357;

/**
 * The Chrome binary, by platform.
 *
 * `CHROME_PATH` wins. The defaults cover a macOS dev machine and the GitHub
 * `ubuntu-latest` image, which ships Chrome as `/usr/bin/google-chrome` — so CI
 * does not have to remember to export anything.
 */
function chromePath() {
  const fromEnv = process.env.CHROME_PATH;
  if (fromEnv) return fromEnv;
  if (process.platform === 'darwin') return '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
  for (const p of ['/usr/bin/google-chrome', '/usr/bin/google-chrome-stable', '/usr/bin/chromium', '/usr/bin/chromium-browser']) {
    if (fs.existsSync(p)) return p;
  }
  return '/usr/bin/google-chrome';
}

const CHROME = chromePath();

if (!fs.existsSync(CHROME)) {
  // An explicit, loud skip rather than a failure: this keeps `task check`
  // usable on a machine without Chrome. A smoke test that silently reported
  // success when it never ran would be worse than having none.
  //
  // CI must NOT rely on this: a skip is exit 0, i.e. a green tick over a suite
  // that never ran. `ci.yml` sets `SMOKE_REQUIRED=1` so a missing browser there
  // is a failure instead.
  if (process.env.SMOKE_REQUIRED) {
    console.error(`FAILED: no Chrome at ${CHROME} (SMOKE_REQUIRED is set, so skipping is not allowed)`);
    process.exit(1);
  }
  console.log(`SKIPPED: no Chrome at ${CHROME}`);
  console.log('Set CHROME_PATH to a Chrome/Chromium binary to run this harness.');
  process.exit(0);
}

const apiHits = [];

const server = http.createServer((req, res) => {
  let p = decodeURIComponent(req.url.split('?')[0]);
  if (p === '/') p = '/index.html';
  if (Object.prototype.hasOwnProperty.call(API, p)) {
    apiHits.push(p);
    res.writeHead(200, { 'Content-Type': 'application/json', 'Cache-Control': 'no-store' });
    res.end(JSON.stringify(API[p]));
    return;
  }
  const file = path.join(UI, p);
  if (!file.startsWith(UI) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    res.writeHead(404); res.end('not found'); return;
  }
  const types = { '.html': 'text/html', '.css': 'text/css', '.js': 'text/javascript', '.svg': 'image/svg+xml' };
  res.writeHead(200, { 'Content-Type': types[path.extname(file)] || 'application/octet-stream' });
  fs.createReadStream(file).pipe(res);
});

const getJson = (url) => new Promise((resolve, reject) => {
  http.get(url, (r) => {
    let d = '';
    r.on('data', (c) => { d += c; });
    r.on('end', () => { try { resolve(JSON.parse(d)); } catch (e) { reject(e); } });
  }).on('error', reject);
});

await new Promise((r) => server.listen(PORT, '127.0.0.1', r));
const chrome = spawn(CHROME, [
  '--headless=new', `--remote-debugging-port=${DEBUG}`,
  `--user-data-dir=${path.join(os.tmpdir(), 'proxy-rs-ui-smoke')}`,
  '--no-first-run', '--no-default-browser-check', '--disable-gpu', 'about:blank',
], { stdio: 'ignore' });

const failures = [];
const check = (label, actual, expected) => {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) failures.push(`${label}\n      expected: ${e}\n      actual:   ${a}`);
};

/**
 * Run one scenario, turning a thrown error into a failure.
 *
 * A scenario asserts against what the page produced. When the page is broken —
 * a module that throws on entry, a pane that never rendered — the scenario's
 * own expression often throws first (`undefined.includes`), and without this
 * the whole harness died on the first one: `harness error: Cannot read
 * properties of undefined`, with every other assertion in the run unreported.
 *
 * Reporting it as a failure keeps the run going and names the scenario, which
 * is the difference between "something broke" and "this view is broken".
 */
const scenario = async (name, fn) => {
  try {
    return await fn();
  } catch (err) {
    failures.push(`${name} threw: ${err?.message || err}\n      ${String(err?.stack || '').split('\n').slice(0, 4).join('\n      ')}`);
    return undefined;
  }
};

let ws;
try {
  let version = null;
  for (let i = 0; i < 60 && !version; i += 1) {
    try { version = await getJson(`http://127.0.0.1:${DEBUG}/json/version`); }
    catch { await new Promise((r) => setTimeout(r, 250)); }
  }
  if (!version) throw new Error('Chrome devtools did not start');

  const target = (await getJson(`http://127.0.0.1:${DEBUG}/json/list`)).find((t) => t.type === 'page');
  ws = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((r) => ws.addEventListener('open', r, { once: true }));

  let msgId = 0;
  const pending = new Map();
  const runtimeErrors = [];
  ws.addEventListener('message', (ev) => {
    const m = JSON.parse(ev.data);
    if (m.id && pending.has(m.id)) {
      const { resolve, reject } = pending.get(m.id);
      pending.delete(m.id);
      m.error ? reject(new Error(JSON.stringify(m.error))) : resolve(m.result);
      return;
    }
    if (m.method === 'Runtime.exceptionThrown') {
      runtimeErrors.push(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text);
    }
    if (m.method === 'Runtime.consoleAPICalled' && m.params.type === 'error') {
      runtimeErrors.push((m.params.args || []).map((a) => a.value ?? a.description).join(' '));
    }
  });
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++msgId;
    pending.set(id, { resolve, reject });
    ws.send(JSON.stringify({ id, method, params }));
  });

  await send('Runtime.enable');
  await send('Page.enable');

  // The bridge must exist before any module evaluates so the real boot path runs.
  await send('Page.addScriptToEvaluateOnNewDocument', { source: `
    window.__CALLS__ = [];
    window.__INVOKE_ARGS__ = {};
    window.__MOCKDATA__ = ${JSON.stringify(MOCK)};
    // The dashboard reads the listener's /api/* paths; point them at this
    // harness so the HTTP leg runs against deterministic fixtures.
    window.__PROXY_API_BASE__ = 'http://127.0.0.1:${PORT}';
    window.__TAURI__ = {
      core: { invoke: (cmd, args) => { window.__CALLS__.push(cmd);
        window.__INVOKE_ARGS__[cmd] = args;
        // A scenario can hold commands by name (window.__HOLD_NAMES__), releasing
        // them with window.__RELEASE__(). Used by the overview-layout scenario,
        // which has to measure the page *before* its values land; interception at
        // the network layer cannot do that here, because every read may arrive
        // over this bridge instead (that is the point of core/api.js's fallback).
        // Read lazily on each call, not captured at boot: CDP runs injected
        // scripts in an unspecified order, and a scenario's names may be
        // registered after this one.
        if ((window.__HOLD_NAMES__ || []).includes(cmd)) {
          return new Promise((resolve) => {
            window.__HELD__.push(() => resolve(
              Object.prototype.hasOwnProperty.call(window.__MOCKDATA__, cmd)
                ? window.__MOCKDATA__[cmd] : null));
          });
        }
        // State-matrix hooks, set by a scenario before it navigates:
        //   __MOCK_ALL_NULL__  — every command answers null (no backend)
        //   __MOCK_OVERRIDE__  — one command's answer is replaced, by name
        // Both are read per call so the scenario can flip them between loads.
        if (window.__MOCK_ALL_NULL__) return Promise.resolve(null);
        const override = window.__MOCK_OVERRIDE__;
        if (override && Object.prototype.hasOwnProperty.call(override, cmd)) {
          return Promise.resolve(override[cmd]);
        }
        // get_growth_progress is take-and-clear on the real backend: the
        // console drains the buffer and the next poll sees nothing new. The
        // mock reproduces that, so a UI that kept re-reading the same lines
        // would show duplicates here too.
        if (cmd === 'get_growth_progress') {
          const lines = window.__MOCKDATA__.__growth_progress_drained
            ? []
            : (window.__MOCKDATA__.__growth_progress_events || []);
          window.__MOCKDATA__.__growth_progress_drained = true;
          return Promise.resolve({ lines });
        }
        return Promise.resolve(Object.prototype.hasOwnProperty.call(window.__MOCKDATA__, cmd)
          ? window.__MOCKDATA__[cmd] : null); } },
      // A real dispatcher: listen() records the handler and __emitTestEvent
      // fires it. An empty stub made every event-driven feature untestable --
      // the growth progress box could not be asserted at all, which is how a
      // "no progress shown" regression reached the user.
      event: {
        __handlers: {},
        listen: (name, handler) => {
          (window.__TAURI__.event.__handlers[name] ||= []).push(handler);
          return Promise.resolve(() => {});
        },
      },
    };
    window.__emitTestEvent = (name, payload) => {
      for (const h of window.__TAURI__.event.__handlers[name] || []) h({ payload });
    };
    // The resolvers waiting on a hold. The hold list lives in
    // window.__HOLD_NAMES__, which a scenario sets before its navigation.
    window.__HELD__ = [];
    window.__RELEASE__ = () => { for (const f of window.__HELD__.splice(0)) f(); };
    window.__heldCount__ = () => window.__HELD__.length;` });

  await send('Page.navigate', { url: `http://127.0.0.1:${PORT}/index.html` });
  await new Promise((r) => setTimeout(r, 2000));

  const ctx = { send, check, failures, apiHits, EXPECTED, MOCK };

  // Order matters. The boot payload is asserted before the growth run, because
  // that run repaints the credential rows the payload describes.
  // Order matters, and it is the order these assertions have always run in: the
  // boot payload is captured once, the automation dialog and the growth run are
  // driven, and only then is the rest of that payload asserted. The payload is a
  // snapshot, so later scenarios cannot change it — but keeping the original
  // sequence means a regression still reports itself at the same point it used
  // to.
  const b = await scenario('boot', () => checkBoot(ctx));
  await scenario('automation dialog', () => checkAutomationDialog(ctx));
  await scenario('growth progress', () => checkGrowthProgress(ctx));
  await scenario('boot (derived)', () => checkBootDerived(ctx, b));
  await scenario('request logs', () => checkRequestLogs(ctx));
  await scenario('sessions', () => checkSessions(ctx));
  await scenario('hash routing', () => checkHashRouting(ctx));
  await scenario('return to overview', () => returnToOverview(ctx));

  const i = await scenario('palette + OAuth', () => checkPaletteAndOAuth(ctx));
  await scenario('pool transfer', () => checkPoolTransfer(ctx));
  await scenario('login recovery', () => checkLoginRecovery(ctx));

  // Last, because it reloads the page and every scenario above worked against
  // the DOM that reloaded document replaces.
  await scenario('overview layout', () => checkOverviewLayout({ send, check, origin: `http://127.0.0.1:${PORT}` }));

  // Loads a different document (the start page), so it goes after every
  // scenario that works against the console.
  await scenario('bootstrap page', () => checkBootstrapPage({ send, check, origin: `http://127.0.0.1:${PORT}` }));

  // The non-happy states. Last of the page-replacing scenarios: each one
  // navigates and swaps the bridge, so anything after it starts from its state.
  await scenario('empty states', () => checkEmptyStates({ send, check, origin: `http://127.0.0.1:${PORT}` }));
  await scenario('empty session list', () => checkEmptySessionList({ send, check, origin: `http://127.0.0.1:${PORT}` }));

  // Every page and pane, loaded fresh at the end so the swapped bridge state
  // above cannot leak into it (a fresh document, default bridge).
  await scenario('every pane', () => checkEveryPaneRenders({ send, check, origin: `http://127.0.0.1:${PORT}` }));

  // After "every pane", which leaves the console on a freshly loaded document
  // with the default bridge — the state this scenario edits the settings form in.
  await scenario('claude permission mode', () => checkClaudePermissionMode({ send, check }));
  await scenario('classifier switch placement', () => checkClassifierSwitchPlacement({ send, check }));

  if (runtimeErrors.length) {
    failures.push(`runtime console errors:\n      ${runtimeErrors.join('\n      ')}`);
  }

  console.log(`boot assertions: counts, text, credential rows, default identity`);
  console.log(`integration: palette + OAuth repaint + pool import/export + login recovery`);
  console.log(`pages + states: every pane, loading/empty, bootstrap start page`);
  console.log(`mock commands exercised: ${i?.called?.length ?? 'n/a'}`);
  if (failures.length === 0) {
    console.log('\nall assertions passed');
    process.exitCode = 0;
  } else {
    console.log(`\n${failures.length} failure(s):`);
    for (const f of failures) console.log(`  - ${f}`);
    process.exitCode = 1;
  }
} catch (err) {
  console.error('harness error:', err.message);
  process.exitCode = 2;
} finally {
  ws?.close();
  chrome.kill();
  server.close();
}
