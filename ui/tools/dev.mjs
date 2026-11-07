#!/usr/bin/env node
/**
 * Local UI dev server: open `ui/` in a normal browser and edit without a Rust
 * build or a Tauri window.
 *
 * ## Why this exists
 *
 * `task dev` compiles the Tauri app and opens its webview, which is the honest
 * way to test IPC — but it costs a Rust build on every iteration and couples
 * the front-end loop to the backend loop. Most UI work (layout, the 会话 view,
 * CSS, text) only needs the page and *some* data.
 *
 * `smoke.mjs` already proved the page boots in a browser against a mock bridge.
 * This harness is that, live: it serves `ui/` statically with file watching of
 * nothing (reload the tab), installs `window.__TAURI__` shims, and answers the
 * commands from **your real `~/.claude/projects`** through the same TypeScript
 * parser rules as the Rust module — so the 会话 page shows real transcripts.
 *
 * ## What it is not
 *
 * It is not the gateway. No request logging, no stats, no service control. Commands
 * outside the session family return an empty payload or a clear "not mocked"
 * error on purpose: a fake success is how a UI bug hides.
 *
 * ## Usage
 *
 *   node ui/tools/dev.mjs                 # http://127.0.0.1:4300
 *   PORT=4400 node ui/tools/dev.mjs       # another port
 *   CLAUDE_PROJECTS_DIR=/path/to/projects node ui/tools/dev.mjs
 *
 * Then open the printed URL, go to 会话, and edit files under `ui/`; reload the
 * tab to see changes. Run `task ui-dev` for the same thing.
 */

import fs from 'node:fs';
import http from 'node:http';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const UI = path.join(REPO, 'ui');
const PORT = Number(process.env.PORT) || 4300;

// ── Claude Code transcripts, parsed here ────────────────────────────────
// Deliberately a *simplification* of src/claude_sessions.rs, not a second
// implementation to keep in sync: it reads whole files (fine for a few dozen),
// does the same turn-key dedupe for tokens, and emits the same JSON shape the
// page consumes. If a field is wrong here the page will look wrong — that is
// the signal to check the Rust side, not to patch the mock to match the page.

const projectsRoot = () =>
  process.env.CLAUDE_PROJECTS_DIR || path.join(process.env.HOME || '', '.claude', 'projects');

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/;
const NOISE = ['<task-notification>', '<system-reminder>', '<local-command-stdout>', '<local-command-caveat>', '<command-name>', 'Caveat: The messages below', '<user-prompt-submit-hook>'];
const TEXT_CAP = 4000;
const TOOL_INPUT_CAP = 2000;

const cap = (s, n) => {
  const chars = [...String(s || '')];
  return chars.length > n ? [chars.slice(0, n).join(''), true] : [chars.join(''), false];
};
const iso = (t) => (t ? Date.parse(t) || null : null);
const str = (v, k) => (v && typeof v[k] === 'string' ? v[k] : null);
const num = (v, k) => (v && typeof v[k] === 'number' ? v[k] : 0);

function emptyTotals() { return { input: 0, output: 0, cache_read: 0, cache_write: 0, thinking: 0 }; }
function addTotals(a, b) { for (const k of Object.keys(a)) a[k] += b[k]; return a; }

function usageOf(msg) {
  let u = msg && msg.usage;
  if (typeof u === 'string') { try { u = JSON.parse(u); } catch { u = null; } }
  if (!u || typeof u !== 'object') return null;
  return {
    input: num(u, 'input_tokens'),
    output: num(u, 'output_tokens'),
    cache_read: num(u, 'cache_read_input_tokens'),
    cache_write: num(u, 'cache_creation_input_tokens'),
    thinking: num(u.output_tokens_details, 'thinking_tokens'),
  };
}

function parseTranscript(file, { keepEntries = false, offset = 0, limit = 200 } = {}) {
  const stat = fs.statSync(file);
  const raw = fs.readFileSync(file, 'utf8');
  const seen = new Set();
  const turns = new Set();
  const models = new Map();
  const s = {
    id: path.basename(file, '.jsonl'),
    encoded_project: path.basename(path.dirname(file)),
    cwd: '', title: '', first_seen_ms: null, last_seen_ms: null,
    file_mtime_ms: Math.round(stat.mtimeMs), size_bytes: stat.size,
    git_branch: null, version: null, turns: 0, assistant_turns: 0, tool_calls: 0,
    subagents: 0, last_model: null, counts_partial: false,
  };
  let firstPrompt = null;
  let totals = emptyTotals();
  let inclTokens = emptyTotals();
  const entries = [];
  const ignored = { user_noise: 0, duplicate_uuid: 0, other: 0, malformed: 0 };
  const note = (ts) => { if (ts == null) return; s.first_seen_ms = s.first_seen_ms == null ? ts : Math.min(s.first_seen_ms, ts); s.last_seen_ms = s.last_seen_ms == null ? ts : Math.max(s.last_seen_ms, ts); };
  const ctx = (o) => { if (!s.cwd && str(o, 'cwd')) s.cwd = o.cwd; if (!s.git_branch && str(o, 'gitBranch')) s.git_branch = o.gitBranch; if (!s.version && str(o, 'version')) s.version = o.version; };
  const push = (e) => { if (keepEntries) entries.push(e); };
  const seenUuid = (o) => { const u = str(o, 'uuid'); if (!u) return false; if (seen.has(u)) { ignored.duplicate_uuid++; return true; } seen.add(u); return false; };

  for (const line of raw.split('\n')) {
    if (!line.trim()) continue;
    let o; try { o = JSON.parse(line); } catch { ignored.malformed++; continue; }
    const type = str(o, 'type');
    if (type === 'ai-title') { if (str(o, 'aiTitle')?.trim()) s.title = o.aiTitle.trim(); else ignored.other++; continue; }
    if (type === 'user' || type === 'assistant') {
      ctx(o); const ts = iso(o.timestamp); note(ts);
      if (seenUuid(o)) continue;
      const msg = o.message && typeof o.message === 'object' ? o.message : (typeof o.message === 'string' ? { role: 'user', content: o.message } : {});
      const sidechain = o.isSidechain === true;
      const uuid = str(o, 'uuid') || '';
      const parent = str(o, 'parentUuid');
      const content = msg.content;
      if (type === 'user') {
        const isMeta = o.isMeta === true;
        if (typeof content === 'string') {
          const t = content.trimStart();
          if (NOISE.some((p) => t.startsWith(p))) ignored.user_noise++;
          else {
            if (!isMeta) { s.turns++; if (!firstPrompt) firstPrompt = content; }
            const [text, truncated] = cap(content, TEXT_CAP);
            push({ uuid, parent_uuid: parent, ts_ms: ts, role: 'user', kind: isMeta ? 'meta' : 'text', sidechain, text, text_truncated: truncated, tool_name: null, tool_input: null, tool_use_id: null, tool_result_is_error: null, model: null, tokens: null });
          }
        } else if (Array.isArray(content)) {
          let hasText = false;
          content.forEach((b, i) => {
            if (!b || typeof b !== 'object') return;
            if (b.type === 'text') { hasText = true; const [text, truncated] = cap(b.text, TEXT_CAP); push({ uuid: `${uuid}-${i}`, parent_uuid: parent, ts_ms: ts, role: 'user', kind: 'text', sidechain, text, text_truncated: truncated, tool_name: null, tool_input: null, tool_use_id: null, tool_result_is_error: null, model: null, tokens: null }); }
            if (b.type === 'tool_result') {
              const c = Array.isArray(b.content) ? b.content.map((p) => p.text || '').join('\n') : String(b.content || '');
              const [text, truncated] = cap(c, TEXT_CAP);
              push({ uuid: `${uuid}-${i}`, parent_uuid: parent, ts_ms: ts, role: 'user', kind: 'tool_result', sidechain, text, text_truncated: truncated, tool_name: null, tool_input: null, tool_use_id: str(b, 'tool_use_id'), tool_result_is_error: b.is_error === true, model: null, tokens: null });
            }
          });
          if (hasText && !isMeta) { s.turns++; if (!firstPrompt) firstPrompt = content.find((b) => b.type === 'text')?.text || null; }
        }
        continue;
      }
      // assistant
      const model = str(msg, 'model');
      if (model) s.last_model = model;
      const key = `${str(msg, 'id') || uuid}|${str(o, 'requestId') || ''}`;
      const fresh = !turns.has(key); if (fresh) turns.add(key);
      const usage = usageOf(msg);
      let firstTokens = null;
      if (fresh) {
        s.assistant_turns++;
        if (usage) {
          addTotals(sidechain ? inclTokens : totals, usage);
          const m = model || 'unknown';
          const row = models.get(m) || { model: m, turns: 0, tokens: emptyTotals() };
          row.turns++; addTotals(row.tokens, usage); models.set(m, row);
        }
        firstTokens = usage;
      }
      const blocks = Array.isArray(content) ? content : (typeof content === 'string' ? [{ type: 'text', text: content }] : []);
      let attached = false;
      blocks.forEach((b, i) => {
        if (!b || typeof b !== 'object') return;
        const base = { uuid: `${uuid}-${i}`, parent_uuid: parent, ts_ms: ts, role: 'assistant', sidechain, model, text: '', text_truncated: false, tool_name: null, tool_input: null, tool_use_id: null, tool_result_is_error: null, tokens: (!attached && fresh) ? firstTokens : null };
        if (b.type === 'text') { [base.text, base.text_truncated] = cap(b.text, TEXT_CAP); push({ ...base, kind: 'text' }); attached = true; }
        else if (b.type === 'thinking' || b.type === 'reasoning') { [base.text, base.text_truncated] = cap(b.thinking || b.text, TEXT_CAP); push({ ...base, kind: 'thinking' }); attached = true; }
        else if (b.type === 'tool_use') {
          s.tool_calls++;
          const rendered = b.input ? JSON.stringify(b.input) : '';
          [base.tool_input, base.text_truncated] = cap(rendered, TOOL_INPUT_CAP);
          push({ ...base, kind: 'tool_use', tool_name: b.name || null, tool_use_id: b.id || null });
          attached = true;
        }
      });
      continue;
    }
    ctx(o); ignored.other++;
  }
  if (!s.title.trim()) s.title = (firstPrompt || '').trim().slice(0, 80).replace(/\s+/g, ' ') || '(未命名会话)';
  const subDir = path.join(path.dirname(file), s.id, 'subagents');
  try { s.subagents = fs.readdirSync(subDir).filter((n) => /^agent-[A-Za-z0-9_-]+\.jsonl$/.test(n)).length; } catch { s.subagents = 0; }
  const modelList = [...models.values()].map((m) => ({ model: m.model, turns: m.turns, tokens: m.tokens })).sort((a, b) => b.tokens.output - a.tokens.output);
  const sorted = keepEntries ? entries : [];
  const end = Math.max(0, sorted.length - offset);
  const start = Math.max(0, end - limit);
  return { summary: s, tokens: totals, tokens_incl_subagents: inclTokens, models: modelList, ignored, entries, page: sorted.slice(start, end), has_more: start > 0 };
}

function listSessions(args = {}) {
  const limit = Math.min(args.limit || 50, 200);
  const offset = args.offset || 0;
  const root = projectsRoot();
  if (!fs.existsSync(root)) return { available: false, reason: `未找到 ${root}`, root: null, sources: [], stats_available: false, total: 0, offset: 0, limit, sessions: [] };
  const files = [];
  for (const proj of fs.readdirSync(root)) {
    const dir = path.join(root, proj);
    try { if (!fs.statSync(dir).isDirectory() || proj.startsWith('.')) continue; } catch { continue; }
    for (const f of fs.readdirSync(dir).filter((n) => n.endsWith('.jsonl') && UUID.test(n.slice(0, -6)))) {
      const p = path.join(dir, f);
      try { files.push({ p, mtime: fs.statSync(p).mtimeMs }); } catch {}
    }
  }
  files.sort((a, b) => b.mtime - a.mtime);
  const page = files.slice(offset, offset + limit).flatMap(({ p }) => {
    try { const { summary } = parseTranscript(p, { keepEntries: false }); return [{ ...summary, gateway: null, gateway_error: null }]; } catch (e) { return []; }
  });
  return {
    available: true, reason: null, root,
    sources: [
      { kind: 'claude_local', label: 'Claude Code', available: true, note: null },
      { kind: 'dsh_local', label: 'DSH', available: false, note: 'DSH 会话是 zstd 压缩的本地存档，暂不支持浏览（dev 预览同样未实现）' },
    ],
    stats_available: false, gateway_error: null, total: files.length, offset, limit, sessions: page,
  };
}

function findSession(id) {
  if (!UUID.test(id)) return null;
  const root = projectsRoot();
  if (!fs.existsSync(root)) return null;
  for (const proj of fs.readdirSync(root)) {
    const p = path.join(root, proj, `${id}.jsonl`);
    if (fs.existsSync(p)) return p;
  }
  return null;
}

function sessionDetail(id, args = {}) {
  const file = findSession(id);
  if (!file) throw new Error('会话不存在: ' + id);
  const r = parseTranscript(file, { keepEntries: true, offset: args.offset || 0, limit: args.limit || 200 });
  return {
    summary: r.summary, cost_state: null, models: r.models, tokens: r.tokens, tokens_incl_subagents: r.tokens_incl_subagents,
    timeline: r.page, timeline_offset: args.offset || 0, timeline_total: r.entries.length, has_more: r.has_more,
    ignored: r.ignored, subagents: [], agent_id: null,
    gateway: null, gateway_error: 'dev 预览不连接网关统计库',
  };
}

// ── Tauri bridge shim, injected like the server's ui/server/bridge.js ─────
const BRIDGE = `
window.__DEV__ = true;
window.__TAURI__ = {
  core: {
    invoke: async (cmd, args) => {
      const r = await fetch('/__dev__/rpc/' + encodeURIComponent(cmd), {
        method: 'POST', headers: { 'content-type': 'application/json' }, body: JSON.stringify(args || {}),
      });
      const body = await r.json();
      if (!r.ok) throw body.error || ('HTTP ' + r.status);
      return body.result;
    },
  },
  event: { listen: async () => () => {} },
};
`;

function rpc(name, args) {
  switch (name) {
    case 'get_status':
      return { running: false, service_running: false, managed_by: 'dev', can_control_service: false, port: null, version: 'dev', provider: 'dev', upstream_url: '', log_path: '', data_dir: '', launch_at_login: false, is_dev: true, current_identity: null };
    case 'get_stats':
      return { requests_total: 0, requests_success: 0, requests_failed: 0, tokens_total: 0, tokens_input: 0, tokens_cache_read: 0, tokens_cache_write: 0, tokens_output: 0, cache_hit_pct: null, overrides_total: 0 };
    case 'get_session_metrics':
      return { speed_tps: null, recent: [] };
    case 'get_request_logs':
      return { items: [], total: 0, models: [], client_models: [], clients: [], sessions: [] };
    case 'get_logs':
      return { entries: [] };
    case 'get_settings':
      return { provider_id: 'dev', custom_url: '', port: null, bind: '127.0.0.1', api_key: '', api_key_set: false, reasoning_model: '', completion_model: '', model_map: '', sanitize_terms: '', launch_at_login: false, force_stream: null, force_stream_preset: true };
    case 'get_providers':
      return { providers: [] };
    case 'get_claude_config':
    case 'get_codex_config':
    case 'get_dsh_config':
      return { supported: false, reason: 'dev 预览不写入客户端配置' };
    case 'wb_credentials_list':
      return { credentials: [], default_identity_id: null, identity_order: [] };
    case 'api_keys_list':
    case 'client_accounts_list':
      return { keys: [], accounts: [] };
    case 'wb_preferences':
      return {};
    case 'claude_sessions_list':
      return listSessions(args && args.args);
    case 'claude_sessions_detail':
      return sessionDetail(args.id, args.args);
    case 'claude_sessions_subagent':
      throw new Error('dev 预览暂不支持子代理对话');
    default:
      // Loud, not silent: a screen that wires a new command must fail visibly
      // here instead of showing an empty panel that looks like a data bug.
      throw new Error(`dev 预览未实现命令: ${name}`);
  }
}

// ── Static server ───────────────────────────────────────────────────────
const TYPES = { '.html': 'text/html; charset=utf-8', '.css': 'text/css; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.svg': 'image/svg+xml', '.json': 'application/json', '.png': 'image/png' };
const INJECT = '<script>window.__DEV__ = true;</script><script type="module" src="/src/main.js"></script>';

const server = http.createServer((req, res) => {
  const url = new URL(req.url, 'http://localhost');
  let p = decodeURIComponent(url.pathname);

  if (p.startsWith('/__dev__/rpc/')) {
    const name = p.slice('/__dev__/rpc/'.length);
    let body = '';
    req.on('data', (c) => { body += c; });
    req.on('end', () => {
      let args = {};
      try { args = body ? JSON.parse(body) : {}; } catch {}
      try { res.writeHead(200, { 'content-type': 'application/json', 'cache-control': 'no-store' }); res.end(JSON.stringify({ result: rpc(name, args) })); }
      catch (e) { res.writeHead(400, { 'content-type': 'application/json' }); res.end(JSON.stringify({ error: String(e.message || e) })); }
    });
    return;
  }

  if (p === '/') p = '/index.html';
  const file = path.join(UI, p);
  if (!file.startsWith(UI + path.sep) || !fs.existsSync(file) || fs.statSync(file).isDirectory()) {
    res.writeHead(404, { 'content-type': 'text/plain' }); res.end('not found'); return;
  }
  const ext = path.extname(file);
  let body = fs.readFileSync(file);
  if (ext === '.html') {
    // The page loads `src/main.js` as a module; the bridge must exist before
    // that, exactly as `console.rs` injects `ui/server/bridge.js` before it.
    body = Buffer.from(body.toString('utf8').replace(
      '<script type="module" src="src/main.js"></script>',
      `<script>${BRIDGE}</script>${INJECT}`,
    ));
  }
  res.writeHead(200, { 'content-type': TYPES[ext] || 'application/octet-stream', 'cache-control': 'no-store' });
  res.end(body);
});

server.listen(PORT, '127.0.0.1', () => {
  console.log(`dev UI: http://127.0.0.1:${PORT}/`);
  console.log(`  Claude sessions from: ${projectsRoot()}`);
  console.log('  edit ui/ and reload the tab; no Rust build needed.');
});
