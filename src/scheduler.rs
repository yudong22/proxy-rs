//! Background scheduler for the WorkBuddy account-maintenance loops, plus the
//! manual-trigger entry point the observation API exposes.
//!
//! Three `tokio` loops are spawned once at GUI start. Each wakes on its own
//! interval and claims its work when the local clock and its own gates allow it.
//!
//! Design notes:
//!
//! * **Local day, not UTC.** The daily allowance resets on the user's own
//!   calendar day, so both the "already ran today" guard and the day stamp use
//!   [`crate::workbuddy_auth::local_day`] with the machine's UTC offset.
//! * **Catch-up on start.** If today's slot has already passed and no run is
//!   recorded, the first tick claims immediately — otherwise launching the app
//!   at 10:00 for a 09:00 slot would silently skip the day.
//! * **Nothing else is scheduled.** The tick is a cheap date comparison; it
//!   does no I/O until a run is actually due, so an idle app pays no requests.
//! * **Every tick that decides something records why.** The gates used to
//!   `continue` silently, which made "switched off" and "already done" and
//!   "still waiting" indistinguishable from the outside. Each is now published
//!   through [`crate::scheduler_state`], which is what `/api/scheduler` reads.
//!
//! ## What this module is not
//!
//! It is not a general task scheduler and cannot be turned into one by adding a
//! config key. The three loops are hardcoded WorkBuddy account maintenance; there
//! is no task record, no queue, and no cron parser. Generic scheduled *agent*
//! work belongs to the harness that owns the agent Sessions, not here — see
//! `docs/scheduler-api.md`.

use crate::scheduler_state::{self, RunOutcome};
use crate::settings::LogBuffer;
use crate::workbuddy_auth;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Loop id published by `/api/scheduler` for the daily check-in.
pub const LOOP_CHECKIN: &str = "checkin";
/// Loop id published by `/api/scheduler` for the login-state scanner.
pub const LOOP_LOGIN_SCAN: &str = "login_scan";
/// Loop id published by `/api/scheduler` for the growth-task automation.
pub const LOOP_GROWTH: &str = "growth";

/// Every loop id, in display order.
///
/// One list because three places must agree on the vocabulary: the snapshot
/// payload, the manual-run route's accepted ids, and the plugin's tool schema. A
/// fourth hand-written copy is how `/api/scheduler/run/checkin` starts returning
/// 404 for a loop the snapshot still lists.
pub const LOOP_IDS: [&str; 3] = [LOOP_CHECKIN, LOOP_LOGIN_SCAN, LOOP_GROWTH];

/// How often the scheduler wakes to test whether a run is due.
///
/// 60s bounds the worst-case lateness of a scheduled check-in to one minute,
/// which is far inside the tolerance of a daily task, and keeps the wakeups
/// cheap enough to be invisible.
const TICK_SECS: u64 = 60;

/// How often the login-state scanner wakes.
///
/// Five minutes rather than the check-in tick's 60 s: this loop can perform a
/// network call per due account, and the thing it protects against (a token
/// silently aging out) is measured in hours. A cheap `load_credentials()` +
/// `needs_refresh()` comparison runs each tick; only accounts actually due cost
/// a request, so an idle app still makes none.
const SCAN_TICK_SECS: u64 = 300;

/// How often the growth-task scheduler wakes.
///
/// Growth tasks have no fixed wall-clock slot (unlike the check-in), but running
/// the same endpoints back-to-back for every account is exactly the pattern
/// upstream risk control watches for. One hour between passes keeps a full pool
/// sweep from ever looking like a burst, and is granular enough that the
/// 23:00–08:00 `black_cat` window is entered within minutes of opening.
const GROWTH_TICK_SECS: u64 = 3600;

/// What kind of clock one loop follows.
///
/// The distinction is what `/api/scheduler` reports as `next_due`, and it exists
/// because only one of the two can be answered honestly:
///
/// * [`Cadence::Daily`] has a wall-clock phase, so the next occurrence is an
///   exact local instant.
/// * [`Cadence::Every`] is aligned to the *process start* moment (the interval
///   anchor lives in the loop's own `tokio::time::interval`, which the HTTP side
///   cannot read). Reporting a countdown would be a fabricated precision, so the
///   payload says `within` and gives only the upper bound.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cadence {
    /// Once per local day at `HH:MM`.
    Daily,
    /// Every N seconds, anchored at process start.
    Every(u64),
}

/// Static description of one loop, as the observation API publishes it.
#[derive(Debug, Clone)]
pub struct LoopSpec {
    /// Stable id used in the API and in the manual-run route.
    pub id: &'static str,
    /// Human-facing name, matching the log lines and the GUI labels.
    pub title: &'static str,
    /// The loop's clock.
    pub cadence: Cadence,
    /// How often the loop wakes, in seconds.
    pub tick_secs: u64,
}

/// The three loops, in display order.
pub fn loop_specs() -> [LoopSpec; 3] {
    [
        LoopSpec {
            id: LOOP_CHECKIN,
            title: "每日打卡",
            cadence: Cadence::Daily,
            tick_secs: TICK_SECS,
        },
        LoopSpec {
            id: LOOP_LOGIN_SCAN,
            title: "登录态巡检",
            cadence: Cadence::Every(SCAN_TICK_SECS),
            tick_secs: SCAN_TICK_SECS,
        },
        LoopSpec {
            id: LOOP_GROWTH,
            title: "成长任务",
            cadence: Cadence::Every(GROWTH_TICK_SECS),
            tick_secs: GROWTH_TICK_SECS,
        },
    ]
}

/// Whether `loop_id` is a loop this build knows about.
pub fn is_known_loop(loop_id: &str) -> bool {
    LOOP_IDS.contains(&loop_id)
}

/// Run the daily check-in scheduler. Does not return until `shutdown` fires.
///
/// This is an `async fn` rather than a self-spawning one: it uses
/// `tokio::time::interval`, which needs an active Tokio reactor, and Tauri's
/// `.setup()` closure runs on the main thread with none — spawning from there
/// panicked ("there is no reactor running") and, because a panic cannot unwind
/// through the Objective-C launch callback, aborted the whole app at startup.
/// The caller must spawn this onto a runtime (`tauri::async_runtime::spawn`).
///
/// `client` is the shared GUI HTTP client; `logs` receives one line per run so
/// an unattended claim is visible in the 实时日志 tab.
pub async fn run_daily_checkin(
    client: reqwest::Client,
    logs: Arc<LogBuffer>,
    shutdown: CancellationToken,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(TICK_SECS));
    // The first tick resolves immediately, which is what performs the
    // catch-up run on start; `interval` is fine with that.
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => {}
        }

        let prefs = workbuddy_auth::load_preferences();
        if !prefs.daily_checkin_enabled {
            note_skip(
                LOOP_CHECKIN,
                "定时打卡未开启（在「账号池」设置里打开后生效）",
            );
            continue;
        }

        let now = crate::util::unix_millis();
        let today = workbuddy_auth::local_day(now);
        if prefs.last_checkin_run_date.as_deref() == Some(today.as_str()) {
            note_skip(LOOP_CHECKIN, "今日已打卡，等待明天");
            continue;
        }
        if !is_due(now, &prefs.daily_checkin_time) {
            note_skip(
                LOOP_CHECKIN,
                format!("未到 {}，等待中", prefs.daily_checkin_time),
            );
            continue;
        }

        // Mark the day *before* the run: a crash or a hang mid-claim then
        // costs one day's points rather than looping on every restart.
        let mut marked = prefs.clone();
        marked.last_checkin_run_date = Some(today.clone());
        if let Err(e) = workbuddy_auth::save_preferences(&marked) {
            logs.push("ERROR", format!("定时打卡无法写入运行记录: {}", e))
                .await;
            publish(
                LOOP_CHECKIN,
                RunOutcome::Failed,
                format!("无法写入运行记录: {e}"),
                false,
            );
            continue;
        }

        let report = workbuddy_auth::batch_claim_daily_checkin(&client).await;
        let summary = format!(
            "定时打卡完成: 共 {} 个账号，成功 {} 个，已打卡 {} 个，失败 {} 个",
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
            false,
        );
    }
}

/// Keep login states fresh in the background.
///
/// Renews any enabled credential whose token is due (known expiry inside the
/// margin, or the unknown-expiry backstop). Runs until `shutdown` fires.
///
/// Why this exists: without it, an account whose `expiresAt` is absent — which is
/// most imported login states — is only ever renewed *reactively*, after a 401
/// has already failed a user's request. A scheduled pass turns that into a
/// renewal that happens while the app is idle.
///
/// Like [`run_daily_checkin`], this is an `async fn` the caller must spawn onto a
/// runtime, because Tauri's `.setup()` closure has no Tokio reactor.
pub async fn run_login_state_scanner(
    client: reqwest::Client,
    logs: Arc<LogBuffer>,
    shutdown: CancellationToken,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(SCAN_TICK_SECS));
    // Skip the immediate first tick: startup already runs a proactive renewal on
    // the request path, and firing a scan before the app has settled would race
    // the first user request for the same credential.
    tick.tick().await;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => {}
        }

        let report = workbuddy_auth::refresh_due_credentials(&client).await;
        if report.total == 0 {
            // Still a completed pass: publishing it is what proves the scanner
            // is alive rather than stuck, which is the entire value of a
            // status surface for a loop whose normal output is silence.
            publish(
                LOOP_LOGIN_SCAN,
                RunOutcome::Idle,
                "无到期账号".to_string(),
                false,
            );
            continue;
        }

        let summary = format!(
            "登录态巡检完成: 到期 {} 个，续期成功 {} 个，失败 {} 个，需重新登录 {} 个",
            report.total, report.refreshed, report.failed, report.needs_relogin
        );
        logs.push("INFO", summary.clone()).await;
        publish(
            LOOP_LOGIN_SCAN,
            if report.failed > 0 || report.needs_relogin > 0 {
                RunOutcome::Failed
            } else {
                RunOutcome::Ok
            },
            summary,
            false,
        );

        for d in report.details.iter().filter(|d| d.needs_relogin) {
            logs.push(
                "WARN",
                format!(
                    "账号 {} ({}) 无法自动续期，需要重新登录: {}",
                    d.label, d.id, d.error
                ),
            )
            .await;
        }
    }
}

/// Growth-task automation: the optional third scheduler loop.
///
/// **Off unless the user turns it on** (`PoolPreferences::growth_enabled`). The
/// loop still ticks when disabled — the tick is a cheap preference read with no
/// I/O, so an idle app pays nothing, and flipping the switch takes effect within
/// an hour instead of requiring a restart.
///
/// Two independent passes share this loop:
///
/// * the **day pass** runs every runnable task once per local day;
/// * the **night pass** runs only `black_cat`, only inside 23:00–08:00, and is
///   tracked by its own date field.
///
/// Keeping them separate matters: sharing one "already ran today" stamp would
/// let the day pass consume the night pass's slot, so `black_cat` (which only
/// counts after 23:00) would never get a chance.
pub async fn run_growth_tasks(
    client: reqwest::Client,
    logs: Arc<LogBuffer>,
    shutdown: CancellationToken,
) {
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(GROWTH_TICK_SECS));
    // Skip the immediate first tick: the app has just started and the day's
    // catch-up (if any) can wait a moment for the pool and settings to settle.
    tick.tick().await;

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            _ = tick.tick() => {}
        }

        let prefs = workbuddy_auth::load_preferences();
        if !prefs.growth_enabled {
            note_skip(LOOP_GROWTH, "成长任务自动化未开启（默认关闭，需手动开启）");
            continue;
        }

        let today = workbuddy_auth::local_day(crate::util::unix_millis());
        let credentials = workbuddy_auth::load_credentials();
        if credentials.is_empty() {
            note_skip(LOOP_GROWTH, "尚未导入任何账号");
            continue;
        }

        // Night pass first: inside the window it is time-critical (it can only
        // run then), while the day pass is valid at any hour.
        let mut ran_anything = false;
        if crate::growth_runner::should_run_night(&prefs, &today) {
            let report =
                crate::growth_runner::run_pool(&client, &credentials, Some(&["black_cat"]), false)
                    .await;
            let summary = format!(
                "成长任务（夜间）: 共 {} 个账号，领取 {} 个任务，获得 {} 积分",
                report.total_accounts, report.total_claimed, report.total_credits
            );
            logs.push("INFO", summary.clone()).await;
            if report.total_accounts > 0 {
                ran_anything = true;
                // Only stamp the day when the pass actually ran; a skipped pool
                // must not consume the night slot.
                if let Err(e) = crate::growth_runner::mark_growth_run(&today, true) {
                    logs.push("ERROR", format!("成长任务夜间记录写入失败: {e}"))
                        .await;
                }
            }
            publish(LOOP_GROWTH, RunOutcome::Ok, summary, false);
        }

        if crate::growth_runner::should_run_day(&prefs, &today) {
            let report = crate::growth_runner::run_pool(&client, &credentials, None, false).await;
            let summary = crate::growth_runner::summarize(&report);
            logs.push("INFO", format!("成长任务: {}", summary)).await;
            // Surface the two outcomes a user must act on, one line each.
            for run in report.accounts.iter() {
                for s in run.skipped.iter().filter(|s| s.hint.is_some()) {
                    logs.push(
                        "INFO",
                        format!("成长任务需你手动完成 {}（{}）", s.code, s.reason),
                    )
                    .await;
                }
                for code in run.stalled.iter() {
                    logs.push(
                        "WARN",
                        format!("成长任务 {} 上报后进度未推进，上游可能已加校验", code),
                    )
                    .await;
                }
            }
            if report.total_accounts > 0 {
                // No `ran_anything = true` here: this branch ran, so the
                // `else if` below cannot be reached, and assigning a value
                // nothing reads is how a stale flag survives a refactor.
                if let Err(e) = crate::growth_runner::mark_growth_run(&today, false) {
                    logs.push("ERROR", format!("成长任务记录写入失败: {e}"))
                        .await;
                }
            }
            publish(LOOP_GROWTH, RunOutcome::Ok, summary, false);
        } else if !ran_anything {
            note_skip(LOOP_GROWTH, "今日批次已跑完，等待明天");
        }
    }
}

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
fn running_loops() -> &'static std::sync::Mutex<Vec<&'static str>> {
    static RUNNING: std::sync::OnceLock<std::sync::Mutex<Vec<&'static str>>> =
        std::sync::OnceLock::new();
    RUNNING.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

/// Claim the manual-run slot for `loop_id`, or report why it cannot be claimed.
fn claim_run(loop_id: &str) -> Result<RunGuard, RunRefusal> {
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
struct RunGuard {
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
async fn perform(id: &str, client: reqwest::Client, logs: Arc<LogBuffer>) {
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
fn publish(loop_id: &str, outcome: RunOutcome, summary: String, manual: bool) {
    let run = scheduler_state::run_now(outcome, summary, manual);
    let _ = scheduler_state::record(loop_id, run);
}

/// Publish a "the gate declined to run" record, collapsing identical repeats.
///
/// Separate from [`publish`] because a skip is a standing condition rather than
/// an event: a disabled loop would otherwise rewrite the state file every minute
/// for as long as the app runs.
fn note_skip(loop_id: &str, reason: impl Into<String>) {
    let reason = reason.into();
    let run = scheduler_state::run_now(RunOutcome::Skipped, reason, false);
    let _ = scheduler_state::record_dedup(loop_id, run);
}

/// Enabled credentials whose token is due for renewal right now.
fn count_due_credentials() -> usize {
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
fn is_due(now_ms: i64, time: &str) -> bool {
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

/// Build the read-only snapshot `/api/scheduler` serves.
///
/// Shaped here rather than in `api.rs` because the cadence constants and the
/// preference fields it reads are private to this module; exporting both just so
/// a shaper elsewhere could reassemble them would spread one contract over two
/// files. `api.rs` keeps only the extraction and status-code concerns.
pub fn snapshot() -> serde_json::Value {
    use serde_json::json;

    let prefs = workbuddy_auth::load_preferences();
    let now_ms = crate::util::unix_millis();
    let today = workbuddy_auth::local_day(now_ms);
    let state = scheduler_state::load();
    let credentials = workbuddy_auth::load_credentials();
    let enabled_accounts = credentials.iter().filter(|c| c.enabled).count();

    let loops: Vec<serde_json::Value> = loop_specs()
        .iter()
        .map(|spec| {
            let stored = state.loop_state(spec.id);
            let enabled = match spec.id {
                LOOP_CHECKIN => prefs.daily_checkin_enabled,
                LOOP_LOGIN_SCAN => true,
                LOOP_GROWTH => prefs.growth_enabled,
                _ => false,
            };

            let cadence = match spec.cadence {
                Cadence::Daily => json!({
                    "kind": "daily",
                    "at": prefs.daily_checkin_time,
                    "tick_secs": spec.tick_secs,
                }),
                Cadence::Every(secs) => json!({
                    "kind": "every",
                    "every_secs": secs,
                    "tick_secs": spec.tick_secs,
                }),
            };

            let next_due = if !enabled {
                // A disabled loop has no next occurrence. Saying "in N seconds"
                // here would be the most misleading possible answer.
                json!({ "kind": "disabled", "at": null, "in_secs": null })
            } else {
                match spec.cadence {
                    Cadence::Daily => {
                        let delta = workbuddy_auth::millis_until_daily_time(
                            now_ms,
                            &prefs.daily_checkin_time,
                        );
                        json!({
                            "kind": "at",
                            "at": render_local_minute(now_ms + delta),
                            "in_secs": delta / 1000,
                        })
                    }
                    Cadence::Every(secs) => json!({
                        "kind": "within",
                        "at": null,
                        "in_secs": secs,
                    }),
                }
            };

            let detail = match spec.id {
                LOOP_CHECKIN => json!({
                    "accounts": enabled_accounts,
                    "checked_in_today": credentials
                        .iter()
                        .filter(|c| c.enabled
                            && c.last_checkin_date.as_deref() == Some(today.as_str()))
                        .count(),
                }),
                LOOP_LOGIN_SCAN => json!({
                    "accounts": enabled_accounts,
                    "due_now": count_due_credentials(),
                }),
                LOOP_GROWTH => json!({
                    "day_pass_done_today": prefs.last_growth_run_date.as_deref()
                        == Some(today.as_str()),
                    "night_pass_done_today": prefs.last_night_growth_date.as_deref()
                        == Some(today.as_str()),
                    "night_window_open": crate::growth::in_night_window(local_hour(now_ms)),
                }),
                _ => json!({}),
            };

            json!({
                "id": spec.id,
                "title": spec.title,
                "enabled": enabled,
                "cadence": cadence,
                "last_run": run_json(stored.last_run.as_ref()),
                "last_skip": run_json(stored.last_skip.as_ref()),
                "next_due": next_due,
                "detail": detail,
            })
        })
        .collect();

    json!({
        "now": render_local_minute(now_ms),
        "now_ms": now_ms,
        "utc_offset_secs": crate::util::local_utc_offset_secs(),
        "process": "proxy-rs-gui",
        // Stated in the payload, not only in docs: a consumer that reads
        // `last_run` without this line will assume the timers survive a quit.
        "note": "这三个循环跑在 Proxy RS 桌面进程里：退出应用即停止，窗口关闭（驻留托盘）不影响；下次启动会补跑当天未完成的批次。",
        "loops": loops,
    })
}

/// One `last_run` / `last_skip` slot as JSON, or `null` when never recorded.
fn run_json(run: Option<&scheduler_state::LastRun>) -> serde_json::Value {
    match run {
        None => serde_json::Value::Null,
        Some(run) => serde_json::json!({
            "at": run.at,
            "at_ms": run.at_ms,
            "outcome": run.outcome.as_str(),
            "summary": run.summary,
            "manual": run.manual,
        }),
    }
}

/// Render epoch millis as a local `YYYY-MM-DD HH:MM` string.
///
/// Minute precision, not seconds: this is a *next occurrence* ("明天 09:00"),
/// and printing `09:00:00` invites a caller to treat it as an exact deadline
/// when the loop can fire up to one tick later.
fn render_local_minute(ms: i64) -> String {
    let secs = (ms + crate::util::local_utc_offset_secs() * 1_000).max(0) as u64 / 1_000;
    let rendered = crate::util::format_epoch_secs(secs);
    // `YYYY-MM-DD HH:MM:SS` → `YYYY-MM-DD HH:MM`
    rendered.get(..16).map(str::to_string).unwrap_or(rendered)
}

/// Hour of the day for an epoch-millis instant, in the machine's zone.
fn local_hour(now_ms: i64) -> u32 {
    let local_ms = now_ms + crate::util::local_utc_offset_secs() * 1_000;
    (local_ms.rem_euclid(86_400_000) / 3_600_000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run is due at or after the configured local time, and not before.
    #[test]
    fn due_only_after_the_configured_local_time() {
        // Build an epoch-ms value at a known local time-of-day.
        let at = |h: i64, m: i64| {
            let offset = crate::util::local_utc_offset_secs() * 1000;
            // Day 0 local midnight expressed as epoch ms.
            let base = (20_000i64 * 86_400_000) - offset;
            base + h * 3_600_000 + m * 60_000
        };

        assert!(!is_due(at(8, 59), "09:00"));
        assert!(is_due(at(9, 0), "09:00"));
        assert!(is_due(at(10, 30), "09:00"), "later the same day is due");
        assert!(is_due(at(0, 0), "00:00"));
    }

    /// An unparseable time falls back to 09:00 rather than being skipped.
    #[test]
    fn invalid_time_falls_back_to_the_default() {
        assert_eq!(workbuddy_auth::normalize_checkin_time("nonsense"), "09:00");
        assert_eq!(workbuddy_auth::normalize_checkin_time("25:00"), "09:00");
        assert_eq!(workbuddy_auth::normalize_checkin_time("9:5"), "09:05");
        assert_eq!(workbuddy_auth::normalize_checkin_time("09:00:30"), "09:00");
    }

    /// The published loop vocabulary is what the run route accepts.
    ///
    /// These two drifted apart once already in a sibling surface: a loop listed
    /// by the snapshot but rejected by the trigger is a 404 the user cannot
    /// explain. Pinning the list to `is_known_loop` makes any divergence a test
    /// failure rather than a support question.
    #[test]
    fn every_published_loop_id_is_triggerable() {
        assert_eq!(loop_specs().len(), LOOP_IDS.len());
        for spec in loop_specs() {
            assert!(
                is_known_loop(spec.id),
                "{} is published but not triggerable",
                spec.id
            );
            assert_eq!(
                LOOP_IDS.iter().filter(|id| **id == spec.id).count(),
                1,
                "{} appears twice in LOOP_IDS",
                spec.id
            );
        }
        assert!(!is_known_loop("nope"));
    }

    /// An unknown id and a loop with a run in flight refuse differently.
    #[test]
    fn claim_run_reports_the_right_refusal() {
        assert_eq!(claim_run("nope").err(), Some(RunRefusal::UnknownLoop));

        // Take the slot, then ask again while it is held.
        let guard = claim_run(LOOP_LOGIN_SCAN).expect("first claim");
        assert_eq!(
            claim_run(LOOP_LOGIN_SCAN).err(),
            Some(RunRefusal::AlreadyRunning)
        );
        // A different loop is unaffected: the guard is per loop, because these
        // loops touch unrelated upstream endpoints.
        let other = claim_run(LOOP_CHECKIN).expect("a different loop is free");
        drop(other);

        // Dropping the guard releases the slot — this is what stops a crashed
        // run from making the loop permanently un-triggerable.
        drop(guard);
        assert!(claim_run(LOOP_LOGIN_SCAN).is_ok());
    }

    /// The snapshot carries every loop with its contract keys present.
    ///
    /// Deliberately asserts key *presence* rather than values: the values depend
    /// on the developer's real preference file, but a missing key is a broken
    /// contract for every consumer regardless of what the user configured.
    #[test]
    fn snapshot_publishes_every_loop_with_its_keys() {
        let payload = snapshot();
        for key in [
            "now",
            "now_ms",
            "utc_offset_secs",
            "process",
            "note",
            "loops",
        ] {
            assert!(
                !payload[key].is_null(),
                "snapshot is missing {key}: {payload}"
            );
        }
        let loops = payload["loops"].as_array().expect("loops array");
        assert_eq!(loops.len(), LOOP_IDS.len());
        for (loop_json, spec) in loops.iter().zip(loop_specs()) {
            assert_eq!(loop_json["id"], spec.id);
            assert_eq!(loop_json["title"], spec.title);
            for key in [
                "enabled",
                "cadence",
                "last_run",
                "last_skip",
                "next_due",
                "detail",
            ] {
                assert!(
                    loop_json.get(key).is_some(),
                    "{} is missing {key}: {loop_json}",
                    spec.id
                );
            }
            // `last_run` is null until something runs; null is the honest
            // answer and must serialize as a key, not be omitted.
            assert!(loop_json["enabled"].is_boolean(), "{loop_json}");
        }
    }

    /// A disabled loop reports no next due rather than a fake countdown.
    #[test]
    fn a_disabled_loop_has_no_next_due() {
        let payload = snapshot();
        for loop_json in payload["loops"].as_array().expect("loops") {
            if loop_json["enabled"] == false {
                assert_eq!(
                    loop_json["next_due"]["kind"], "disabled",
                    "{} is off but claims a next due: {loop_json}",
                    loop_json["id"]
                );
                assert!(loop_json["next_due"]["in_secs"].is_null(), "{loop_json}");
            }
        }
    }

    /// The next local occurrence renders at minute precision.
    #[test]
    fn next_due_renders_minute_precision() {
        let rendered = render_local_minute(0);
        assert_eq!(rendered.len(), 16, "YYYY-MM-DD HH:MM: {rendered}");
        assert!(rendered.chars().nth(4) == Some('-'), "{rendered}");
        assert!(rendered.chars().nth(13) == Some(':'), "{rendered}");
    }

    /// The night-window hour helper follows the same local clock as the day.
    #[test]
    fn local_hour_reads_the_machine_zone() {
        let (_, ms) = scheduler_state::now_stamp();
        let hour = local_hour(ms);
        assert!(hour < 24, "{hour}");
        // Feed back a synthetic instant at local 23:00 and confirm it lands in
        // the night window the growth loop gates on.
        let offset = crate::util::local_utc_offset_secs() * 1_000;
        let midnight = (ms + offset).div_euclid(86_400_000) * 86_400_000 - offset;
        assert_eq!(local_hour(midnight + 23 * 3_600_000), 23);
        assert!(crate::growth::in_night_window(23));
        assert!(!crate::growth::in_night_window(12));
    }

    /// Regression: the scheduler must run inside a Tokio runtime.
    ///
    /// It was originally self-spawning with `tokio::spawn`, which panics with
    /// "there is no reactor running" when called from Tauri's `.setup()`
    /// closure (main thread, no reactor). Because a panic cannot unwind through
    /// the Objective-C launch callback, that aborted the app at every startup.
    /// Driving the future on a runtime here is what the caller now does.
    #[tokio::test]
    async fn runs_and_stops_inside_a_tokio_runtime() {
        let logs = std::sync::Arc::new(crate::settings::LogBuffer::new(10));
        let shutdown = CancellationToken::new();
        let cancel = shutdown.clone();

        // Drive the scheduler and cancel it from a background task. Reaching
        // completion proves the interval was created on a live reactor.
        let handle = tokio::spawn(run_daily_checkin(
            reqwest::Client::new(),
            logs.clone(),
            shutdown,
        ));
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            cancel.cancel();
        });

        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), handle).await;
        assert!(joined.is_ok(), "scheduler did not stop within 5s");
        assert!(joined.unwrap().is_ok(), "scheduler task panicked");
    }
}
