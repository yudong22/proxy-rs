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
    data_dir: '/Users/test/.proxy-rs',
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
    // Today's request count and cache-hit rate for this session (see
    // stats::SessionMetrics). `cache_hit_pct` is null when unmeasured.
    requests: 7, cache_hit_pct: 62,
    recent: [{
      // A cumulative tool wait in the hours range, so this fixture exercises
      // `formatLongDuration`'s minutes path rather than a sub-minute value that
      // both formatters would render identically.
      session_id: 'dsh:0.1.6', model_ms: 1500, tool_wait_ms: 14_190_100, tool_waits: 1281,
      avg_ttft_ms: 300, output_tokens: 500, speed_tps: 42.5,
      requests: 7, cache_hit_pct: 62,
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
  get_dsh_config: {
    supported: true, profile: 'web',
    settings_path: '/tmp/.dsh/profiles/web/cordis.patch.yml', settings_exists: true,
    provider_exists: true, model_count: 4, base_url: 'http://127.0.0.1:3457/v1',
    credential_present: true, session_header: true,
  },
  get_request_logs: { items: [], total: 0, models: [], clients: [], sessions: [] },
  get_logs: { entries: [] },
  wb_credentials_list: {
    credentials: [
      { id: 'cred-1', label: '测试账号一', nickname: '甲', state: 'ok', enabled: true, points: 1234.5, sticky_sessions: 2, checked_in_today: true, masked_token: 'sk-***1', has_refresh_token: true, needs_relogin: false },
      { id: 'cred-2', label: '测试账号二', nickname: '乙', state: 'cooldown', enabled: true, points: null, sticky_sessions: 0, checked_in_today: false, masked_token: 'sk-***2', has_refresh_token: true, needs_relogin: false },
      { id: 'cred-3', label: '需重登账号', nickname: '丙', state: 'expired', enabled: true, points: null, sticky_sessions: 0, checked_in_today: false, masked_token: 'sk-***3', has_refresh_token: false, needs_relogin: true },
    ],
    default_identity_id: 'cred-1',
  },
  wb_refresh_logins: {
    total: 3, refreshed: 2, failed: 1, needs_relogin: 1,
    details: [
      { id: 'cred-1', label: '测试账号一', refreshed: true, error: '', needs_relogin: false },
      { id: 'cred-2', label: '测试账号二', refreshed: true, error: '', needs_relogin: false },
      { id: 'cred-3', label: '需重登账号', refreshed: false, error: '凭据没有 refreshToken，无法刷新', needs_relogin: true },
    ],
  },
  wb_relogin: { id: 'cred-3', label: '需重登账号', enabled: true, masked_token: 'sk-***3' },
  wb_refresh_points_single: {
    result: { id: 'cred-1', label: '测试账号一', points: 1234.5 },
    current_identity: { id: 'cred-1', label: '测试账号一', points: 1234.5 },
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
  pool_export: {
    ok: true, cancelled: false, path: '/tmp/proxy-rs-pool-20260101.json', encrypted: true,
    counts: { credentials: 2, api_keys: 1 }, empty: false, warnings: [],
  },
  pool_import_preview: {
    mode: 'merge', encrypted: false,
    credentials_total: 2, credentials_added: 1, credentials_updated: 1, credentials_unchanged: 0,
    credentials_removed: 0,
    keys_total: 1, keys_added: 1, keys_updated: 0, keys_unchanged: 0, keys_removed: 0,
    default_identity_applied: 'cred-1', default_identity_dropped: false,
    checkin_enabled: true, checkin_time: '08:30',
    warnings: ['账号 wb-000000 的 id 与 token 不一致，已按 token 重新计算为 cred-1'],
  },
  pool_import: {
    mode: 'merge', encrypted: false,
    credentials_total: 2, credentials_added: 1, credentials_updated: 1, credentials_unchanged: 0,
    credentials_removed: 0,
    keys_total: 1, keys_added: 1, keys_updated: 0, keys_unchanged: 0, keys_removed: 0,
    default_identity_applied: 'cred-1', default_identity_dropped: false,
    checkin_enabled: true, checkin_time: '08:30', warnings: [],
  },
};

/** What the UI must produce. Values captured from the pre-split build. */
const EXPECTED = {
  counts: { credentialRows: 4, providerOptions: 3, speedRows: 8 },
  // 7 metric rows + the 会话 header row.
  speedMetrics: {
    '请求次数': '7',
    '缓存命中': '62%',
    '模型用时': '1.5秒',
    '工具调用用时': '236分 30秒（1281 次）',
    '首 token 平均（TTFT）': '300毫秒',
    '输出 tokens': '500',
    '输出速度（TPS）': '42.5 tok/s',
  },
  speedHeaderLabels: ['dsh:0.1.6'],
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
  rowLabels: ['测试账号一', '测试账号二', '需重登账号', '备用密钥'],
  rowStates: ['正常', '冷却中', '已过期', '已启用'],
  defaultRowFlags: [true, false, false, false],
  selectValue: 'cred-1',
  // The DSH group renders its live state on load, from get_dsh_config.
  dshButtonPresent: true,
  dshStatus: '当前 profile web · 4 个模型 · http://127.0.0.1:3457/v1 · 凭据已就绪 · 会话识别已开启',
  // 缓存命中率 shows a value only — no expand affordance, no detail section.
  cacheCard: { present: true, clickable: false, expandable: false, hasCaret: false, detailExists: false },
  // Account-pool import/export (2.0.0). The dialogs are modal-only, so these
  // are read after driving them from the toolbar.
  exportResult: '已导出 2 个账号、1 个密钥（口令加密） /tmp/proxy-rs-pool-20260101.json',
  // The export dialog names the directory it reads and the counts it will write,
  // so "empty export" is diagnosable from the dialog itself (mock get_status
  // plus the two pool list commands: 2 credentials + 1 key).
  exportSourceShown: '/Users/test/.proxy-rs',
  exportCountsShown: '账号 3 个、密钥 1 个',
  // Batch refresh must name the accounts needing a re-bind, not just count them:
  // a fresh login is a manual step, so the user has to know which row to act on.
  refreshStatusContains: ['成功 2 个', '失败 1 个', '个需要重新绑定', '需重登账号'],
  // Dismissing the native save panel is a no-op, not a failure: the message says
  // so, carries no error tone, and the 导出 button is usable again.
  exportCancelled: {
    text: '已取消导出（未写入任何文件）',
    isError: false,
    retryable: true,
  },
  // An export that carries no identity is a failure the user must notice, even
  // though the file itself was written successfully. Asserted as substrings: the
  // message spans inline markup, so exact equality would only be testing
  // whitespace.
  exportEmpty: {
    isError: true,
    contains: [
      '不包含任何账号或密钥',
      '本机身份池为空',
      '/tmp/proxy-rs-pool-empty.json',
      '请在正式实例中导出',
    ],
  },
  importPreview: {
    credentialsAdded: '1',
    credentialsUpdated: '1',
    warningText: '账号 wb-000000 的 id 与 token 不一致，已按 token 重新计算为 cred-1',
    identity: 'cred-1',
  },
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
    window.__INVOKE_ARGS__ = {};
    window.__MOCKDATA__ = ${JSON.stringify(MOCK)};
    window.__TAURI__ = {
      core: { invoke: (cmd, args) => { window.__CALLS__.push(cmd);
        window.__INVOKE_ARGS__[cmd] = args;
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
        // The overview's second card row. 缓存命中率 is deliberately inert: it
        // reports a value and has no drill-down, so it must carry neither the
        // affordance classes nor a hidden detail section.
        cacheCard: (() => {
          const all = [...document.querySelectorAll('.metrics-grid .metric-card')];
          const card = all.find((c) => c.textContent.includes('缓存命中率'));
          return {
            present: Boolean(card),
            clickable: card?.classList.contains('clickable') || false,
            expandable: card?.classList.contains('expandable') || false,
            hasCaret: Boolean(card?.querySelector('.stat-caret')),
            detailExists: Boolean(document.querySelector('#stat-detail')),
          };
        })(),
        // The 输出速度 drill-down: metric row label -> that row's value cell for
        // the current session. Asserted by name rather than by row count so
        // adding a metric does not look like a regression, and a missing one
        // cannot hide behind a matching total.
        speedMetrics: (() => {
          const out = {};
          for (const tr of document.querySelectorAll('#speed-sessions-body tr')) {
            const th = tr.querySelector('th.col-metric');
            const td = tr.querySelector('td');
            if (th && td) out[th.textContent.trim()] = td.textContent.trim();
          }
          return out;
        })(),
        speedHeaderLabels: [...document.querySelectorAll('#speed-sessions-body th.col-session-cell')]
          .map((th) => th.textContent.trim()),
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
        dshButtonPresent: Boolean(document.querySelector('#btn-apply-dsh-config')),
        dshStatus: t('#dsh-config-status'),
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
  check('dsh button present', b.dshButtonPresent, EXPECTED.dshButtonPresent);
  check('dsh status', b.dshStatus, EXPECTED.dshStatus);
  check('speed drill-down metrics', b.speedMetrics, EXPECTED.speedMetrics);
  check('speed drill-down session columns', b.speedHeaderLabels, EXPECTED.speedHeaderLabels);
  check('缓存命中率 card is not expandable', b.cacheCard, EXPECTED.cacheCard);

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

  // Cross-module integration 3: the account-pool transfer dialogs (2.0.0).
  // Both are modal-only, so the toolbar buttons are the entry point, and the
  // import path must go preview -> (nothing written) -> apply -> pool repaint.
  //
  // The export leg starts by cancelling: the save panel is now a real OS sheet,
  // so "dismissed it" is an ordinary outcome the UI must report as such instead
  // of as a failure.
  await send('Runtime.evaluate', { expression: `(() => {
    window.__CALLS__ = [];
    window.__MOCKDATA__.pool_export = { ok: false, cancelled: true };
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
    document.querySelector('#btn-wb-export').click();
    document.querySelector('#btn-wb-export-run').click();
  })()` });
  await new Promise((r) => setTimeout(r, 300));

  const cancelStep = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => ({
      text: (document.querySelector('#wb-export-result')?.textContent || '')
        .replace(/\\s+/g, ' ').trim(),
      isError: document.querySelector('#wb-export-result')?.classList.contains('error'),
      retryable: !document.querySelector('#btn-wb-export-run')?.disabled,
    }))()`,
  });
  check('cancelled export is not an error', cancelStep.result.value, EXPECTED.exportCancelled);

  await send('Runtime.evaluate', { expression: `(() => {
    window.__MOCKDATA__.pool_export = {
      ok: true, cancelled: false, path: '/tmp/proxy-rs-pool-20260101.json', encrypted: true,
      counts: { credentials: 2, api_keys: 1 }, empty: false, warnings: [],
    };
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
    document.querySelector('#btn-wb-export').click();
    document.querySelector('#wb-export-encrypt').checked = true;
    document.querySelector('#wb-export-encrypt')
      .dispatchEvent(new Event('change', { bubbles: true }));
    document.querySelector('#wb-export-passphrase').value = 'hunter2hunter2';
    document.querySelector('#wb-export-passphrase2').value = 'hunter2hunter2';
    document.querySelector('#btn-wb-export-run').click();
  })()` });
  await new Promise((r) => setTimeout(r, 500));

  // Read the export result before the next dialog replaces the modal body.
  const exportStep = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => ({
      text: (document.querySelector('#wb-export-result')?.textContent || '')
        .replace(/\\s+/g, ' ').trim(),
      withPassphrase: Boolean(window.__INVOKE_ARGS__?.pool_export?.body?.passphrase),
      sourceShown: (document.querySelector('#wb-export-source')?.textContent || '').trim(),
      countsShown: (document.querySelector('#wb-export-counts')?.textContent || '').trim(),
    }))()`,
  });

  // An empty export must not look like a success: the backend reports
  // `empty: true` and the UI has to say so in the error tone, naming the reason.
  await send('Runtime.evaluate', { expression: `(() => {
    window.__MOCKDATA__.pool_export = {
      ok: true, cancelled: false, path: '/tmp/proxy-rs-pool-empty.json', encrypted: false,
      counts: { credentials: 0, api_keys: 0 }, empty: true,
      warnings: ['本机身份池为空，导出的文件不包含任何账号或密钥'],
    };
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
    document.querySelector('#btn-wb-export').click();
    document.querySelector('#btn-wb-export-run').click();
  })()` });
  await new Promise((r) => setTimeout(r, 400));

  const emptyStep = await send('Runtime.evaluate', {
    returnByValue: true,
    expression: `(() => {
      const el = document.querySelector('#wb-export-result');
      return {
        // The message spans inline <b> tags, so collapse whitespace around the
        // element boundaries before comparing.
        text: (el?.textContent || '').replace(/\\s+/g, ' ').trim(),
        isError: el?.classList.contains('error'),
      };
    })()`,
  });
  const empty = emptyStep.result.value;
  check('empty export is an error tone', empty.isError, EXPECTED.exportEmpty.isError);
  for (const needle of EXPECTED.exportEmpty.contains) {
    if (!empty.text.includes(needle)) {
      failures.push(`empty export message is missing ${JSON.stringify(needle)}\n      actual: ${empty.text}`);
    }
  }

  // Restore the healthy mock before the import leg runs.
  await send('Runtime.evaluate', { expression: `(() => {
    window.__MOCKDATA__.pool_export = {
      ok: true, cancelled: false, path: '/tmp/proxy-rs-pool-20260101.json', encrypted: true,
      counts: { credentials: 2, api_keys: 1 }, empty: false, warnings: [],
    };
  })()` });

  await send('Runtime.evaluate', { expression: `(() => {
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
    document.querySelector('#btn-wb-import').click();
    document.querySelector('#wb-import-text').value = '{"format":"proxy-rs-pool"}';
    document.querySelector('#btn-wb-import-preview').click();
  })()` });
  await new Promise((r) => setTimeout(r, 400));

  // Only a successful preview may enable 确认导入; the apply call is what must
  // repaint the pool, so the assertion covers both halves.
  const transfer = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      const t = (s) => (document.querySelector(s)?.textContent || '').replace(/\\s+/g, ' ').trim();
      const previewEnabled = !document.querySelector('#btn-wb-import-run')?.disabled;
      const preview = {
        credentialsAdded: t('.wb-transfer-cards .wb-stat-card:nth-child(2) .wb-stat-val'),
        credentialsUpdated: t('.wb-transfer-cards .wb-stat-card:nth-child(3) .wb-stat-val'),
        warningText: t('.wb-transfer-warnings li'),
        identity: t('.wb-transfer-identity .mono'),
      };
      const before = window.__CALLS__.length;
      document.querySelector('#btn-wb-import-run').click();
      await new Promise((r) => setTimeout(r, 600));
      return {
        previewEnabled,
        preview,
        importCalled: window.__CALLS__.includes('pool_import'),
        poolRepainted: window.__CALLS__.slice(before).includes('wb_credentials_list'),
        modalClosed: !document.querySelector('#request-modal-overlay')?.classList.contains('active'),
      };
    })()`,
  });
  const ex = exportStep.result.value;
  const tr = transfer.result.value;
  check('export result text', ex.text, EXPECTED.exportResult);
  check('export sends the passphrase', ex.withPassphrase, true);
  // The dialog names the data directory it will read — the fact that explains an
  // unexpectedly empty export.
  check('export dialog shows its data directory', ex.sourceShown, EXPECTED.exportSourceShown);
  check('export dialog shows what will be exported', ex.countsShown, EXPECTED.exportCountsShown);
  check('preview enables 确认导入', tr.previewEnabled, true);
  check('import preview counts', tr.preview, EXPECTED.importPreview);
  check('import called', tr.importCalled, true);
  check('import repaints the pool', tr.poolRepainted, true);
  check('import closes the dialog', tr.modalClosed, true);

  // Cross-module integration 4: login-state recovery (relogin / refresh logins).
  // The account with no refresh token must be visibly marked, batch refresh must
  // name the account that needs a re-bind rather than only counting it, and the
  // re-bind dialog must submit to `wb_relogin` and repaint the pool.
  await send('Runtime.evaluate', { expression: `(() => {
    window.__CALLS__ = [];
    const ov = document.querySelector('#request-modal-overlay');
    ov?.classList.remove('active');
    document.querySelector('#btn-wb-refresh-logins').click();
  })()` });
  await new Promise((r) => setTimeout(r, 400));

  const recovery = await send('Runtime.evaluate', {
    returnByValue: true,
    awaitPromise: true,
    expression: `(async () => {
      const t = (s) => (document.querySelector(s)?.textContent || '').replace(/\\s+/g, ' ').trim();
      const refreshStatus = t('#wb-status');

      // The per-row 刷新积分 button must refresh ONLY its own row. It used to
      // ignore its data-points id and call the pool-wide command, so this
      // asserts the id actually reaches the backend.
      const pointsRow = [...document.querySelectorAll('#wb-identity-list .wb-credential-row')]
        .find((r) => r.textContent.includes('测试账号二'));
      pointsRow.querySelector('[data-points]').click();
      await new Promise((r) => setTimeout(r, 300));
      const rowPoints = {
        called: window.__CALLS__.includes('wb_refresh_points_single'),
        id: window.__INVOKE_ARGS__?.wb_refresh_points_single?.id,
        calledPoolWide: window.__CALLS__.includes('wb_refresh_points'),
        status: t('#wb-status'),
      };

      const row = [...document.querySelectorAll('#wb-identity-list .wb-credential-row')]
        .find((r) => r.textContent.includes('需重登账号'));
      const badge = row?.querySelector('.wb-relogin-pill')?.textContent?.trim() || '';

      const before = window.__CALLS__.length;
      // The button on *that* row, not the first one in the list.
      row.querySelector('[data-relogin]').click();
      const dialogOpen = document.querySelector('#request-modal-overlay')?.classList.contains('active');
      const targetShown = t('#modal-req-body .mono');
      document.querySelector('#wb-relogin-text').value = '{"accessToken":"fresh-token"}';
      document.querySelector('#btn-wb-relogin-run').click();
      await new Promise((r) => setTimeout(r, 400));
      return {
        refreshStatus,
        reloginBadge: badge,
        dialogOpen,
        targetShown,
        rowPoints,
        refreshCalled: window.__CALLS__.includes('wb_refresh_logins'),
        reloginCalled: window.__CALLS__.includes('wb_relogin'),
        reloginBody: window.__INVOKE_ARGS__?.wb_relogin?.body?.id,
        poolRepainted: window.__CALLS__.slice(before).includes('wb_credentials_list'),
        dialogClosed: !document.querySelector('#request-modal-overlay')?.classList.contains('active'),
      };
    })()`,
  });
  const rc = recovery.result.value;
  check('batch refresh called', rc.refreshCalled, true);
  check('needs-relogin account is marked', rc.reloginBadge, '需重新绑定');
  check('relogin dialog opens', rc.dialogOpen, true);
  check('relogin dialog names its target', rc.targetShown, 'cred-3');
  check('relogin called', rc.reloginCalled, true);
  check('relogin targets the row it was opened from', rc.reloginBody, 'cred-3');
  check('relogin repaints the pool', rc.poolRepainted, true);
  check('relogin closes the dialog', rc.dialogClosed, true);
  // The per-row points button must target its own row and must NOT fall back to
  // the pool-wide command.
  check('row 刷新积分 calls the single-identity command', rc.rowPoints.called, true);
  check('row 刷新积分 passes its own id', rc.rowPoints.id, 'cred-2');
  check('row 刷新积分 does not refresh the whole pool', rc.rowPoints.calledPoolWide, false);
  for (const needle of EXPECTED.refreshStatusContains) {
    if (!rc.refreshStatus.includes(needle)) {
      failures.push(`batch refresh status is missing ${JSON.stringify(needle)}\n      actual: ${rc.refreshStatus}`);
    }
  }

  if (runtimeErrors.length) {
    failures.push(`runtime console errors:\n      ${runtimeErrors.join('\n      ')}`);
  }

  console.log(`boot assertions: counts, text, credential rows, default identity`);
  console.log(`integration: palette + OAuth repaint + pool import/export + login recovery`);
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
