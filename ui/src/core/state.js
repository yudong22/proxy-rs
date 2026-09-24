/**
 * Shared application state.
 *
 * One mutable object, imported by whoever needs it. The UI has no render loop
 * and no framework: a view reads what it needs, the poller/event handler writes
 * what changed, and the view's own render function does the DOM work.
 *
 * Keep this to values more than one module actually reads. Per-view scratch
 * (a palette's selection index, a settings form's loaded flag) belongs inside
 * that view module instead.
 */

export const appState = {
  /** Service lifecycle, last seen from `get_status`. */
  running: false,
  port: 3456,

  /** Which tab is showing. Kept here because the log poller and the tray's
   *  `switchTab` both need to know whether the logs view is live. */
  activeTab: 'overview',

  /** 'table' (DB request log) | 'console' (raw log tail). */
  logsViewMode: 'table',

  /** SQLite request log: current page of rows plus the facets the filters
   *  are built from. */
  requestLogs: [],
  requestLogsTotal: 0,
  requestLogsModels: [],
  requestLogsClients: [],
  requestLogsSessions: [],
  requestLogsPage: 0,
  requestLogsPageSize: 50,

  /** Raw console log entries from `get_logs`. */
  logs: [],

  /** Whether the API key input is currently revealed. */
  keyVisible: false,
};

/** Reset the request-log facet lists, e.g. after the table is cleared. */
export function clearRequestLogFacets() {
  appState.requestLogs = [];
  appState.requestLogsTotal = 0;
  appState.requestLogsPage = 0;
}
