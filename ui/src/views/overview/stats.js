/**
 * Today's token totals.
 *
 * Split out of `overview.js`. Read over HTTP when the listener is up, falling
 * back to the IPC command when it is not — see `core/api.js`.
 */

import { setValue } from '../../core/dom.js';
import { getStats } from '../../core/api.js';
import { formatNumber } from '../../lib/format.js';

/**
 * Read today's token totals.
 *
 * `getStats` reads `GET /api/stats/today` from the local listener and falls back
 * to the `get_stats` Tauri command when the service is stopped — see
 * `core/api.js`. Both return the identical payload (`src/api.rs` defines it
 * once), so this function does not care which transport answered.
 *
 * The values are written through `setValue`, which also clears each card's
 * skeleton. A failed read logs and returns, leaving the skeletons in place: a
 * card still showing the loading bar is honest, a card showing `-` is not.
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

  setValue('stat-requests', formatNumber(s.requests_total));
  setValue('stat-tokens-total', formatNumber(s.tokens_total));
  setValue('stat-cache-pct', `${s.cache_hit_pct || 0}%`);
  setValue('stat-requests-failed', formatNumber(s.requests_failed));
  setValue('metric-overrides', formatNumber(s.overrides_total));
}
