#!/usr/bin/env node
/**
 * Checks on the Vite build output (`ui-dist/`).
 *
 * `ui/` is checked statically by `check-ui.mjs` and behaviourally by `smoke.mjs`
 * — both against the source tree. Neither says anything about what actually
 * ships: a build can drop a stylesheet, place a chunk where the console's route
 * cannot reach it, or rewrite the entry tag the server bridge is injected into.
 * Each of those is invisible to the source checks and fatal at runtime, so they
 * are asserted here, against the real output.
 *
 * Run: node ui/tools/check-dist.mjs   (after `npm run build:ui`)
 *
 * Exits 1 on any problem.
 */

import fs from 'node:fs';
import path from 'node:path';
import { fileURLToPath } from 'node:url';

const REPO = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..', '..');
const UI = path.join(REPO, 'ui');
const DIST = path.join(REPO, 'ui-dist');

const problems = [];
const fail = (msg) => problems.push(msg);

if (!fs.existsSync(DIST)) {
  console.error('ui-dist/ does not exist — run `npm run build:ui` first.');
  process.exit(1);
}

const html = fs.readFileSync(path.join(DIST, 'index.html'), 'utf8');
const files = [];
(function walk(dir) {
  for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
    const p = path.join(dir, e.name);
    if (e.isDirectory()) walk(p);
    else files.push(path.relative(DIST, p).split(path.sep).join('/'));
  }
})(DIST);

// ── 1. Every URL the page references exists in the build ─────────────────────
// A `./x` href/src that `build.rs` would not find is a 404 in the console and
// an empty file in the Tauri webview. Relative URLs resolve against the page,
// which is served at the root of both.
const urls = [...html.matchAll(/(?:href|src)="(\.\/[^"]+)"/g)].map((m) => m[1].slice(2));
for (const url of urls) {
  if (!files.includes(url)) fail(`index.html references ${url}, which the build did not produce`);
}

// ── 2. Exactly one entry script, in the shape console.rs matches ─────────────
const scripts = [...html.matchAll(/<script type="module"[^>]*>/g)].map((m) => m[0]);
if (scripts.length !== 1) {
  fail(`expected one module <script>, found ${scripts.length}`);
} else if (!/^<script type="module" src="src\/main-[^"]+\.js">$/.test(scripts[0])) {
  // console.rs replaces the literal `<script type="module" src="src/main.js">`
  // — see the `proxy-rs:console-script-tag` plugin in vite.config.js. Any
  // attribute Vite adds back (crossorigin, integrity) breaks that match.
  fail(`the entry script tag is not in the shape console.rs injects into: ${scripts[0]}`);
}

// ── 3. Every stylesheet index.html linked is in the bundle ───────────────────
// The source is 20 <link> tags whose order is the cascade. Vite follows them
// and concatenates in that order; the check here is that it followed all of
// them, because a link Vite silently skips is a whole layer missing.
const sourceLinks = [...fs.readFileSync(path.join(UI, 'index.html'), 'utf8')
  .matchAll(/<link[^>]+href="style\/([^"]+)"/g)].map((m) => m[1]);
const builtCss = files.filter((f) => f.startsWith('style/') && f.endsWith('.css'));
if (builtCss.length !== 1) {
  fail(`expected one bundled stylesheet, found ${builtCss.length}: ${builtCss.join(', ')}`);
} else {
  const css = fs.readFileSync(path.join(DIST, builtCss[0]), 'utf8');
  // Each source stylesheet's rules end up in the bundle; a layer that was
  // dropped shows up as one of its distinctive selectors going missing. These
  // are taken from the files themselves (the first selector each one defines),
  // not guessed — a marker that never existed in the source would report a
  // false failure.
  const markers = {
    'tokens.css': '--surface2:',
    'base.css': '[hidden]',
    'controls.css': '.btn-primary',
    'badges.css': '.badge-pill',
    'overlays.css': '.modal',
    'layout.css': '.tab-content',
    'cards.css': '.metric-card',
    'logs.css': '.logs-panel',
    'logs-data-table.css': '.data-table',
    'logs-console.css': '.log-line',
    'detail-table.css': '.detail-table',
    'view-overview.css': '.stat-detail',
    'view-settings.css': '.form-section',
    'view-logs.css': '.logs-console-view',
    'view-sessions.css': '.sess-row',
    'view-sessions-composer.css': '.sess-input-field',
    'view-sessions-transcript.css': '.sess-pre',
    'wb-pool.css': '.wb-credential-row',
    'wb-qr.css': '.wb-qr',
    'wb-report.css': '.wb-checkin-report-modal',
    // Tailwind emits only the utilities some class in `ui/src` uses, so the
    // marker is a utility `.shadow` (the `shadow:` theme key added in
    // tailwind.config.js, used by `.metric-card`-style cards) rather than a
    // layer header, which minification removes.
    'tailwind.css': '.shadow{',
  };
  for (const name of sourceLinks) {
    const marker = markers[name];
    if (!marker) {
      fail(`style/${name} is linked but check-dist.mjs has no marker for it — add one`);
      continue;
    }
    if (!css.includes(marker)) fail(`style/${name} appears to be missing from the bundle (no "${marker}")`);
  }
}

// ── 4. Tailwind's preflight is still disabled ────────────────────────────────
// `tailwind.config.js` turns preflight off because its reset would land after
// base.css. Turning it back on is a one-word edit that changes the box model of
// every existing element and still leaves the page *looking* mostly right —
// the kind of regression that gets noticed a week later. Preflight's signature
// rule (`border-style: solid` on every element) is absent iff it is off.
const bundledCss = builtCss.length === 1
  ? fs.readFileSync(path.join(DIST, builtCss[0]), 'utf8')
  : '';
if (bundledCss.includes('border-style:solid') || bundledCss.includes('border-style: solid')) {
  fail('Tailwind preflight appears to be enabled (found border-style: solid) — '
    + 'it lands after base.css and changes the box model of every existing rule');
}

// ── 5. Nothing landed outside the three routes console.rs serves ─────────────
// build.rs walks `src/`, `style/` and `assets/` and panics on an unknown
// extension, so a stray file outside them would fail the Rust build — but only
// after this ran. Catch it here with a clearer message.
//
// `index.html` and `bootstrap.html` are the two HTML entry points. They are
// served by special-cased routes / the Tauri window's own `src` attribute
// rather than by a directory walk, so they are allowed at the root — and the
// check that they exist is the point of the rule, not an exception to it.
const roots = ['src/', 'style/', 'assets/'];
const topLevelHtml = new Set(['index.html', 'bootstrap.html']);
for (const f of files) {
  if (topLevelHtml.has(f)) continue;
  if (!roots.some((r) => f.startsWith(r))) {
    fail(`${f} is outside the served roots (src/, style/, assets/) — console.rs cannot serve it`);
  }
}

// Both entry points must be present: the console route serves `index.html` and
// `tauri.conf.json` points the window at `bootstrap.html`. A missing
// `bootstrap.html` is a blank app window, not a 404 the user can diagnose.
for (const f of topLevelHtml) {
  if (!files.includes(f)) fail(`${f} is missing from the build — the app or console entry would 404`);
}

if (problems.length) {
  console.log(`${problems.length} problem(s) in ui-dist/:`);
  for (const p of problems) console.log(`  - ${p}`);
  process.exit(1);
}

console.log(`build: ${files.length} files, ${sourceLinks.length} stylesheets, entry script shape ok`);
