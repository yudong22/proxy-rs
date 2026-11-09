/**
 * Application entry point.
 *
 * Wires the view modules together and owns the two globals the Rust side
 * reaches for. The backend drives the UI in two places via
 * `webview.eval(...)`:
 *
 *   window.refreshStatus()   — after start/stop, from the tray or the GUI
 *   window.switchTab('logs') — the tray's 显示日志 item
 *
 * `eval` runs in global scope, so these must be assigned to `window`
 * explicitly: a module's top-level `function` is not a global.
 */

import { $, $$ } from './core/dom.js';
import { listen } from './core/ipc.js';
import { appState } from './core/state.js';
import { initModal } from './components/modal.js';
import { initConfirm } from './components/confirm.js';
import {
  initOverview,
  refreshStatus,
  refreshStats,
  refreshSessionMetrics,
  renderOverview,
} from './views/overview.js';
import { renderTabs, setActiveTab, TABS } from './views/tabs.js';
import {
  initLogs,
  renderLogsView,
  setLogsViewMode,
  fetchLogs,
  pollRequestLogs,
} from './views/logs.js';
import { fetchRequestLogs } from './components/table.js';
import {
  initSessions,
  loadSessions,
  renderSessions,
  consumeDeepLink,
} from './views/sessions/index.js';
import { initSettings, loadSettings, renderSettings } from './views/settings/index.js';
import { initPalette, close as closePalette } from './views/palette.js';

// ── Tab routing ────────────────────────────────────────────────

/**
 * Show exactly one tab pane.
 *
 * The panes themselves are written by `renderTabs` next door; this only flips
 * the `active` class, so the two places that know a tab's identity are one list
 * (TABS) plus the pane's own id.
 */
function showPane(tabName) {
  for (const pane of $$('.tab-content')) {
    pane.classList.toggle('active', pane.id === `tab-${tabName}`);
  }
}

/** The default tab when the URL carries no (or an unknown) hash. */
const DEFAULT_TAB = 'overview';

/**
 * The tab named by `location.hash`, or the default.
 *
 * A hash makes a tab addressable — refresh keeps it, a link from another
 * window lands on it, and the back button walks tab history the way a set of
 * pages would. Only a known key is honoured: a stale `#old-view` must not leave
 * the console with every pane hidden.
 *
 * ## Why only the first segment names the tab
 *
 * `#sessions/<uuid>` is a deep link into one session (the backend's login
 * redirect emits it, and the console hands it out as the shareable address).
 * The router's job is only to pick the pane, so it reads the part before the
 * first `/` and leaves the rest untouched — the sessions view owns the id, and
 * duplicating that rule here is how a second, subtly different id parser gets
 * written. Lower-casing is applied to the tab segment alone: the id is
 * case-sensitive by shape and must reach the view verbatim.
 */
function tabFromHash() {
  const segment = window.location.hash.replace(/^#/, '').split('/')[0].toLowerCase();
  return TABS.some((t) => t.key === segment) ? segment : DEFAULT_TAB;
}

/**
 * Show one tab and run whatever that view needs on entry.
 *
 * The URL is the source of truth: every switch writes the hash (which itself
 * fires `hashchange`), and `applyTab` is the single place that does the DOM
 * work, so a click, a link and the back button cannot drift apart.
 *
 * Only the tab segment is compared, not the whole hash: on `#sessions/<uuid>`
 * the tab is already "sessions", and rewriting the hash because the two strings
 * differ would throw the deep link away on the very first paint. Switching *away*
 * and back is what discards it, which is what a tab click should mean.
 */
export function switchTab(tabName) {
  const key = TABS.some((t) => t.key === tabName) ? tabName : DEFAULT_TAB;
  const current = window.location.hash.replace(/^#/, '').split('/')[0].toLowerCase();
  if (current !== key) {
    window.location.hash = key;
    // Navigating to the same hash fires no event; the guard above only skips
    // the write, so the apply below always runs.
  }
  applyTab(key);
}

/** Paint one tab and run its on-entry work. */
function applyTab(tabName) {
  appState.activeTab = tabName;

  setActiveTab(tabName);
  showPane(tabName);

  if (tabName === 'logs') {
    if (appState.logsViewMode === 'table') fetchRequestLogs();
    else fetchLogs();
  } else if (tabName === 'sessions') {
    // Reads the local transcript directory; once per page lifetime, plus the
    // toolbar's explicit 刷新 (sessions are files, not a live counter).
    //
    // `consumeDeepLink` runs after the list, never before: it selects a session,
    // and a selection painted against a list that has not loaded yet would mark
    // no row and scroll nowhere. It re-reads the hash itself, so a `#sessions/x`
    // arrival and a plain `#sessions` click take the same path.
    Promise.resolve(loadSessions()).then(consumeDeepLink, consumeDeepLink);
  } else if (tabName === 'settings') {
    loadSettings();
  }
}

// Backend entry points (see the file header).
window.switchTab = switchTab;
window.refreshStatus = refreshStatus;

// The hash is the router: back/forward and a pasted `#logs` link both land
// here, and the hash was already honoured on first paint below.
//
// A hash change *within* the sessions tab (`#sessions/<a>` → `#sessions/<b>`) is
// not a tab change, so it must not re-run the tab's on-entry work — but it does
// have to move the selection, otherwise a pasted link to another session in the
// same tab silently does nothing.
window.addEventListener('hashchange', () => {
  const key = tabFromHash();
  if (key !== appState.activeTab) applyTab(key);
  else if (key === 'sessions') consumeDeepLink();
});

// ── Escape closes whatever is on top ───────────────────────────
// The palette and the confirm dialog both handle their own Escape; this is the
// fallback for the transition where neither is focused.

document.addEventListener('keydown', (e) => {
  if (e.key !== 'Escape') return;
  if ($('#palette-overlay')?.classList.contains('open')) {
    closePalette();
    return;
  }
  $('#request-modal-overlay')?.classList.remove('active');
});

// ── Service-state push ─────────────────────────────────────────
// Replaces the old 2s `setInterval(refreshStatus, 2000)`. The backend emits
// `service-state` whenever the listener comes up, goes down, or fails to bind —
// i.e. at exactly the moments the status actually changes.
listen('service-state', () => {
  refreshStatus();
});

// ── Stats ──────────────────────────────────────────────────────
// Stats deliberately stay on a timer rather than an event. The write happens in
// `src/proxy.rs`, which is Layer 3 and has no `AppHandle`; emitting from there
// would mean threading Tauri through the request path for a cosmetic refresh.
// 10s is slow enough to be invisible in CPU terms and keeps 请求总数 /
// Token 总量 moving while a long session runs.
setInterval(refreshStats, 10000);
// The 输出速度 card reads the latest *turn*, which changes as soon as a request
// finishes — so it polls alongside the stats rather than only on service-state
// events (those fire for start/stop, not for a completed conversation turn).
setInterval(refreshSessionMetrics, 10000);

// ── Log polling ────────────────────────────────────────────────
// Only while the logs tab is actually showing: an idle tab does no work.
// The console tail is small and cheap to re-read; the DB table is paged, so it
// is polled through pollRequestLogs(), which honours 实时刷新.
setInterval(() => {
  if (appState.activeTab !== 'logs') return;
  if (appState.logsViewMode === 'table') pollRequestLogs();
  else fetchLogs();
}, 2000);

// ── Boot ───────────────────────────────────────────────────────
// Render first, wire second: every `init*` below binds listeners to elements
// the render pass has just created, so an empty mount point in index.html is
// the only thing this file may assume.

renderTabs();
renderOverview();
renderLogsView();
renderSessions();
renderSettings();

initModal();
initConfirm();
initOverview();
initLogs();
initSessions();
initSettings();
initPalette(switchTab);

// Tab clicks
for (const btn of $$('.tab')) {
  btn.addEventListener('click', () => switchTab(btn.dataset.tab));
}

// Stats are read after the status lands, not beside it: the HTTP base URL is
// built from `appState.port`, which `refreshStatus` is what fills in. Racing
// the two would send the first `/api/stats/today` to the default port even when
// the user configured another one — a failed fetch that only the 10s poll (or
// the IPC fallback, when running inside Tauri) would paper over.
refreshStatus().then(refreshStats, refreshStats);
refreshSessionMetrics();
loadSettings();

// Activate the default tab and put the logs view into its default mode. Both
// are explicit now: index.html carries no `active` class or inline `display`,
// so nothing is showing until these run.
// Honour the URL: a deep link (`#sessions`) wins over the default tab.
if (!window.location.hash) window.history.replaceState(null, '', `#${DEFAULT_TAB}`);
switchTab(tabFromHash());
setLogsViewMode(appState.logsViewMode);
