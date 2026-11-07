//! 登录态巡检 loop.

use super::*;

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
