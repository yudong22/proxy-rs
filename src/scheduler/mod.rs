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
//! ## File layout
//!
//! Split from one 975-line file. The three loops share one engine (claim a run,
//! take one pass, publish the outcome) and differ only in *what they do per
//! pass* — so the engine is one file and each loop is one file, which is the
//! direction a fourth loop would extend in.
//!
//! | file | owns |
//! |---|---|
//! | [`loops`] | loop ids, tick rates, cadence, specs |
//! | [`daily_checkin`] | the 每日打卡 loop |
//! | [`login_scan`] | the 登录态巡检 loop |
//! | [`growth_tasks`] | the 成长任务 loop |
//! | [`engine`] | the shared run engine: claim/guard, one pass, publish, due-ness |
//! | [`snapshot`] | the `GET /api/scheduler` payload |
//!
//! Adding a fourth loop means a new file next to the three, plus one row in
//! [`loops::LOOP_IDS`] — it should never mean editing the engine.

use crate::scheduler_state::{self, RunOutcome};
use crate::settings::LogBuffer;
use crate::workbuddy_auth;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

mod daily_checkin;
mod engine;
mod growth_tasks;
mod login_scan;
mod loops;
mod snapshot;

pub use daily_checkin::*;
pub use engine::*;
pub use growth_tasks::*;
pub use login_scan::*;
pub use loops::*;
pub use snapshot::*;

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
