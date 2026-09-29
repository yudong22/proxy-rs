#!/usr/bin/env node
/**
 * UI consistency checks.
 *
 * The front-end is plain ES modules with no framework and no bundler. That keeps
 * it simple, but it also means several classes of mistake are invisible until a
 * user notices them. This script checks the ones that can be decided reliably,
 * so "the UI drifted" fails a check instead of a screenshot.
 *
 * What it checks, and why each is sound:
 *
 *   1. Stylesheet cascade — every .css on disk is linked from index.html, in the
 *      documented order, with tokens.css first. A file that is not linked is dead
 *      code; a wrong order silently changes what overrides what.
 *   2. Duplicate ids — two static `id="x"` in the same document. This is only
 *      checked for static markup, never for ids built from variables, so it
 *      cannot produce a false positive.
 *   3. Module graph — every relative import resolves, and every named import is
 *      actually exported by its target. Catches a rename that missed a caller.
 *   4. Runtime ids (optional, needs Chrome) — every `$('#id')` in the source
 *      resolves to an element in the rendered page. Static analysis cannot do
 *      this soundly here because ids are routinely built from constants and
 *      injected into templates; the DOM is the only ground truth.
 *
 * Run: node ui/tools/check-ui.mjs            (static checks only)
 *      node ui/tools/check-ui.mjs --runtime  (also load the page in headless Chrome)
 *
 * Exits 1 on any problem.
 */

import fs from 'node:fs';
import path from 'node:path';
import http from 'node:http';
import os from 'node:os';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const UI = path.join(REPO, 'ui');
const SRC = path.join(UI, 'src');
const STYLE = path.join(UI, 'style');
const INDEX = path.join(UI, 'index.html');

/** The cascade order index.html documents. Earlier files may be overridden by
 *  later ones; tokens.css must come first because every later file reads it. */
const CASCADE = [
  'tokens.css',
  'base.css',
  'controls.css',
  'badges.css',
  'overlays.css',
  'layout.css',
  'cards.css',
  'logs.css',
  'views.css',
  'workbuddy.css',
];

const problems = [];
const notes = [];
const rel = (p) => path.relative(REPO, p);

function walk(dir, ext, out = []) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p, ext, out);
    else if (p.endsWith(ext)) out.push(p);
  }
  return out;
}

const jsFiles = walk(SRC, '.js').sort();
const cssFiles = walk(STYLE, '.css').map((f) => path.basename(f)).sort();
const indexHtml = fs.readFileSync(INDEX, 'utf8');

// ── 1. Stylesheet cascade ────────────────────────────────────────────────────
const linked = [...indexHtml.matchAll(/<link[^>]+href="style\/([^"]+)"/g)].map((m) => m[1]);

for (const f of cssFiles) {
  if (!linked.includes(f)) problems.push(`style/${f} exists but is not linked from index.html`);
}
for (const f of linked) {
  if (!cssFiles.includes(f)) problems.push(`index.html links style/${f}, which does not exist`);
}

// Order: the linked sequence must be a prefix-consistent subset of CASCADE.
const knownLinked = linked.filter((f) => CASCADE.includes(f));
const expectedOrder = CASCADE.filter((f) => knownLinked.includes(f));
if (knownLinked.join(',') !== expectedOrder.join(',')) {
  problems.push(
    `stylesheet cascade order is wrong.\n      linked:   ${linked.join(' -> ')}\n      expected: ${expectedOrder.join(' -> ')}`,
  );
}
if (linked[0] !== 'tokens.css') {
  problems.push('tokens.css must be the first stylesheet: every later file reads its custom properties');
}
// A new stylesheet must be added to CASCADE as well as to index.html, so the
// layering stays a decision someone made rather than an accident of load order.
for (const f of cssFiles) {
  if (!CASCADE.includes(f)) {
    problems.push(`style/${f} is not listed in the documented cascade in ui/tools/check-ui.mjs (add it, in order)`);
  }
}

// ── 2. Duplicate static ids ──────────────────────────────────────────────────
// Only static `id="literal"` in markup templates and index.html. Ids produced
// from variables are dynamically unique by construction and are skipped.
const staticIds = new Map();
const recordId = (id, where) => {
  if (!staticIds.has(id)) staticIds.set(id, new Set());
  staticIds.get(id).add(where);
};
for (const f of jsFiles) {
  const src = fs.readFileSync(f, 'utf8');
  for (const m of src.matchAll(/\bid="([A-Za-z][\w-]*)"/g)) recordId(m[1], rel(f));
}
for (const m of indexHtml.matchAll(/\bid="([A-Za-z][\w-]*)"/g)) recordId(m[1], rel(INDEX));

for (const [id, where] of staticIds) {
  // One file may legitimately build the same id in two template branches that
  // never render together (a modal shell reused for two overlays is the common
  // case) — so only flag ids that appear across DIFFERENT files, which is the
  // case static analysis can decide without rendering.
  if (where.size > 1) {
    problems.push(`id "${id}" is defined as static markup in several modules: ${[...where].join(', ')}`);
  }
}

// ── 3. Module graph ──────────────────────────────────────────────────────────
function exportedNames(src) {
  const names = new Set();
  for (const m of src.matchAll(/^export\s+(?:async\s+)?(?:function|class|const|let|var)\s+([A-Za-z0-9_$]+)/gm)) {
    names.add(m[1]);
  }
  for (const m of src.matchAll(/^export\s*\{([^}]+)\}/gm)) {
    for (const part of m[1].split(',')) {
      const t = part.trim();
      if (t) names.add((t.split(/\s+as\s+/).pop() || t).trim());
    }
  }
  return names;
}

let importCount = 0;
for (const f of jsFiles) {
  const src = fs.readFileSync(f, 'utf8');
  for (const m of src.matchAll(/import\s+(?:([A-Za-z0-9_$]+)\s*,\s*)?(?:\{([^}]*)\})?\s*from\s*['"]([^'"]+)['"]/g)) {
    const [, def, named, spec] = m;
    importCount += 1;
    const target = path.resolve(path.dirname(f), spec);
    if (!fs.existsSync(target)) {
      problems.push(`${rel(f)} imports '${spec}', which does not exist`);
      continue;
    }
    const exp = exportedNames(fs.readFileSync(target, 'utf8'));
    if (def && !exp.has(def)) problems.push(`${rel(f)}: default import '${def}' is not exported by ${spec}`);
    for (const part of (named || '').split(',')) {
      const t = part.trim();
      if (!t) continue;
      const orig = t.split(/\s+as\s+/)[0].trim();
      if (!exp.has(orig)) problems.push(`${rel(f)}: '${orig}' is not exported by ${spec}`);
    }
  }
  for (const m of src.matchAll(/await import\(\s*['"]([^'"]+)['"]\s*\)/g)) {
    importCount += 1;
    const target = path.resolve(path.dirname(f), m[1]);
    if (!fs.existsSync(target)) problems.push(`${rel(f)}: dynamic import '${m[1]}' does not exist`);
  }
}

// ── 4. Runtime id check (optional) ───────────────────────────────────────────
async function runtimeIdCheck() {
  const CHROME = process.env.CHROME_PATH
    || '/Applications/Google Chrome.app/Contents/MacOS/Google Chrome';
  if (!fs.existsSync(CHROME)) {
    notes.push('runtime check skipped: Chrome not found (set CHROME_PATH to enable)');
    return;
  }

  const PORT = 4187;
  const DEBUG = 9347;
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
  await new Promise((r) => server.listen(PORT, '127.0.0.1', r));

  const chrome = spawn(CHROME, [
    '--headless=new',
    `--remote-debugging-port=${DEBUG}`,
    `--user-data-dir=${path.join(os.tmpdir(), 'proxy-rs-ui-check')}`,
    '--no-first-run', '--no-default-browser-check', '--disable-gpu',
    'about:blank',
  ], { stdio: 'ignore' });

  const getJson = (url) => new Promise((resolve, reject) => {
    http.get(url, (res) => {
      let d = '';
      res.on('data', (c) => { d += c; });
      res.on('end', () => { try { resolve(JSON.parse(d)); } catch (e) { reject(e); } });
    }).on('error', reject);
  });

  try {
    let version = null;
    for (let i = 0; i < 60 && !version; i += 1) {
      try { version = await getJson(`http://127.0.0.1:${DEBUG}/json/version`); }
      catch { await new Promise((r) => setTimeout(r, 250)); }
    }
    if (!version) { notes.push('runtime check skipped: devtools did not start'); return; }

    const targets = await getJson(`http://127.0.0.1:${DEBUG}/json/list`);
    const page = targets.find((t) => t.type === 'page');
    const ws = new WebSocket(page.webSocketDebuggerUrl);
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
    await send('Page.navigate', { url: `http://127.0.0.1:${PORT}/index.html` });
    await new Promise((r) => setTimeout(r, 2500));

    const probe = await send('Runtime.evaluate', {
      returnByValue: true,
      expression: `(() => {
        const present = new Set([...document.querySelectorAll('[id]')].map(e => e.id));
        return { present: [...present], linked: [...document.querySelectorAll('link[rel=stylesheet]')].map(l => l.getAttribute('href')) };
      })()`,
    });
    ws.close();

    const present = new Set(probe.result.value.present);

    // Ids the source reads with a static selector. Comments are stripped first:
    // a doc comment illustrating the helper (`$('#id')`) is prose, not a lookup.
    // Ids defined as static markup elsewhere are allowed to be absent from the
    // load-time DOM — modals, the QR dialog and the palette build themselves on
    // first use, which is the documented pattern in components/overlay.js. What
    // this check exists to catch is an id that is neither rendered nor defined
    // anywhere: a typo, whose every `setText` would be a silent no-op.
    const stripComments = (src) => src.replace(/\/\*[\s\S]*?\*\//g, '').replace(/^[ \t]*\/\/.*$/gm, '');
    const readIds = new Map();
    const definedIds = new Set();
    for (const f of jsFiles) {
      const src = stripComments(fs.readFileSync(f, 'utf8'));
      for (const m of src.matchAll(/\$\(\s*['"`]#([A-Za-z][\w-]*)['"`]/g)) {
        if (!readIds.has(m[1])) readIds.set(m[1], new Set());
        readIds.get(m[1]).add(rel(f));
      }
      for (const m of src.matchAll(/getElementById\(\s*['"`]([A-Za-z][\w-]*)['"`]/g)) {
        if (!readIds.has(m[1])) readIds.set(m[1], new Set());
        readIds.get(m[1]).add(rel(f));
      }
      for (const m of src.matchAll(/\bid="([A-Za-z][\w-]*)"/g)) definedIds.add(m[1]);
    }
    for (const m of indexHtml.matchAll(/\bid="([A-Za-z][\w-]*)"/g)) definedIds.add(m[1]);

    const missing = [...readIds].filter(([id]) => !present.has(id) && !definedIds.has(id));
    for (const [id, files] of missing) {
      problems.push(
        `id "${id}" is read by ${[...files].join(', ')} but is neither defined in markup nor present in the rendered page`,
      );
    }

    const lazy = [...readIds].filter(([id]) => !present.has(id) && definedIds.has(id));
    for (const err of runtimeErrors) problems.push(`runtime console error: ${err}`);
    notes.push(
      `runtime: ${present.size} ids rendered, ${readIds.size} static id lookups checked`
      + (lazy.length ? `, ${lazy.length} built on first use (${lazy.map(([id]) => id).join(', ')})` : ''),
    );
  } finally {
    chrome.kill();
    server.close();
  }
}

// ── Report ───────────────────────────────────────────────────────────────────
console.log(`static: ${jsFiles.length} JS modules, ${cssFiles.length} stylesheets, ${importCount} imports`);
console.log(`stylesheets linked: ${linked.length}/${cssFiles.length} (${linked.join(' -> ')})`);

if (process.argv.includes('--runtime')) {
  await runtimeIdCheck();
}

for (const n of notes) console.log(`note: ${n}`);
if (problems.length === 0) {
  console.log('\nno problems found');
  process.exit(0);
}
console.log(`\n${problems.length} problem(s):`);
for (const p of problems) console.log(`  - ${p}`);
process.exit(1);
