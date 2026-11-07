//! The three loops' identity: ids, tick rates, cadence and specs.

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
pub(crate) const TICK_SECS: u64 = 60;

/// How often the login-state scanner wakes.
///
/// Five minutes rather than the check-in tick's 60 s: this loop can perform a
/// network call per due account, and the thing it protects against (a token
/// silently aging out) is measured in hours. A cheap `load_credentials()` +
/// `needs_refresh()` comparison runs each tick; only accounts actually due cost
/// a request, so an idle app still makes none.
pub(crate) const SCAN_TICK_SECS: u64 = 300;

/// How often the growth-task scheduler wakes.
///
/// Growth tasks have no fixed wall-clock slot (unlike the check-in), but running
/// the same endpoints back-to-back for every account is exactly the pattern
/// upstream risk control watches for. One hour between passes keeps a full pool
/// sweep from ever looking like a burst, and is granular enough that the
/// 23:00–08:00 `black_cat` window is entered within minutes of opening.
pub(crate) const GROWTH_TICK_SECS: u64 = 3600;

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
