//! 成长任务 loop.

use super::*;

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
