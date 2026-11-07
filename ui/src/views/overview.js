/**
 * Overview view: service status, token totals, and the local endpoint
 * reference users copy into their clients.
 *
 * The markup is built in `cards.js`, into the empty `#tab-overview` mount point.
 *
 * The implementation lives in `overview/`; this file is the module `main.js` and
 * the settings views import, so the split changed no import path:
 *
 *   cards.js    the static layout (metric cards, stats row, endpoint list)
 *   status.js   `refreshStatus` — header, identity cards, endpoints
 *   stats.js    `refreshStats` — today's token totals
 *   speed.js    the 输出速度 card and its drill-down table
 *   index.js    `initOverview` — start/stop, card clicks, copy buttons
 *
 * The exports below are exactly the set this module had before the split.
 */

export { renderOverview } from './overview/cards.js';
export { refreshStatus } from './overview/status.js';
export { refreshStats } from './overview/stats.js';
export { refreshSessionMetrics } from './overview/speed.js';
export { initOverview } from './overview/index.js';
