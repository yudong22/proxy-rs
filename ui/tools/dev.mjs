#!/usr/bin/env node
/**
 * Local UI dev loop: edit the front-end, see it live, against a **real
 * proxy-rs-server** — no mocks, no Rust rebuild per edit.
 *
 * ## How it works
 *
 * Vite serves `ui/` with hot module replacement and proxies everything the
 * gateway owns (`/admin/*`, `/api/*`) to a running `proxy-rs-server`. What the
 * browser loads is the real console: real login, real session list from
 * `~/.claude/projects`, real RPC. The only thing Vite substitutes is the module
 * graph, which is what is being edited.
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
 *      or:                  PROXY_API_TOKEN=dev ./target/debug/proxy-rs-server
 *   2. Start the UI loop:   task ui-dev
 *
 *   # another gateway
 *   BACKEND=http://127.0.0.1:3456 task ui-dev
 *
 * The console needs a login session. The gateway's login page is proxied too,
 * so the first visit lands there and sets the cookie on this origin; every
 * `/admin/api/rpc/*` call after it carries that session.
 *
 * ## What is not proxied
 *
 * The page and its modules come from `ui/` through Vite — that is the point.
 * Anything under `PROXY_PATHS` goes to the gateway verbatim.
 */

import { createServer } from 'vite';
import baseConfig from '../../vite.config.js';

const BACKEND = (process.env.BACKEND || 'http://127.0.0.1:3456').replace(/\/+$/, '');
// `vite.config.js` reads BACKEND itself for the proxy table; set before create.
process.env.BACKEND = BACKEND;

const server = await createServer(baseConfig);

await server.listen();
server.printUrls();
console.log(`  backend: ${BACKEND}`);
console.log('  (if a panel is empty, open /admin/login on this origin, or start the gateway with `task dev-server`)');
console.log('  edit files under ui/ — modules hot-reload, CSS updates without a page reload.');
