//! 每日打卡 loop.

use super::*;

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
