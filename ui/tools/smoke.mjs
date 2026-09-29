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
 *     import of the settings view, and the account pool's repaint after OAuth.
 */

import fs from 'node:fs';
import http from 'node:http';
import os from 'node:os';
import path from 'node:path';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const UI = path.join(REPO, 'ui');
const PORT = 4197;
const DEBUG = 9357;
const CHROME = process.env.CHROME_PATH
  || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';

if (!fs.existsSync(CHROME)) {
  // An explicit, loud skip rather than a failure: this keeps `task check`
  // usable on a machine without Chrome. A smoke test that silently reported
  // success when it never ran would be worse than having none.
  console.log(`SKIPPED: no Chrome at ${CHROME}`);
  console.log('Set CHROME_PATH to a Chrome/Chromium binary to run this harness.');
  process.exit(0);
}

/** A mock Rust backend: the shapes the UI actually reads out of each command. */
const MOCK = {
  get_status: {
    running: true, port: 3456, provider: 'workbuddy-cn', version: '9.9.9',
    upstream_url: 'https://example.test/v1', log_path: '/tmp/proxy.log',
    launch_at_login: false, is_dev: false,
    current_identity: { id: 'cred-1', label: '测试账号一', points: 1234.5 },
  },
  get_stats: {
    requests_total: 1234, tokens_total: 2500000, cache_hit_pct: 42, requests_failed: 7,
    tokens_input: 100, tokens_cache_read: 200, tokens_cache_write: 300,
    tokens_output: 400, overrides_total: 5,
  },
  get_session_metrics: {
    speed_tps: 42.5, model: 'deepseek-chat', measured_turns: 3, output_tokens: 500,
    session_id: 'dsh:0.1.6',
    recent: [{
      session_id: 'dsh:0.1.6', model_ms: 1500, tool_wait_ms: 200, tool_waits: 2,
      avg_ttft_ms: 300, output_tokens: 500, speed_tps: 42.5,
    }],
  },
  get_settings: {
    provider_id: 'workbuddy-cn', custom_url: '', port: 3456, bind: '127.0.0.1',
    reasoning_model: '', completion_model: '', model_map: '', sanitize_terms: '',
    launch_at_login: false, force_stream: null, force_stream_preset: true,
  },
  get_providers: { providers: [{ id: 'workbuddy-cn', name: 'WorkBuddy CN' }, { id: 'openai', name: 'OpenAI' }] },
  get_claude_config: { env: { ANTHROPIC_MODEL: 'deepseek-chat' } },
  get_codex_config: { supported: true, catalog_exists: false, catalog_path: '/tmp/cat.json', config_path: '/tmp/cfg.toml' },
  get_request_logs: { items: [], total: 0, models: [], clients: [], sessions: [] },
  get_logs: { entries: [] },
  wb_credentials_list: {
    credentials: [
      { id: 'cred-1', label: '测试账号一', nickname: '甲', state: 'ok', enabled: true, points: 1234.5, sticky_sessions: 2, checked_in_today: true, masked_token: 'sk-***1' },
      { id: 'cred-2', label: '测试账号二', nickname: '乙', state: 'cooldown', enabled: true, points: null, sticky_sessions: 0, checked_in_today: false, masked_token: 'sk-***2' },
    ],
    default_identity_id: 'cred-1',
  },
  api_keys_list: {
    keys: [{ id: 'key-1', label: '备用密钥', enabled: true, points: 88, masked: 'sk-***k' }],
    default_identity_id: 'cred-1',
  },
  wb_preferences: { daily_checkin_enabled: true, daily_checkin_time: '08:30', last_checkin_run_date: '2026-01-01' },
  fetch_models: { models: [{ id: 'deepseek-chat', name: 'DeepSeek Chat' }] },
  test_upstream: { ok: true, detail: '上游流式响应正常' },
  wb_oauth_start: { state: 'abc', auth_url: 'https://example.test/auth', qr_svg: '<svg id="qrsvg"></svg>' },
  wb_oauth_poll: { status: 'success', credential: { label: '新扫码账号' } },
};

/** What the UI must produce. Values captured from the pre-split build. */
const EXPECTED = {
  counts: { credentialRows: 3, providerOptions: 3, speedRows: 6 },
  text: {
    metricProvider: 'workbuddy-cn',
    metricPoints: '1234.50',
    metricPointsAccount: '测试账号一',
    metricSpeed: '42.5 tok/s',
    portValue: '3456',
    checkinTime: '08:30',
    scheduleHint: '上次定时打卡：2026-01-01',
    forceStreamHint: '跟随服务商预设：开启',
    statusText: '运行中 · 端口 3456',
    appVersion: 'v9.9.9',
  },
  rowLabels: ['测试账号一', '测试账号二', '备用密钥'],
  rowStates: ['正常', '冷却中', '已启用'],
  defaultRowFlags: [true, false, false],
  selectValue: 'cred-1',
};

const server = http.createServer((req, res) => {
  let p = decodeURIComponent(req.url.split('?')[0]);
  if (p === '/') p = '/index.html';
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
    window.__MOCKDATA__ = ${JSON.stringify(MOCK)};
    window.__TAURI__ = {
      core: { invoke: (cmd) => { window.__CALLS__.push(cmd);
        return Promise.resolve(Object.prototype.hasOwnProperty.call(window.__MOCKDATA__, cmd)
          ? window.__MOCKDATA__[cmd] : null); } },
      event: { listen: () => Promise.resolve(() => {}) },
    };` });

  await send('Page.navigate', { url: `http://127.0.0.1:${PORT}/index.html` });
  await new Promise((r) => setTimeout(r, 2000));

  const boot = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const rows = [...document.querySelectorAll('#wb-identity-list .wb-credential-row')];
      const sel = document.querySelector('#wb-default-identity-select');
      const t = (s) => document.querySelector(s)?.textContent;
      const v = (s) => document.querySelector(s)?.value;
      return {
        counts: {
          credentialRows: rows.length,
          providerOptions: document.querySelector('#setting-provider')?.options.length,
          speedRows: document.querySelectorAll('#speed-sessions-body tr').length,
        },
        text: {
          metricProvider: t('#metric-provider'), metricPoints: t('#metric-points'),
          metricPointsAccount: t('#metric-points-account'), metricSpeed: t('#metric-speed'),
          portValue: v('#setting-port'), checkinTime: v('#wb-checkin-time'),
          scheduleHint: t('#wb-schedule-hint'), forceStreamHint: t('#force-stream-hint'),
          statusText: t('#status-text'), appVersion: t('#app-version'),
        },
        rowLabels: rows.map((r) => r.querySelector('.wb-credential-label')?.textContent),
        rowStates: rows.map((r) => r.querySelector('.wb-state-pill')?.textContent),
        defaultRowFlags: rows.map((r) => r.classList.contains('wb-credential-row--default')),
        selectValue: sel?.value,
      };
    })()`,
  });
  const b = boot.result.value;
  check('counts', b.counts, EXPECTED.counts);
  check('text', b.text, EXPECTED.text);
  check('rowLabels', b.rowLabels, EXPECTED.rowLabels);
  check('rowStates', b.rowStates, EXPECTED.rowStates);
  check('defaultRowFlags', b.defaultRowFlags, EXPECTED.defaultRowFlags);
  check('selectValue', b.selectValue, EXPECTED.selectValue);

  // Cross-module integration 1: palette's dynamic import of the settings view.
  await send('Runtime.evaluate', { expression: 'window.__CALLS__.length = 0;' });
  await send('Runtime.evaluate', {
    expression: `import('./src/views/settings/index.js').then(async (m) => {
      window.__T_TYPE__ = typeof m.testUpstream;
      await m.testUpstream(document.querySelector('#settings-test-result'));
      window.__T_TEXT__ = document.querySelector('#settings-test-result')?.textContent;
      window.__T_CLASS__ = document.querySelector('#settings-test-result')?.className;
    });`,
  });
  await new Promise((r) => setTimeout(r, 800));

  // Cross-module integration 2: OAuth success must repaint the pool via the
  // callback the pool injected into the OAuth module.
  await send('Runtime.evaluate', { expression: `document.querySelector('#btn-wb-qrcode').click();` });
  await new Promise((r) => setTimeout(r, 3000));

  const integ = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => ({
      testUpstreamType: window.__T_TYPE__,
      testUpstreamText: window.__T_TEXT__,
      testUpstreamClass: window.__T_CLASS__,
      qrModalActive: document.querySelector('#request-modal-overlay')?.classList.contains('active'),
      qrSvgInjected: Boolean(document.querySelector('#wb-qr-target svg')),
      pollStatus: document.querySelector('#wb-qr-poll-status')?.textContent?.trim(),
      called: [...new Set(window.__CALLS__)],
    }))()`,
  });
  const i = integ.result.value;
  check('palette dynamic import exposes testUpstream', i.testUpstreamType, 'function');
  check('testUpstream result', i.testUpstreamText, '✓ 连通性测试通过 (上游流式响应正常)');
  check('testUpstream class', i.testUpstreamClass, 'test-result ok');
  check('qr svg injected', i.qrSvgInjected, true);
  check('qr poll status', i.pollStatus, '✅ 扫码成功！账号已自动加入账号池');
  for (const cmd of ['wb_oauth_start', 'wb_oauth_poll', 'wb_credentials_list', 'api_keys_list']) {
    if (!i.called.includes(cmd)) failures.push(`OAuth flow did not call ${cmd} (pool repaint likely broken)`);
  }

  if (runtimeErrors.length) {
    failures.push(`runtime console errors:\n      ${runtimeErrors.join('\n      ')}`);
  }

  console.log(`boot assertions: counts, text, credential rows, default identity`);
  console.log(`integration: palette dynamic import + OAuth repaint`);
  console.log(`mock commands exercised: ${i.called.length}`);
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
