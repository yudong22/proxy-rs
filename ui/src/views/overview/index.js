/**
 * Wiring for the overview view's interactions.
 *
 * Split out of `overview.js` so the public file is a pure re-export and this is
 * the only place that knows which card does what.
 */

import { $, setHidden } from '../../core/dom.js';
import { invoke } from '../../core/ipc.js';
import { appState } from '../../core/state.js';
import { toast } from '../../components/toast.js';
import { refreshStatus } from './status.js';
import { refreshSessionMetrics } from './speed.js';

/** Wire the interactions that belong to this view. Called once, from main.js. */
export function initOverview() {
  // Start / stop the service. `start_service` rejects when the port could not
  // be bound, so a busy port surfaces here instead of looking like success.
  const toggle = $('#btn-toggle-service');
  toggle?.addEventListener('click', async () => {
    toggle.disabled = true;
    try {
      await invoke(appState.running ? 'stop_service' : 'start_service');
      await refreshStatus();
    } catch (err) {
      console.error('Toggle service failed:', err);
      toast('操作失败: ' + err, 'error');
      await refreshStatus();
    } finally {
      toggle.disabled = false;
    }
  });

  // The 输出速度 card expands the per-turn generation detail. It re-reads on
  // open so the panel describes the turn that just finished rather than
  // whichever one was current at the last 10 s poll.
  $('#stat-card-speed')?.addEventListener('click', async () => {
    const card = $('#stat-card-speed');
    const detail = $('#speed-detail');
    if (!card || !detail) return;
    const open = detail.hasAttribute('hidden');
    setHidden(detail, !open);
    card.classList.toggle('active', open);
    if (open) await refreshSessionMetrics();
  });

  // The 今日 override 次数 card jumps to the request log, where each override's
  // reason is spelled out in the override column.
  $('#stat-card-overrides')?.addEventListener('click', () => {
    window.switchTab('logs');
  });

  // The 剩余积分 card refreshes on click. Debounced **per identity**: each
  // click costs one upstream billing request per identity, so hammering the
  // card would hammer the billing endpoint — but the window must not follow the
  // *card*. Otherwise refreshing account A, then switching to B, left B locked
  // out for the rest of A's window, which is what the 3 分钟 prompt was
  // complaining about. MATCHES_COOLDOWN_MS mirrors the server-side failover
  // memory TTL (3 min).
  const MATCHES_COOLDOWN_MS = 3 * 60 * 1000;
  /** Identity id → wall-clock ms of its last successful refresh. */
  const lastPointsRefreshById = new Map();
  let refreshingPoints = false;

  $('#stat-card-points')?.addEventListener('click', async () => {
    if (refreshingPoints) return;

    const identityId = appState.currentIdentity?.id || '';
    const lastMs = lastPointsRefreshById.get(identityId) || 0;
    const elapsed = Date.now() - lastMs;
    if (elapsed < MATCHES_COOLDOWN_MS) {
      const remainSec = Math.ceil((MATCHES_COOLDOWN_MS - elapsed) / 1000);
      toast(`该账号积分刚刚刷新过，${remainSec}s 后可再次刷新`);
      return;
    }

    const card = $('#stat-card-points');
    const value = $('#metric-points');
    refreshingPoints = true;
    // The in-flight text keeps the `pending` class so the skeleton bar stays
    // behind it and the card's height does not move; the dim + pulse come from
    // `.metric-card.loading` on the card itself.
    if (value) {
      value.textContent = '⏳ 刷新中…';
      value.classList.add('pending');
    }
    if (card) card.classList.add('loading');
    try {
      await invoke('wb_refresh_points');
      lastPointsRefreshById.set(identityId, Date.now());
      // `refreshStatus` in the finally block repaints the card from
      // `get_status`, so the fresh balance flows through the same path as a
      // status poll — one way for the card to update.
      toast('剩余积分已刷新');
    } catch (err) {
      console.error('refresh points failed:', err);
      toast('刷新剩余积分失败: ' + err, 'error');
      // Allow a retry immediately: nothing was fetched, so the window would
      // otherwise pointlessly block the user for three minutes.
      lastPointsRefreshById.delete(identityId);
    } finally {
      refreshingPoints = false;
      if (card) card.classList.remove('loading');
      // Repaints the card from `get_status`. This deliberately skips the
      // session-metrics and stats reads: the speed card and the totals row are
      // unchanged by a balance refresh, and on a slow link a combined refresh
      // would leave the points card showing "⏳ 刷新中…" until all three
      // answered. The 10 s pollers pick the other two up on their own.
      await refreshStatus();
    }
  });

  // Copy-to-clipboard buttons on the endpoint rows.
  for (const btn of document.querySelectorAll('.btn-copy[data-copy]')) {
    btn.addEventListener('click', async () => {
      const el = document.getElementById(btn.dataset.copy);
      if (!el) return;
      await navigator.clipboard.writeText(el.textContent.trim());
      const original = btn.textContent;
      btn.textContent = '已复制!';
      setTimeout(() => { btn.textContent = original; }, 1500);
    });
  }
}
