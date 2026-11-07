/**
 * Today's token totals.
 *
 * Split out of `overview.js`. Read over HTTP when the listener is up, falling
 * back to the IPC command when it is not — see `core/api.js`.
 */

import { setText } from '../../core/dom.js';
import { getStats } from '../../core/api.js';
import { formatNumber } from '../../lib/format.js';

/**
 * Read today's token totals.
 *
 * `getStats` reads `GET /api/stats/today` from the local listener and falls back
 * to the `get_stats` Tauri command when the service is stopped — see
 * `core/api.js`. Both return the identical payload (`src/api.rs` defines it
 * once), so this function does not care which transport answered.
 */
export async function refreshStats() {
  let s;
  try {
    s = await getStats();
  } catch (err) {
    console.error('refreshStats error:', err);
    return;
  }
  if (!s) return;

  setText('stat-requests', formatNumber(s.requests_total));
  setText('stat-tokens-total', formatNumber(s.tokens_total));
  setText('stat-cache-pct', `${s.cache_hit_pct || 0}%`);
  setText('stat-requests-failed', formatNumber(s.requests_failed));
  setText('metric-overrides', formatNumber(s.overrides_total));
}
