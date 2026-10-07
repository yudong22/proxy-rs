//! Background scheduler for the WorkBuddy daily check-in.
//!
//! One tokio task, spawned once at GUI start. It wakes on a 60-second tick and
//! claims the daily check-in when the local wall clock has reached the
//! configured `HH:MM` and that day's run has not happened yet.
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

use crate::workbuddy_auth;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// How often the scheduler wakes to test whether a run is due.
///
/// 60s bounds the worst-case lateness of a scheduled check-in to one minute,
/// which is far inside the tolerance of a daily task, and keeps the wakeups
/// cheap enough to be invisible.
const TICK_SECS: u64 = 60;

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
    logs: Arc<crate::settings::LogBuffer>,
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
            continue;
        }

        let now = crate::util::unix_millis();
        let today = workbuddy_auth::local_day(now);
        if prefs.last_checkin_run_date.as_deref() == Some(today.as_str()) {
            continue;
        }
        if !is_due(now, &prefs.daily_checkin_time) {
            continue;
        }

        // Mark the day *before* the run: a crash or a hang mid-claim then
        // costs one day's points rather than looping on every restart.
        let mut marked = prefs.clone();
        marked.last_checkin_run_date = Some(today.clone());
        if let Err(e) = workbuddy_auth::save_preferences(&marked) {
            logs.push("ERROR", format!("定时打卡无法写入运行记录: {}", e))
                .await;
            continue;
        }

        let report = workbuddy_auth::batch_claim_daily_checkin(&client).await;
        logs.push(
            "INFO",
            format!(
                "定时打卡完成: 共 {} 个账号，成功 {} 个，已打卡 {} 个，失败 {} 个",
                report.total, report.success, report.already_checked_in, report.failed
            ),
        )
        .await;
    }
}

/// How often the login-state scanner wakes.
///
/// Five minutes rather than the check-in tick's 60 s: this loop can perform a
/// network call per due account, and the thing it protects against (a token
/// silently aging out) is measured in hours. A cheap `load_credentials()` +
/// `needs_refresh()` comparison runs each tick; only accounts actually due cost
/// a request, so an idle app still makes none.
const SCAN_TICK_SECS: u64 = 300;

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
    logs: Arc<crate::settings::LogBuffer>,
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
            continue;
        }

        logs.push(
            "INFO",
            format!(
                "登录态巡检完成: 到期 {} 个，续期成功 {} 个，失败 {} 个，需重新登录 {} 个",
                report.total, report.refreshed, report.failed, report.needs_relogin
            ),
        )
        .await;

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
