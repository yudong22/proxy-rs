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
import { initOverview, refreshStatus, refreshStats, renderOverview } from './views/overview.js';
import { renderTabs, setActiveTab } from './views/tabs.js';
import {
  initLogs,
  renderLogsView,
  setLogsViewMode,
  fetchLogs,
  pollRequestLogs,
} from './views/logs.js';
import { fetchRequestLogs } from './components/table.js';
import { initSettings, loadSettings, renderSettings } from './views/settings.js';
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

/** Show one tab and run whatever that view needs on entry. */
export function switchTab(tabName) {
  appState.activeTab = tabName;

  setActiveTab(tabName);
  showPane(tabName);

  if (tabName === 'logs') {
    if (appState.logsViewMode === 'table') fetchRequestLogs();
    else fetchLogs();
  } else if (tabName === 'settings') {
    loadSettings();
  }
}

// Backend entry points (see the file header).
window.switchTab = switchTab;
window.refreshStatus = refreshStatus;

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
renderSettings();

initModal();
initConfirm();
initOverview();
initLogs();
initSettings();
initPalette(switchTab);

// Tab clicks
for (const btn of $$('.tab')) {
  btn.addEventListener('click', () => switchTab(btn.dataset.tab));
}

refreshStatus();
refreshStats();
loadSettings();

// Activate the default tab and put the logs view into its default mode. Both
// are explicit now: index.html carries no `active` class or inline `display`,
// so nothing is showing until these run.
switchTab(appState.activeTab);
setLogsViewMode(appState.logsViewMode);
