/**
 * Growth-task progress: the polled progress buffer and the toolbar run button.
 *
 * Split out of `credential-pool.js`. A run is long and streamed, so it owns
 * state (in-flight flag, grace window, the line buffer) and a timer; keeping
 * that beside the list renderer made both harder to follow.
 */

import { $ } from '../../../core/dom.js';
import { invoke } from '../../../core/ipc.js';
import { refreshStatus } from '../../overview.js';
import { toast } from '../../../components/toast.js';
import { showWbStatus } from './status.js';
import { renderIdentityPool } from './pool-table.js';

/** Whether a growth run is in flight (drives the progress line). */
let growthRunActive = false;
/** Set once the run's summary is written, so late events cannot overwrite it. */
let growthRunFinished = true;
/**
 * Timestamp until which the poller keeps talking to the backend even though the
 * run is over. A run resolves before its last progress lines are necessarily
 * flushed, so the drain continues for a short grace window to catch the tail.
 * Without it the final lines are dropped; without the window the poller would
 * keep asking forever (see `drainProgress`).
 */
let growthPollGraceUntil = 0;
/** Recent progress lines, newest last. */
let growthProgressLines = [];
/** Poll draining the backend's progress buffer while a run is in flight. */
let growthProgressTimer = null;

/**
 * Paint the growth progress box.
 *
 * `scroll` scrolls it into view — the run button lives in the toolbar above the
 * list, so a user who clicked there is not looking at this area yet. Only on
 * start: scrolling on every update would fight the user scrolling the log.
 *
 * @param {boolean} scroll
 */
function paintGrowthProgress(scroll) {
  const el = $('#wb-growth-progress');
  if (!el) return;
  el.hidden = false;
  el.textContent = growthProgressLines.join('\n');
  if (scroll) el.scrollIntoView({ block: 'nearest' });
}

// 增长积分：跑一遍成长任务并领奖。结果里「跳过」和「未推进」必须显式告知，
// 否则用户会以为"跑了=全拿到了"。
// Growth progress stream, **polled**.
//
// The backend used to push these as Tauri events; a shared HTTP console has
// no push channel, so progress now lands in a server-side buffer that
// `get_growth_progress` drains (take-and-clear). The poll runs only while a
// run is in flight and its summary has not yet been written, so an idle view
// costs nothing. Gating on a strict "still in flight" flag lost the tail of
// very fast runs for the events path (the run resolves before its last
// events arrive), so the poll deliberately keeps draining until
// `growthRunFinished` — the buffer is empty by then, and the extra poll is
// free.
function appendProgress(p) {
  if (!p || !p.phase) return;
  const lines = growthProgressLines;
  if (p.phase === 'account_start') {
    lines.push(`⏳ 第 ${p.index}/${p.total} 个账号：${p.label}`);
  } else if (p.phase === 'task_claimed') {
    lines.push(`  ✓ ${p.code} +${p.credit} 分`);
  } else if (p.phase === 'task_stalled') {
    lines.push(`  · ${p.code} 未推进，已跳过`);
  } else if (p.phase === 'account_done') {
    lines.push(
      p.claimed > 0
        ? `  → ${p.label}：${p.claimed} 个任务 +${p.credits} 分`
        : `  → ${p.label}：本次无可领取`,
    );
  } else if (p.phase === 'finished') {
    lines.push(`全部完成：${p.claimed} 个任务 +${p.credits} 分`);
  }
  // Keep only the tail: a long pool run would otherwise fill the line.
  if (lines.length > 6) lines.splice(0, lines.length - 6);
  paintGrowthProgress(false);
}

// Dual-channel progress: listen() is active on desktop/harness, while HTTP
// console uses polling (where listen() degrades cleanly to a no-op).
// Drain the backend's progress buffer once.
//
// The endpoint is take-and-clear, so each call yields only what was
// produced since the last one. A failure is ignored: the next tick retries
// (a listener restart mid-run is transient, and the run itself will fail
// loudly if it was real).
//
// The network call happens **only** while a run is in flight or inside the
// short grace window that follows it. The timer itself ticks forever, but an
// idle settings page must not ask the backend four times a second — that
// was the actual bug: the poller used to be gated on `growthRunFinished`,
// which starts out "not finished", so every idle page hammered
// `get_growth_progress` at 4 Hz forever.
async function drainProgress(force = false) {
  if (!force && !growthRunActive && Date.now() > growthPollGraceUntil) return;
  let res;
  try {
    res = await invoke('get_growth_progress');
  } catch {
    return;
  }
  for (const p of res?.lines || []) {
    appendProgress(p);
  }
}

/**
 * Start the poll.
 *
 * A re-init (view remount) must not stack a second poll on the first, so the
 * previous timer is dropped here rather than by the caller.
 */
export function initGrowthProgress() {
  if (growthProgressTimer) clearInterval(growthProgressTimer);
  // 250ms, not 1s: a whole-pool run can finish inside a second, and a slow
  // first poll would report nothing at all for the runs that matter most.
  growthProgressTimer = setInterval(drainProgress, 250);
}

/** Wire the toolbar's 增长积分 button. */
export function initGrowthRun() {
  $('#btn-wb-growth')?.addEventListener('click', async () => {
    const btn = $('#btn-wb-growth');
    if (btn) {
      btn.disabled = true;
      btn.textContent = '⏳ 增长中...';
    }
    // Progress state: the poller appends lines while `growthRunActive`.
    growthRunActive = true;
    growthRunFinished = false;
    growthPollGraceUntil = 0;
    growthProgressLines = ['开始运行成长任务…'];
    paintGrowthProgress(true);

    // Heartbeat: a whole-pool run staggers accounts 2–5 s apart, so even with
    // events there can be multi-second gaps with nothing new to say. Without a
    // ticking clock the UI looks frozen exactly when the user is watching it.
    const startedAt = Date.now();
    const heartbeat = setInterval(() => {
      if (!growthRunActive) return;
      const secs = Math.floor((Date.now() - startedAt) / 1000);
      const el = $('#wb-growth-progress');
      if (!el) return;
      const last = growthProgressLines[growthProgressLines.length - 1] || '';
      el.textContent = `${growthProgressLines.join('\n')}\n… 运行中 ${secs}s`;
      // Keep the box where the user's eye is: the button is in the toolbar above.
      if (el.scrollIntoView && secs % 5 === 0 && last) {
        el.scrollIntoView({ block: 'nearest' });
      }
    }, 1000);
    try {
      const report = await invoke('wb_growth_run', { dryRun: false });
      await renderIdentityPool();
      await refreshStatus();

      const claimed = report?.total_claimed ?? 0;
      const credits = report?.total_credits ?? 0;
      const manual = (report?.accounts || [])
        .flatMap((a) => a.skipped || [])
        .filter((s) => s.hint);
      const stalled = (report?.accounts || []).reduce(
        (n, a) => n + (a.stalled?.length || 0),
        0,
      );

      // One consolidated message. Previously two `showWbStatus` calls ran back to
      // back and the second overwrote the first, so a successful run could end
      // up showing only "有 N 个任务需要你手动完成" and the earned credits were
      // never mentioned.
      const parts = [];
      if (claimed > 0) {
        parts.push(`领取 ${claimed} 个任务，获得 ${credits} 积分`);
      } else if (stalled === 0 && manual.length === 0) {
        // Nothing left to automate and nothing failed: this is the normal
        // "all claimed" state, not an error. Saying "没有可领取的积分" alone read
        // as a failure to users.
        parts.push('所有可自动化的任务都已领完');
      } else {
        parts.push('本次没有新领取的积分');
      }
      if (stalled > 0) parts.push(`${stalled} 个任务上报后未推进`);
      if (manual.length > 0) parts.push(`${manual.length} 个任务需你手动完成`);
      showWbStatus(`增长任务完成：${parts.join('；')}`);

      // A toast makes the outcome visible even if the status line is off-screen.
      // Only a real failure gets the error tone; "already all claimed" is a
      // success with zero delta.
      const allDone = claimed === 0 && stalled === 0;
      toast(
        claimed > 0
          ? `增长完成：+${credits} 积分（${claimed} 个任务）`
          : allDone
            ? '增长完成：可自动化的任务都已领完'
            : '增长已运行：本次没有新领取的积分',
        'ok',
      );
    } catch (e) {
      showWbStatus('增长任务失败: ' + e, true);
      toast('增长任务失败: ' + e, 'error');
    } finally {
      growthRunActive = false;
      clearInterval(heartbeat);
      // Drain once more before freezing the box: the run's last progress lines
      // are written just before the command resolves, and the poller may not
      // have ticked since. The freeze flag is set *after* this drain, so those
      // lines are not discarded.
      await drainProgress(true);
      // Keep polling for a short grace window so a line flushed just after the
      // command resolved still shows up — but stop after it, instead of asking
      // the backend forever.
      growthPollGraceUntil = Date.now() + 2000;
      growthRunFinished = true;
      paintGrowthProgress(false);
      if (btn) {
        btn.disabled = false;
        btn.textContent = '📈 增长积分';
      }
      // Leave the progress line visible briefly, then clear it so a later run
      // starts clean instead of appending to stale output.
      setTimeout(() => {
        const el = $('#wb-growth-progress');
        if (el && !growthRunActive) {
          el.textContent = growthProgressLines.join('\n');
        }
      }, 0);
    }
  });
}
