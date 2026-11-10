#!/usr/bin/env node
/**
 * Start the headless gateway `ui-dev` proxies to — without go-task.
 *
 * ## Why this exists
 *
 * `Taskfile.yaml` documents `task dev-server` as the first half of the dev loop,
 * but `task` (go-task) is not installed by default on any of this project's
 * machines. The failure mode is indistinguishable from "the backend is down":
 * `ui-dev` starts fine, the page loads, and every panel that reads `/api/*` gets
 *
 *     [vite] http proxy error: /api/stats/today
 *     Error: connect ECONNREFUSED 127.0.0.1:4457
 *
 * — because nothing was ever listening on 4457. That is the exact symptom this
 * script removes: the loop's two halves now both run through npm, which is
 * already required by `npm ci`.
 *
 * ## What it does (and nothing more)
 *
 *   1. Builds `proxy-rs-server` once (cargo, once — not per edit).
 *   2. Points `$HOME/.proxy-rs-dev/gui-settings.json` at the test port. This is
 *      load-bearing rather than tidy: the gateway reads its port from that file,
 *      so a stale value from an older run would leave the gateway on one port
 *      and `vite.config.js` proxying to another. (That drift is a real bug this
 *      repo has had; both sides now derive the number from the same offset.)
 *   3. Runs the gateway in the foreground, with `PROXY_API_TOKEN=dev` so the
 *      auto-login in `vite.config.js` has a token to submit.
 *
 * ## Ports
 *
 * Kept in step with `vite.config.js` by deriving them the same way:
 * `PROD_PORT` (default 3457) is the installed app's data plane, and the test
 * gateway sits `DEV_PORT_OFFSET` (default 1000) above it — i.e. 4457. The app
 * also owns `PROD_PORT + 1` for its console, which is why the test instance is
 * far away rather than adjacent, so the two pairs stay distinguishable in
 * `lsof` output.
 */

import { spawn, spawnSync } from 'node:child_process';
import { mkdirSync, readFileSync, writeFileSync } from 'node:fs';
import { homedir } from 'node:os';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';

const here = dirname(fileURLToPath(import.meta.url));
const repo = resolve(here, '../..');

const PROD_PORT = Number(process.env.PROD_PROXY_PORT) || 3457;
const DEV_PORT_OFFSET = Number(process.env.DEV_PORT_OFFSET) || 1000;
const PROXY_PORT = Number(process.env.DEV_PROXY_PORT) || PROD_PORT + DEV_PORT_OFFSET;

/** The token auto-login submits. Same value `Taskfile.yaml` uses. */
const TOKEN = process.env.DEV_API_TOKEN || 'dev';

const devDir = join(homedir(), '.proxy-rs-dev');
const settingsPath = join(devDir, 'gui-settings.json');

function log(line) {
  process.stdout.write(`${line}\n`);
}

// 1) Build once. `npm run dev-server` is expected to be long-lived, so paying
//    for a compile at start is fine — and cargo is incremental, so a restart
//    with no Rust changes is a no-op.
log('⏳ building proxy-rs-server (once per session, incremental after that)…');
const build = spawnSync('cargo', ['build', '--bin', 'proxy-rs-server'], {
  cwd: repo,
  stdio: 'inherit',
});
if (build.error) {
  log(`✗ could not run cargo: ${build.error.message}`);
  log('  install Rust (https://rustup.rs) or run the gateway another way;');
  log('  `ui-dev` only needs *something* listening on the test port.');
  process.exit(1);
}
if (build.status !== 0) {
  log('✗ cargo build failed — see the output above.');
  process.exit(build.status ?? 1);
}

// 2) Always rewrite the port. A leftover file from an older run is exactly how
//    the gateway and the Vite proxy came to disagree about the port before.
mkdirSync(devDir, { recursive: true });
let settings = {};
try {
  settings = JSON.parse(readFileSync(settingsPath, 'utf8'));
} catch {
  // Missing or unparsable: start from a clean object rather than failing. The
  // file is a cache of dev preferences, not state worth protecting.
}
settings.port = PROXY_PORT;
writeFileSync(settingsPath, `${JSON.stringify(settings, null, 2)}\n`);

log(`🧪 test gateway : http://127.0.0.1:${PROXY_PORT}  (/api/* and /admin/*, one listener)`);
log(`🚀 installed app: http://127.0.0.1:${PROD_PORT}  (/api/*), console ${PROD_PORT + 1}`);
log(`   data dir    : ${devDir}`);
log('   switch backends with the DEV badge in the page, bottom-right.');

// 3) Foreground, so Ctrl-C stops it and its output is the operator's log.
const server = spawn(join(repo, 'target/debug/proxy-rs-server'), [], {
  cwd: repo,
  stdio: 'inherit',
  env: {
    ...process.env,
    PROXY_DATA_DIR: devDir,
    PROXY_API_TOKEN: TOKEN,
  },
});
server.on('exit', (code, signal) => {
  if (signal) process.kill(process.pid, signal);
  else process.exit(code ?? 0);
});
