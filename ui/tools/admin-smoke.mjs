#!/usr/bin/env node
/**
 * Boot the server-hosted console in headless Chrome and assert it is the *real*
 * desktop front-end, still working over the HTTP bridge.
 *
 * ## Why this harness exists (and why it asserts on shared UI)
 *
 * The gateway serves `ui/index.html` verbatim — the same file, modules and
 * stylesheets the Tauri window loads — and the only difference is that
 * `src/core/ipc.js`'s `invoke()` is filled by `ui/server/bridge.js` instead of
 * Tauri IPC. The entire value of that design is that the two front-ends cannot
 * drift, which is only true if the shared UI actually *runs* this way.
 *
 * Several failure modes are invisible to any static check and to any unit test
 * of the bridge alone:
 *
 *   · the bridge script not being injected before `main.js` (blank page);
 *   · a command the desktop app calls but the RPC table forgot (every panel that
 *     uses it renders empty, with a console error the operator never sees);
 *   · a relative asset path that 404s only from the `/admin/` prefix;
 *   · a cookie the browser refuses because it was marked `Secure` over HTTP.
 *
 * ## Usage
 *
 *   BASE=http://127.0.0.1:3456 TOKEN=xxx node ui/tools/admin-smoke.mjs
 *
 * Prints SKIPPED (exit 0) when the prerequisites are absent, so it can sit in a
 * normal `task check` without a gateway running.
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
  console.log('Set CHROME_PATH to a Chrome/Chromium binary.');
  process.exit(0);
}

const DEBUG = 9400 + (process.pid % 200);
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

try {
  const res = await fetch(`${BASE}/admin/`, { signal: AbortSignal.timeout(3000) });
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

  const waitFor = async (expression, label, timeoutMs = 8000) => {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      if (await evaluate(expression)) return;
      await new Promise((r) => setTimeout(r, 120));
    }
    throw new Error(`timed out waiting for ${label}`);
  };

  await send('Page.enable');
  await send('Runtime.enable');

  // ── The login page is what an anonymous visitor gets ────────────────────
  await send('Page.navigate', { url: `${BASE}/admin/` });
  await waitFor('document.readyState === "complete"', 'page load');
  await new Promise((r) => setTimeout(r, 400));

  check('an anonymous visitor gets the login page', await evaluate('document.title'), 'Proxy RS · 登录');
  ok(
    'the login form is present',
    await evaluate('!!document.querySelector("#form") && !!document.querySelector("#token")')
  );

  // ── The form is shaped so a password manager offers to save the token ───
  // This is a functional requirement, not cosmetics: it is the criterion Chrome,
  // Safari and Firefox use, and a fetch-based login silently fails it.
  const formShape = await evaluate(`(() => {
    const f = document.querySelector('#form');
    const pw = f?.querySelector('input[type=password]');
    const un = f?.querySelector('input[autocomplete=username]');
    return {
      method: (f?.getAttribute('method') || '').toLowerCase(),
      action: f?.getAttribute('action') || '',
      pwAutocomplete: pw?.getAttribute('autocomplete') || '',
      hasUserField: !!un,
      // Password managers ignore a hidden username field.
      userVisible: un ? !!(un.offsetWidth || un.offsetHeight) : false,
    };
  })()`);
  check('the login form posts natively', formShape.method, 'post');
  check('the login form targets the page route', formShape.action, '/admin/login');
  check('the password field is marked for password managers', formShape.pwAutocomplete, 'current-password');
  check('a username field is paired with it', formShape.hasUserField, true);
  check('the username field is visible', formShape.userVisible, true);

  // ── A wrong token is refused, via the redirect the form now uses ────────
  await evaluate('document.querySelector("#token").value = "definitely-wrong"');
  await evaluate('document.querySelector("#form").requestSubmit()');
  // A native post navigates, so the failure arrives as `?e=invalid_token`.
  await waitFor('location.search.includes("e=")', 'the wrong-token redirect');
  await waitFor(
    'document.querySelector("#err")?.classList.contains("show")',
    'the wrong-token error to render'
  );
  ok(
    'a wrong token is reported and stays on the login page',
    await evaluate('location.pathname === "/admin/login"')
  );

  // ── The right token logs in and lands on the *shared* console ───────────
  await evaluate(`document.querySelector("#token").value = ${JSON.stringify(TOKEN)}`);
  await evaluate('document.querySelector("#form").requestSubmit()');
  // Wait for a console landmark, not `readyState`: the navigation happens as a
  // result of the submit, so polling readyState samples the *old* document and
  // races the new one.
  await waitFor(
    'document.querySelector("#tabs") && document.title === "Proxy RS"',
    'the console to load after login',
    15000
  );
  await new Promise((r) => setTimeout(r, 1200));

  // The decisive assertion: this is the desktop front-end, not a lookalike.
  check('the console is the desktop app (title)', await evaluate('document.title'), 'Proxy RS');
  ok(
    'the server bridge replaced Tauri IPC',
    await evaluate('typeof window.__TAURI__?.core?.invoke === "function"')
  );
  // Wait rather than check once: over the public internet the module graph and
  // the first RPC round trips take a beat longer than on loopback, and a bare
  // check here produced a false failure against production.
  await waitFor(
    'document.querySelectorAll(".tab").length === 3',
    'the tab bar to render'
  );
  ok(
    'the desktop tab bar rendered',
    (await evaluate('[...document.querySelectorAll(".tab")].map(t=>t.textContent.trim()).join("|")')) ===
      '运行概览|实时日志|服务设置'
  );
  ok(
    'the desktop header chrome rendered',
    await evaluate('!!document.querySelector(".header .logo")')
  );

  // ── Live data actually flows through the bridge ─────────────────────────
  await waitFor(
    'document.querySelector("#status-text")?.textContent?.includes("运行中")',
    'the status line to be filled from get_status'
  );
  const statusText = await evaluate('document.querySelector("#status-text").textContent');
  ok('the status line names the live port', /运行中 · 端口 \d+/.test(statusText), statusText);

  const shownVersion = await evaluate('document.querySelector("#app-version")?.textContent');
  ok(
    'the version comes from the gateway',
    /^v\d+\.\d+/.test(shownVersion || ''),
    String(shownVersion)
  );

  const cards = await evaluate(
    '[...document.querySelectorAll(".metric-label")].map(e=>e.textContent.trim().replace(/\\s+/g,"")).join(",")'
  );
  ok('the desktop metric cards rendered', cards.includes('请求总数') && cards.includes('Token总量'), cards);

  // The 剩余积分 card must carry the live identity, not an em dash. It was empty
  // for a while because the server's status payload omitted `current_identity`
  // while the desktop command had it — same front-end, a hole in the data.
  const points = await evaluate('document.querySelector("#metric-points")?.textContent.trim()');
  ok(
    'the 剩余积分 card shows a value, not a placeholder',
    points && points !== '—' && points !== '-',
    `got ${JSON.stringify(points)}`
  );

  const endpoints = await evaluate(
    '[...document.querySelectorAll(".kv dt")].map(d=>d.textContent.trim()).join(",")'
  );
  ok(
    'the endpoint reference table rendered',
    endpoints.includes('Messages API') && endpoints.includes('日志文件'),
    endpoints
  );

  // ── Every tab renders ──────────────────────────────────────────────────
  for (const tab of ['logs', 'settings', 'overview']) {
    await evaluate(`document.querySelector('.tab[data-tab="${tab}"]').click()`);
    await new Promise((r) => setTimeout(r, 700));
    const length = await evaluate(
      `(document.querySelector("#tab-${tab}")?.textContent || "").trim().length`
    );
    ok(`tab "${tab}" renders content`, length > 50, `text length ${length}`);
  }

  // ── The commands the settings view needs all route ─────────────────────
  // Asserted through the bridge directly: a missing RPC arm produces an empty
  // panel, which is exactly the failure this harness is for.
  for (const [command, args] of [
    ['get_providers', {}],
    ['wb_credentials_list', {}],
    ['api_keys_list', {}],
    ['wb_preferences', {}],
    ['client_accounts_list', {}],
    ['get_settings', {}],
    ['get_session_metrics', {}],
  ]) {
    const result = await evaluate(
      `window.__TAURI__.core.invoke(${JSON.stringify(command)}, ${JSON.stringify(args)})`
        + '.then(()=>"ok",e=>"ERR: "+e)'
    );
    check(`command ${command} is routed`, result, 'ok');
  }

  // ── The service toggle explains itself instead of lying ────────────────
  await evaluate(`document.querySelector('.tab[data-tab="overview"]').click()`);
  await new Promise((r) => setTimeout(r, 400));
  await evaluate('document.querySelector("#btn-toggle-service").click()');
  await new Promise((r) => setTimeout(r, 800));
  const toastText = await evaluate(
    '[...document.querySelectorAll(".toast,[class*=toast]")].map(t=>t.textContent).join(" / ")'
  );
  ok(
    'start/stop reports that systemd owns the lifecycle',
    toastText.includes('systemd'),
    toastText
  );

  // ── Logout returns to the form and kills the session ───────────────────
  await evaluate('fetch("/admin/api/logout",{method:"POST",credentials:"same-origin"})');
  const afterLogout = await evaluate(
    'fetch("/admin/api/rpc/get_status",{method:"POST",credentials:"same-origin",headers:{"content-type":"application/json"},body:"{}"}).then(r=>r.status)'
  );
  check('the session is invalid after logout', afterLogout, 401);

  // ── No uncaught errors in that whole run ───────────────────────────────
  // The start/stop click logs through `console.error` by design (the shared view
  // reports every command failure that way), so its expected refusal is filtered
  // out — everything else is a genuine error.
  const unexpected = runtimeErrors.filter(
    (e) => !e.includes('Toggle service failed')
  );
  ok('no uncaught console errors', unexpected.length === 0, unexpected.join('\n      '));
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
