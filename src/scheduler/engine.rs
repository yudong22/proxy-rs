//! The shared run engine: claim/guard, one pass, publish, due-ness.

use super::*;

/// Why a manual trigger could not start.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunRefusal {
    /// The id is not one of [`LOOP_IDS`].
    UnknownLoop,
    /// An earlier manual run of this loop is still in flight.
    AlreadyRunning,
}

impl RunRefusal {
    /// Stable machine-readable slug (`unknown_loop` / `already_running`).
    pub fn slug(self) -> &'static str {
        match self {
            RunRefusal::UnknownLoop => "unknown_loop",
            RunRefusal::AlreadyRunning => "already_running",
        }
    }
}

/// Manual-run guard: which loops currently have a manual run in flight.
///
/// A `std::sync::Mutex<Vec<&'static str>>` rather than a set of atomics because
/// the whole check-then-insert must be one critical section: two requests racing
/// on two atomics can both observe "not running" and both start, which for the
/// check-in means two concurrent claim sweeps over the same accounts.
pub(crate) fn running_loops() -> &'static std::sync::Mutex<Vec<&'static str>> {
    static RUNNING: std::sync::OnceLock<std::sync::Mutex<Vec<&'static str>>> =
        std::sync::OnceLock::new();
    RUNNING.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Claim the manual-run slot for `loop_id`, or report why it cannot be claimed.
pub(crate) fn claim_run(loop_id: &str) -> Result<RunGuard, RunRefusal> {
    let Some(id) = LOOP_IDS.iter().copied().find(|id| *id == loop_id) else {
        return Err(RunRefusal::UnknownLoop);
    };
    let mut running = running_loops()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if running.contains(&id) {
        return Err(RunRefusal::AlreadyRunning);
    }
    running.push(id);
    drop(running);
    Ok(RunGuard { id })
}

/// Releases the manual-run slot when the spawned run ends, including on panic.
///
/// A `Drop` guard rather than an explicit release at the end of the run: the run
/// performs network I/O that can panic inside a dependency, and a leaked slot
/// would make the loop permanently un-triggerable until the app restarts.
pub(crate) struct RunGuard {
    id: &'static str,
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        let mut running = running_loops()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        running.retain(|id| *id != self.id);
    }
}

/// Perform one loop's work immediately, without waiting for its clock.
///
/// Only one manual run per loop at a time; a second caller gets
/// [`RunRefusal::AlreadyRunning`] rather than joining the first, because the
/// caller's question ("did it start?") has a different answer.
///
/// The work itself is factored into [`perform`] so the guard's claim/release
/// stays in one place and cannot be skipped by an early return.
pub async fn run_once(
    loop_id: &str,
    client: reqwest::Client,
    logs: Arc<LogBuffer>,
) -> Result<(), RunRefusal> {
    let guard = claim_run(loop_id)?;
    perform(guard.id, client, logs).await;
    // `guard` drops here, releasing the slot on both the success and the panic
    // path.
    Ok(())
}

/// One loop's manual work, with the caller's live entry point for each.
///
/// Semantics, all of which differ from a scheduled tick on purpose:
///
/// * **`checkin` is forced.** It ignores the per-account "already claimed today"
///   short-circuit, matching the GUI's manual button: an explicit request is not
///   overridden by a local record that may be wrong after an upstream reset.
/// * **It never writes the once-a-day preference stamps.** This is the subtle
///   one. A manual run does real work; it must not convince the scheduler that
///   today's slot is consumed, or clicking the button once would silently cancel
///   the day's automatic run.
/// * **`growth` runs the day pass only.** The night pass is gated on a
///   23:00–08:00 window, and offering a manual entry for it would let a caller
///   manufacture a run the upstream's own rules reserve for that window.
///
/// `logs` receives the same one-line summary a scheduled run would publish, so a
/// manual run is visible in the console rather than only in this state file.
pub(crate) async fn perform(id: &str, client: reqwest::Client, logs: Arc<LogBuffer>) {
    match id {
        LOOP_CHECKIN => {
            let report = workbuddy_auth::batch_claim_daily_checkin_forced(&client).await;
            let summary = format!(
                "手动打卡完成: 共 {} 个账号，成功 {} 个，已打卡 {} 个，失败 {} 个",
                report.total, report.success, report.already_checked_in, report.failed
            );
            logs.push("INFO", summary.clone()).await;
            publish(
                LOOP_CHECKIN,
                if report.failed > 0 {
                    RunOutcome::Failed
                } else {
                    RunOutcome::Ok
                },
                summary,
                true,
            );
        }
        LOOP_LOGIN_SCAN => {
            let report = workbuddy_auth::refresh_due_credentials(&client).await;
            let summary = if report.total == 0 {
                "手动巡检: 无到期账号".to_string()
            } else {
                format!(
                    "手动巡检完成: 到期 {} 个，续期成功 {} 个，失败 {} 个，需重新登录 {} 个",
                    report.total, report.refreshed, report.failed, report.needs_relogin
                )
            };
            logs.push("INFO", summary.clone()).await;
            publish(
                LOOP_LOGIN_SCAN,
                if report.total == 0 {
                    RunOutcome::Idle
                } else if report.failed > 0 || report.needs_relogin > 0 {
                    RunOutcome::Failed
                } else {
                    RunOutcome::Ok
                },
                summary,
                true,
            );
        }
        LOOP_GROWTH => {
            let credentials = workbuddy_auth::load_credentials();
            if credentials.is_empty() {
                let summary = "手动成长任务: 尚未导入任何账号".to_string();
                logs.push("WARN", summary.clone()).await;
                publish(LOOP_GROWTH, RunOutcome::Idle, summary, true);
            } else {
                // `dry_run: false` — a manual trigger is an explicit request to
                // do the work, not a preview. The GUI's separate dry-run button
                // is what previews.
                let report =
                    crate::growth_runner::run_pool(&client, &credentials, None, false).await;
                let summary = format!("手动{}", crate::growth_runner::summarize(&report));
                logs.push("INFO", summary.clone()).await;
                publish(LOOP_GROWTH, RunOutcome::Ok, summary, true);
            }
        }
        _ => {}
    }
}

/// Write one run record, ignoring a state-file failure.
///
/// Telemetry must never break the work it observes: if the disk is full or the
/// data directory is read-only, the loop's actual result still stands and still
/// reached the log buffer. Returning the error here would either abort a run
/// that already happened or force every call site into the same `let _ =`
/// ceremony.
pub(crate) fn publish(loop_id: &str, outcome: RunOutcome, summary: String, manual: bool) {
    let run = scheduler_state::run_now(outcome, summary, manual);
    let _ = scheduler_state::record(loop_id, run);
}

/// Publish a "the gate declined to run" record, collapsing identical repeats.
///
/// Separate from [`publish`] because a skip is a standing condition rather than
/// an event: a disabled loop would otherwise rewrite the state file every minute
/// for as long as the app runs.
pub(crate) fn note_skip(loop_id: &str, reason: impl Into<String>) {
    let reason = reason.into();
    let run = scheduler_state::run_now(RunOutcome::Skipped, reason, false);
    let _ = scheduler_state::record_dedup(loop_id, run);
}

/// Enabled credentials whose token is due for renewal right now.
pub(crate) fn count_due_credentials() -> usize {
    let now = crate::util::unix_millis();
    workbuddy_auth::load_credentials()
        .into_iter()
        .filter(|c| c.enabled && c.needs_refresh(now))
        .count()
}

/// Whether the configured time has arrived on the local clock.
///
/// True once the wall clock is at or past `HH:MM` today (which covers the
/// catch-up case: a start after the slot is immediately due).
pub(crate) fn is_due(now_ms: i64, time: &str) -> bool {
    let time = workbuddy_auth::normalize_checkin_time(time);
    let (h, m) = time.split_once(':').unwrap_or(("9", "0"));
    let h: i64 = h.parse().unwrap_or(9);
    let m: i64 = m.parse().unwrap_or(0);
    let target_of_day = h * 3_600_000 + m * 60_000;

    let offset = crate::util::local_utc_offset_secs() * 1000;
    let local_ms = now_ms + offset;
    let into_day = local_ms.rem_euclid(86_400_000);
    into_day >= target_of_day
}

/// The process that currently hosts the three loops, for the snapshot payload.
///
/// Not a constant, because two binaries host these loops now: the desktop app
/// and the headless server ([`src/bin/proxy-rs-server.rs`]). Reporting
/// `proxy-rs-gui` from the server would be actively misleading — the `note`
/// beside it tells a consumer "quitting the app stops the timers", which is
/// true of the desktop build and false of a systemd unit. The executable's own
/// file name is the honest answer and needs no wiring at either call site.
pub(crate) fn hosting_process() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "proxy-rs".to_string())
}
