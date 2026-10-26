//! Durable "what did the background loops last actually do" state.
//!
//! The three scheduler loops ([`crate::scheduler`]) each kept their only trace in
//! a 1000-entry in-memory ring ([`crate::settings::LogBuffer`]). That ring is
//! seeded from `logs/proxy.log` and rotated by capacity, so the answer to
//! "did the 09:00 check-in run, and what happened?" decayed away within hours —
//! and on a quiet machine, a forced re-run and a scheduled run were
//! indistinguishable once both lines had scrolled out.
//!
//! This module is that missing fact, and only that fact.
//!
//! ## Two slots per loop, not one
//!
//! The obvious single "last run" field is wrong for these loops, because they
//! tick on a short interval and mostly *decline* to act: the check-in loop wakes
//! every 60 s and on almost every wake answers "not due yet". If a skip
//! overwrote the run record, the thing an operator most wants to see — when the
//! work last really happened and how it ended — would be replaced by a
//! timestamp proving only that the timer is alive.
//!
//! So each loop keeps two independent slots:
//!
//! * [`LoopState::last_run`] — the last time work was attempted (`Ok` / `Idle` /
//!   `Failed`). This is "上次运行".
//! * [`LoopState::last_skip`] — the last time a gate declined to run, with the
//!   reason (`Skipped`). This is "为什么现在没在跑".
//!
//! They answer different questions and neither can stand in for the other.
//!
//! ## What this module deliberately is not
//!
//! * **Not a scheduler.** Nothing here decides when anything runs. The loops
//!   still tick on their own `tokio::time::interval`; this file is written *by*
//!   them, never read by them to decide anything.
//! * **Not a queue or a job store.** No pending work, no retries, no history
//!   beyond these two slots.
//! * **Not the source of truth for "already ran today".** Those guards stay in
//!   [`crate::workbuddy_auth::PoolPreferences`] (`last_checkin_run_date`,
//!   `last_growth_run_date`), because they gate *behaviour*. Mixing an
//!   observability record into a behavioural gate is how a corrupted telemetry
//!   write starts silently skipping real work. It is also why a manual run —
//!   which does real work — never writes those preference fields.
//!
//! ## Durability
//!
//! Whole-file JSON at `~/.proxy-rs/scheduler-state.json`, replaced atomically
//! (temp file + rename) because three loops on different tasks write it. A torn
//! write would leave the file unparseable, and the *safe* degradation for a
//! status record is "no last run known" — never a fabricated timestamp — so
//! [`load`] maps any read or parse failure to the empty state.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

/// How one scheduled run ended.
///
/// `Idle` and `Skipped` exist because collapsing them into one "nothing
/// happened" bucket is exactly how a silent no-op starts looking like a healthy
/// run: `Idle` means the loop ran and found no work (no accounts due), while
/// `Skipped` means it never tried (the feature is off, or today's slot was
/// already consumed). An operator needs to tell those apart before deciding
/// whether to re-run.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    /// The run did work and reported a normal result.
    ///
    /// The `Default` variant exists only so [`LastRun`] can derive `Default`
    /// (used by tests and by `LoopState`); a real record is always constructed
    /// with an explicit outcome, so this is never a fallback a run can reach.
    #[default]
    Ok,
    /// The run happened but there was nothing due.
    Idle,
    /// The loop ticked but the enabled / once-a-day gate declined the run.
    Skipped,
    /// The run was attempted and failed.
    Failed,
}

impl RunOutcome {
    /// The wire spelling used by `/api/scheduler`.
    pub fn as_str(self) -> &'static str {
        match self {
            RunOutcome::Ok => "ok",
            RunOutcome::Idle => "idle",
            RunOutcome::Skipped => "skipped",
            RunOutcome::Failed => "failed",
        }
    }
}

/// One completed run.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LastRun {
    /// Local `YYYY-MM-DD HH:MM:SS` of the run.
    ///
    /// Stored as a rendered local string *and* the epoch value: this is
    /// human-facing telemetry, and re-deriving local time from a stored instant
    /// across a DST boundary produces a time the user never saw. `at_ms` exists
    /// so a caller can sort or diff without re-parsing.
    pub at: String,
    /// The same instant in epoch millis.
    #[serde(default)]
    pub at_ms: i64,
    pub outcome: RunOutcome,
    /// One line describing the result, in the same wording the log line uses.
    pub summary: String,
    /// True when the run was started by a manual trigger rather than the loop's
    /// own clock. Kept because a manual run must not be mistaken for evidence
    /// that the timer fired.
    #[serde(default)]
    pub manual: bool,
}

/// The two slots kept for one loop.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LoopState {
    /// Last attempt at real work (`Ok` / `Idle` / `Failed`).
    #[serde(default)]
    pub last_run: Option<LastRun>,
    /// Last time a gate declined, with its reason (`Skipped`).
    #[serde(default)]
    pub last_skip: Option<LastRun>,
}

/// Every loop's record, keyed by the loop id the API publishes
/// (`checkin`, `login_scan`, `growth`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SchedulerState {
    #[serde(default)]
    pub loops: BTreeMap<String, LoopState>,
}

impl SchedulerState {
    /// One loop's slots, or the empty slots when it has never recorded.
    pub fn loop_state(&self, loop_id: &str) -> LoopState {
        self.loops.get(loop_id).cloned().unwrap_or_default()
    }
}

/// State file path: `~/.proxy-rs/scheduler-state.json`.
pub fn state_path() -> PathBuf {
    crate::settings::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("scheduler-state.json")
}

/// The recorded state, or the empty state when nothing usable is stored.
///
/// A missing, unreadable or unparseable file is *not* an error: this is a status
/// record, and the honest answer to "what did it last do" when the file is gone
/// is "nothing is known". Failing here would take down the read endpoint on a
/// fresh install — the one moment it is most useful.
pub fn load() -> SchedulerState {
    load_from(&state_path())
}

/// [`load`] against an explicit path, so tests never touch the real data dir.
fn load_from(path: &std::path::Path) -> SchedulerState {
    let Ok(text) = std::fs::read_to_string(path) else {
        return SchedulerState::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist one loop's fresh record.
///
/// Read-modify-write under a process-wide mutex rather than a plain write: the
/// three loops live on separate tasks and finish at unrelated times, so two of
/// them can complete within the same tick window. Without the lock each would
/// load the state before the other's write landed and the later write would
/// silently drop the earlier loop's record — the exact "one update vanishes,
/// nobody notices" failure a status file must not have.
pub fn record(loop_id: &str, run: LastRun) -> anyhow::Result<()> {
    record_at(&state_path(), loop_id, run)
}

/// [`record`] against an explicit path, so tests never touch the real data dir.
fn record_at(path: &std::path::Path, loop_id: &str, run: LastRun) -> anyhow::Result<()> {
    let _guard = write_lock()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    let mut state = load_from(path);
    let slot = state.loops.entry(loop_id.to_string()).or_default();
    if run.outcome == RunOutcome::Skipped {
        slot.last_skip = Some(run);
    } else {
        slot.last_run = Some(run);
    }

    write_atomic(path, &state)
}

/// Rewrite the whole state document atomically.
fn write_atomic(path: &std::path::Path, state: &SchedulerState) -> anyhow::Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string_pretty(state)?;

    // Write beside the target then rename: rename is atomic on the same
    // filesystem, so a reader (or a crash) sees either the old file or the new
    // one, never a half-written document. A direct `fs::write` truncates first,
    // which would leave an empty file for the duration of the write.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    match std::fs::rename(&tmp, path) {
        Ok(()) => Ok(()),
        Err(e) => {
            // Clean up so a failed rename does not leave `.tmp` files piling up
            // in the data directory across every run.
            let _ = std::fs::remove_file(&tmp);
            Err(e.into())
        }
    }
}

/// Serializes every read-modify-write of the state file.
///
/// A `std::sync::Mutex` (not tokio's) because the critical section is pure
/// blocking file I/O with no `.await` in it; holding an async mutex across that
/// would needlessly occupy a runtime worker.
fn write_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

/// The last outcome+summary this process already published for `loop_id`.
///
/// A skip fires on *every tick* of a disabled loop — once a minute for the
/// check-in loop. Persisting each one would rewrite the state file 1440 times a
/// day for a feature the user has switched off, and would scrub the real "last
/// skip reason changed" signal from the file's mtime. Skips are therefore
/// published once per distinct reason; every other outcome is always published,
/// because those are events rather than a standing configuration.
fn last_published_skip() -> &'static std::sync::Mutex<BTreeMap<String, String>> {
    static LAST: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, String>>> =
        std::sync::OnceLock::new();
    LAST.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

/// Record a run, collapsing repeated identical skips.
///
/// Returns `true` when something was written (useful in tests to assert the
/// dedupe actually engages).
pub fn record_dedup(loop_id: &str, run: LastRun) -> anyhow::Result<bool> {
    if run.outcome == RunOutcome::Skipped {
        let mut last = last_published_skip()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if last.get(loop_id) == Some(&run.summary) {
            return Ok(false);
        }
        last.insert(loop_id.to_string(), run.summary.clone());
    }
    record(loop_id, run)?;
    Ok(true)
}

/// The local clock as both the rendered string and the epoch millis a
/// [`LastRun`] carries.
///
/// One helper because the two fields must describe the same instant; building
/// them at two call sites is how `at` and `at_ms` drift apart.
pub fn now_stamp() -> (String, i64) {
    let ms = crate::util::unix_millis();
    (render_local_ms(ms), ms)
}

/// Render epoch millis as a local `YYYY-MM-DD HH:MM:SS` string.
fn render_local_ms(ms: i64) -> String {
    let secs = (ms + crate::util::local_utc_offset_secs() * 1_000).max(0) as u64 / 1_000;
    crate::util::format_epoch_secs(secs)
}

/// Build a [`LastRun`] stamped at the current instant.
pub fn run_now(outcome: RunOutcome, summary: impl Into<String>, manual: bool) -> LastRun {
    let (at, at_ms) = now_stamp();
    LastRun {
        at,
        at_ms,
        outcome,
        summary: summary.into(),
        manual,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "proxy-rs-scheduler-state-{}-{}",
            std::process::id(),
            name
        ));
        std::fs::create_dir_all(&dir).expect("temp dir");
        dir.join("scheduler-state.json")
    }

    fn run(outcome: RunOutcome, summary: &str, manual: bool) -> LastRun {
        LastRun {
            at: "2026-10-26 09:00:12".to_string(),
            at_ms: 1_790_000_000_000,
            outcome,
            summary: summary.to_string(),
            manual,
        }
    }

    /// The whole point of this module: a loop's result survives a restart.
    #[test]
    fn a_recorded_run_round_trips_through_the_file() {
        let path = temp_path("roundtrip");
        let _ = std::fs::remove_file(&path);

        let r = run(RunOutcome::Ok, "定时打卡完成: 共 3 个账号", false);
        record_at(&path, "checkin", r.clone()).expect("record");

        let loaded = load_from(&path);
        assert_eq!(loaded.loop_state("checkin").last_run, Some(r));
        assert_eq!(loaded.loop_state("checkin").last_skip, None);
    }

    /// Two loops recording in sequence must not clobber each other.
    ///
    /// This is the failure a naive `fs::write` of a freshly-built state has: the
    /// second loop's write contains only its own record and the first one
    /// disappears. Asserting both keys catches it.
    #[test]
    fn recording_a_second_loop_keeps_the_first_one() {
        let path = temp_path("merge");
        let _ = std::fs::remove_file(&path);

        record_at(&path, "checkin", run(RunOutcome::Ok, "打卡完成", false)).expect("checkin");
        record_at(
            &path,
            "growth",
            run(RunOutcome::Skipped, "成长任务未开启", false),
        )
        .expect("growth");

        let loaded = load_from(&path);
        assert!(
            loaded.loop_state("checkin").last_run.is_some(),
            "{loaded:?}"
        );
        assert!(
            loaded.loop_state("growth").last_skip.is_some(),
            "{loaded:?}"
        );
    }

    /// A skip must not overwrite the last real run.
    ///
    /// This is the decisive property of the two-slot design: the check-in loop
    /// skips on ~every tick, so a single-slot record would answer "last run:
    /// skipped" forever and never show the 09:00 run that actually happened.
    #[test]
    fn a_skip_does_not_erase_the_last_real_run() {
        let path = temp_path("twoslot");
        let _ = std::fs::remove_file(&path);

        record_at(&path, "checkin", run(RunOutcome::Ok, "定时打卡完成", false)).expect("run");
        record_at(
            &path,
            "checkin",
            run(RunOutcome::Skipped, "今日已打卡", false),
        )
        .expect("skip");

        let state = load_from(&path).loop_state("checkin");
        assert_eq!(
            state.last_run.as_ref().map(|r| r.outcome),
            Some(RunOutcome::Ok),
            "the real run survives a later skip"
        );
        assert_eq!(
            state.last_skip.as_ref().map(|r| r.summary.as_str()),
            Some("今日已打卡")
        );
    }

    /// Re-recording one slot replaces it rather than appending.
    #[test]
    fn a_second_run_of_the_same_loop_replaces_the_first() {
        let path = temp_path("replace");
        let _ = std::fs::remove_file(&path);

        record_at(&path, "login_scan", run(RunOutcome::Ok, "旧", false)).expect("first");
        record_at(&path, "login_scan", run(RunOutcome::Failed, "新", true)).expect("second");

        let state = load_from(&path);
        assert_eq!(state.loops.len(), 1);
        let last = state.loop_state("login_scan").last_run.expect("record");
        assert_eq!(last.outcome, RunOutcome::Failed);
        assert!(last.manual, "the manual flag of the latest run wins");
    }

    /// A corrupt or missing file degrades to "nothing known", never an error.
    ///
    /// The alternative — surfacing a parse error — would blank the endpoint on a
    /// fresh install and on every interruption mid-write, which is precisely
    /// when an operator is looking at it.
    #[test]
    fn a_corrupt_file_reads_as_the_empty_state() {
        let path = temp_path("corrupt");
        std::fs::create_dir_all(path.parent().unwrap()).expect("dir");
        std::fs::write(&path, "{not json").expect("write junk");
        assert_eq!(load_from(&path), SchedulerState::default());

        let missing = temp_path("missing");
        let _ = std::fs::remove_file(&missing);
        assert_eq!(load_from(&missing), SchedulerState::default());
    }

    /// Repeated identical skips are collapsed; distinct ones are not.
    ///
    /// Without this a disabled check-in loop rewrites the state file every
    /// minute for as long as the app runs.
    #[test]
    fn identical_skips_are_recorded_once() {
        let path = temp_path("dedup");
        let _ = std::fs::remove_file(&path);
        let id = format!("dedup-{}", std::process::id());

        // Drive the same file through the dedupe helper by pre-seeding the
        // published map with a sentinel unique to this test.
        let mut last = last_published_skip().lock().expect("lock");
        last.remove(&id);
        drop(last);

        let first = record_dedup(&id, run(RunOutcome::Skipped, "未开启", false)).expect("first");
        let second = record_dedup(&id, run(RunOutcome::Skipped, "未开启", false)).expect("second");
        let changed =
            record_dedup(&id, run(RunOutcome::Skipped, "今天已跑", false)).expect("third");

        assert!(first, "the first skip is published");
        assert!(!second, "an identical skip is collapsed");
        assert!(changed, "a different skip reason is published");

        // An `Ok` run is never deduped, even with an identical summary: two runs
        // at different times are two events.
        assert!(record_dedup(&id, run(RunOutcome::Ok, "未开启", false)).expect("ok 1"));
        assert!(record_dedup(&id, run(RunOutcome::Ok, "未开启", false)).expect("ok 2"));
    }

    /// The rendered stamp and the epoch millis describe the same instant.
    #[test]
    fn now_stamp_renders_the_local_clock() {
        let (at, ms) = now_stamp();
        assert_eq!(at.len(), 19, "YYYY-MM-DD HH:MM:SS: {at}");
        assert!(at.chars().nth(4) == Some('-'), "{at}");
        assert_eq!(render_local_ms(ms), at);
    }

    /// Outcomes keep the wire spelling the API documents.
    #[test]
    fn outcomes_use_the_documented_wire_names() {
        assert_eq!(RunOutcome::Ok.as_str(), "ok");
        assert_eq!(RunOutcome::Idle.as_str(), "idle");
        assert_eq!(RunOutcome::Skipped.as_str(), "skipped");
        assert_eq!(RunOutcome::Failed.as_str(), "failed");
        // And serialize to that same spelling, so the file and the API agree
        // without a second mapping table.
        for outcome in [
            RunOutcome::Ok,
            RunOutcome::Idle,
            RunOutcome::Skipped,
            RunOutcome::Failed,
        ] {
            let wire = serde_json::to_string(&outcome).expect("serialize");
            assert_eq!(wire, format!("\"{}\"", outcome.as_str()));
        }
    }
}
