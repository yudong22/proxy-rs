#!/usr/bin/env node
/**
 * Local UI dev loop: edit the front-end, see it live, against a **real
 * proxy-rs-server** — no mocks, no Rust rebuild per edit.
 *
 * ## How it works
 *
 * Vite serves `ui/` with hot module replacement and proxies the gateway's two
 * listeners to a running `proxy-rs-server`. What the browser loads is the real
 * console: real login, real session list from `~/.claude/projects`, real RPC.
 * The only thing Vite substitutes is the module graph, which is what is being
 * edited.
 *
 * **Two ports, not one.** The desktop runs a data plane and a console, on
 * `port` and `port + 1`, and they serve different prefixes:
 *
 *   /api/*    → data plane   (stats, request logs)
 *   /admin/*  → console      (the page, the RPC bridge, login)
 *
 * Both are proxied, each to its own port. Pointing them at one address — which
 * this did — 404s whichever half is not served there.
 *
 * ## Why the old mock dev server is gone
 *
 * `ui/tools/dev.mjs` used to re-implement `src/claude_sessions.rs`'s parser in
 * JavaScript and answer every other command from a stub. That shell was always
 * one field behind the Rust module, and the drift was invisible: a field the
 * mock emitted and Rust did not looked correct here and blank in the app, and a
 * command that failed only in the real gateway (the one class of bug this loop
 * exists to catch) could not fail here at all. The real backend removes both.
 *
 * ## Usage
 *
 *   1. Start the gateway:   task dev-server        (builds once, then runs)
 *   2. Start the UI loop:   task ui-dev
 *
 * `task dev-server` and `task ui-dev` set the same `PROXY_PORT`, so the two
 * always agree. To point at a gateway started some other way:
 *
 *   DEV_PROXY_PORT=3477 task ui-dev
 *
 * (The console is assumed to be one port above it; override with
 * `DEV_CONSOLE_PORT` when the app had to walk past a busy +1.)
 *
 * The console needs a login session. The gateway's login page is proxied too,
 * so the first visit lands there and sets the cookie on this origin; every
 * `/admin/api/rpc/*` call after it carries that session.
 *
 * ## What is not proxied
 *
 * The page and its modules come from `ui/` through Vite — that is the point.
 */

import { createServer } from 'vite';
import baseConfig from '../../vite.config.js';

// `vite.config.js` reads these for the proxy table; set before create.
const proxyPort = Number(process.env.DEV_PROXY_PORT) || 3457;
const consolePort = Number(process.env.DEV_CONSOLE_PORT) || proxyPort;
const prodPort = Number(process.env.PROD_PROXY_PORT) || 3457;
const prodConsolePort = Number(process.env.PROD_CONSOLE_PORT) || prodPort + 1;

const server = await createServer(baseConfig);

await server.listen();
server.printUrls();
console.log(`  test backend : 127.0.0.1:${proxyPort}  (/api/*)`);
console.log(`                 console → ${consolePort === proxyPort ? 'same listener' : `127.0.0.1:${consolePort}`}`);
console.log(`  prod backend : 127.0.0.1:${prodPort}  (/api/*)`);
console.log(`                 console → 127.0.0.1:${prodConsolePort}  (installed app)`);
console.log('  switch them with the DEV badge in the bottom-right corner.');
console.log('  (if a panel is empty, open /admin/login on this origin, or run `task dev-server`)');
console.log('  edit files under ui/ — modules hot-reload, CSS updates without a page reload.');
