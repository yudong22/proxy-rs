//! The `GET /api/scheduler` snapshot payload.

use super::*;

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

    let process = hosting_process();
    let note = if process == "proxy-rs-server" {
        "这三个循环跑在 Proxy RS 服务器进程里：进程退出即停止，由 systemd 托管（Restart=always 会自动拉起）；下次启动会补跑当天未完成的批次。"
    } else {
        "这三个循环跑在 Proxy RS 桌面进程里：退出应用即停止，窗口关闭（驻留托盘）不影响；下次启动会补跑当天未完成的批次。"
    };

    json!({
        "now": render_local_minute(now_ms),
        "now_ms": now_ms,
        "utc_offset_secs": crate::util::local_utc_offset_secs(),
        "process": process,
        // Stated in the payload, not only in docs: a consumer that reads
        // `last_run` without this line will assume the timers survive a quit.
        // The wording has to match the *hosting* process, or a server deployment
        // tells its reader to go click a button that does not exist.
        "note": note,
        "loops": loops,
    })
}

/// One `last_run` / `last_skip` slot as JSON, or `null` when never recorded.
pub(crate) fn run_json(run: Option<&scheduler_state::LastRun>) -> serde_json::Value {
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
pub(crate) fn render_local_minute(ms: i64) -> String {
    let secs = (ms + crate::util::local_utc_offset_secs() * 1_000).max(0) as u64 / 1_000;
    let rendered = crate::util::format_epoch_secs(secs);
    // `YYYY-MM-DD HH:MM:SS` → `YYYY-MM-DD HH:MM`
    rendered.get(..16).map(str::to_string).unwrap_or(rendered)
}

/// Hour of the day for an epoch-millis instant, in the machine's zone.
pub(crate) fn local_hour(now_ms: i64) -> u32 {
    let local_ms = now_ms + crate::util::local_utc_offset_secs() * 1_000;
    (local_ms.rem_euclid(86_400_000) / 3_600_000) as u32
}
