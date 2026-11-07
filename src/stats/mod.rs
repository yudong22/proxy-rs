//! Request-log persistence and the dashboard queries over it.
//!
//! ## File layout
//!
//! Split from one 3,400-line file into seven. What this split deliberately does
//! *not* do is invent a boundary: `StatsDb` is still one type over one table, and
//! its methods live in several `impl StatsDb` blocks across files — the
//! idiomatic way to keep one type's identity while giving each responsibility a
//! file of its own.
//!
//! | file | owns |
//! |---|---|
//! | [`record`] | the wire shapes (`TokenRecord`, `RequestOutcome`, `RequestLogItem`…) and row conversion |
//! | [`metrics`] | session metric folding and the day aggregate |
//! | [`connection`] | opening the database, schema/migrations, the read-connection pool |
//! | [`requests`] | writing the request log and querying it back |
//! | [`sessions`] | per-session queries, including the sub-agent tree fold |
//! | [`day`] | the per-day aggregate query |
//! | [`writer`] | the background thread that batches rows into one transaction |
//!
//! The 3.3.0 audit's conclusion is preserved: `stats` is **not** a god module and
//! must not be split by "reads vs writes" — that would produce two modules
//! fighting over the same connection pool. The split here is by *query subject*
//! inside the one cohesive store.

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::settings::data_dir;

// Shared by the row builders and the query paths that filter by local day.
fn local_date_string() -> String {
    let offset_secs = crate::util::local_utc_offset_secs();
    let local_secs = (now_secs() + offset_secs).max(0) as u64;
    let (y, m, d) = crate::util::civil_from_days((local_secs / 86400) as i64);
    format!("{:04}-{:02}-{:02}", y, m, d)
}

/// Return current local datetime as `YYYY-MM-DD HH:MM:SS` string, in the same
/// zone the log file uses.
fn local_datetime_string() -> String {
    let offset_secs = crate::util::local_utc_offset_secs();
    let local_secs = (now_secs() + offset_secs).max(0) as u64;
    // `format_epoch_secs` renders an epoch-seconds value; feeding it the
    // already-shifted local seconds yields local wall-clock fields.
    crate::util::format_epoch_secs(local_secs)
}

fn now_secs() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

mod connection;
mod day;
mod metrics;
mod record;
mod requests;
mod sessions;
mod writer;

// `writer` defines no public items — only the crate-internal thread entry point,
// which `connection` calls. It is imported rather than glob re-exported because
// a `pub use writer::*` would re-export nothing and warn.
pub(crate) use writer::writer_loop;

pub use connection::*;
pub use metrics::*;
pub use record::*;

#[cfg(test)]
mod tests {
    use super::*;

    fn outcome<'a>(
        model: &'a str,
        route: &'a str,
        tokens: &'a TokenRecord,
        status: u16,
        error: Option<&'a str>,
        streamed: bool,
    ) -> RequestOutcome<'a> {
        // In these unit tests the requested model equals the served model; the
        // virtual `free` resolution is exercised elsewhere. Defaulting keeps the
        // call sites terse.
        RequestOutcome {
            model,
            client_model: model,
            route,
            tokens,
            duration_ms: 1250,
            timing: UpstreamTiming::default(),
            streamed,
            status,
            error,
            session_id: "",
            parent_session_id: "",
            client: "",
            override_key: "",
            override_model: "",
            override_reason: "",
        }
    }

    /// `outcome` tagged with a session id (and optionally a parent session).
    ///
    /// A variant rather than more positional parameters: almost every caller
    /// cares about the session and nothing else, and a six-place predicate is
    /// how the wrong argument silently lands in the wrong field.
    fn session_outcome<'a>(
        session_id: &'a str,
        parent_session_id: &'a str,
        tokens: &'a TokenRecord,
        model_ms: i64,
    ) -> RequestOutcome<'a> {
        RequestOutcome {
            model: "hy3",
            client_model: "hy3",
            route: "/v1/chat/completions",
            tokens,
            duration_ms: 1000,
            timing: UpstreamTiming {
                ttft_ms: 100,
                model_ms,
                ended_with_tool_call: false,
            },
            streamed: true,
            status: 200,
            error: None,
            session_id,
            parent_session_id,
            client: "dsh",
            override_key: "",
            override_model: "",
            override_reason: "",
        }
    }

    /// The tool wait is measured on the client, not swallowed by this turn's own
    /// duration.
    ///
    /// Both stamps are row-*write* times (end of each turn), so the raw gap also
    /// contains the current turn's duration and must be backed out. Getting this
    /// wrong inflates every tool wait by the length of the following request.
    #[test]
    fn tool_wait_excludes_the_current_turns_own_duration() {
        // Prev turn ended at 10:00:00. This turn ended at 10:00:20 having taken
        // 5 s, so it began at 10:00:15 → the client spent 15 s running the tool.
        let wait = tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:20", 5_000);
        assert_eq!(wait, Some(15_000), "20s gap minus this turn's own 5s");

        // Same stamps, a turn that took the whole 20 s: the client did not wait.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:20", 20_000),
            Some(0)
        );

        // A gap shorter than the turn itself is a clock/estimation artifact, not
        // a negative tool wait: report nothing rather than a nonsense value.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:01", 30_000),
            None
        );

        // Resumed much later: not tool execution.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 12:00:00", 1_000),
            None
        );

        // Unparseable stamps yield nothing instead of a fabricated number.
        assert_eq!(tool_wait_ms("nonsense", "2026-09-28 10:00:20", 0), None);
        assert_eq!(tool_wait_ms("2026-09-28 10:00:00", "", 0), None);

        // Timestamps are whole seconds, so a sub-second turn inside the same
        // second backs out to a real (small) wait rather than a negative one.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:02", 1_500),
            Some(500)
        );
        // ...and a turn that fills its own second lands on exactly zero.
        assert_eq!(
            tool_wait_ms("2026-09-28 10:00:00", "2026-09-28 10:00:02", 2_000),
            Some(0)
        );
    }

    /// The timestamps in the DB are local wall-clock, and a plain subtraction of
    /// two of them is the elapsed time between them regardless of the UTC offset.
    #[test]
    fn local_datetime_parsing_is_monotonic_across_a_day_boundary() {
        let a = parse_local_datetime_secs("2026-09-28 23:59:59").unwrap();
        let b = parse_local_datetime_secs("2026-09-29 00:00:01").unwrap();
        assert_eq!(b - a, 2, "crossing midnight adds two seconds");

        // A leap day parses, so the day-count arithmetic is not off by one.
        let feb28 = parse_local_datetime_secs("2028-02-28 00:00:00").unwrap();
        let mar1 = parse_local_datetime_secs("2028-03-01 00:00:00").unwrap();
        assert_eq!(mar1 - feb28, 2 * 86_400, "2028 is a leap year");
    }

    #[test]
    fn upstream_timings_round_trip_and_yield_tps() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord {
            output: 600,
            ..Default::default()
        };
        let mut o = outcome("hy3", "/v1/messages", &tokens, 200, None, true);
        o.duration_ms = 9_000;
        o.timing = UpstreamTiming {
            ttft_ms: 320,
            model_ms: 3_000,
            ended_with_tool_call: true,
        };
        let _ = db.record_request_log(o);

        let res = db
            .query_request_logs(&RequestLogFilter {
                limit: Some(10),
                ..Default::default()
            })
            .unwrap();
        let item = &res.items[0];
        assert_eq!(item.ttft_ms, 320);
        assert_eq!(item.model_ms, 3_000);
        assert!(item.ended_with_tool_call);

        // 600 tokens over the 3 s model span, NOT over the 9 s request.
        let tps = UpstreamTiming {
            ttft_ms: item.ttft_ms,
            model_ms: item.model_ms,
            ended_with_tool_call: item.ended_with_tool_call,
        }
        .tps(item.output_tokens)
        .expect("measurable");
        assert!((tps - 200.0).abs() < 0.001, "got {tps}");

        // An unmeasured span yields no speed rather than a fabricated zero.
        assert_eq!(
            UpstreamTiming {
                ttft_ms: 0,
                model_ms: 0,
                ended_with_tool_call: false,
            }
            .tps(600),
            None
        );
        assert_eq!(
            UpstreamTiming {
                ttft_ms: 0,
                model_ms: 1_000,
                ended_with_tool_call: false,
            }
            .tps(0),
            None,
            "no output tokens means no speed"
        );
    }

    /// An older database gains the numeric timing columns instead of failing to
    /// open — and old rows read back as 0 ("unknown"), never as a real speed.
    #[test]
    fn migration_adds_the_timing_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL,
                model TEXT NOT NULL,
                route TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 60,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                streamed INTEGER NOT NULL DEFAULT 0,
                status INTEGER NOT NULL DEFAULT 0,
                error TEXT
             );
             INSERT INTO request_logs (created_at, model, route, output_tokens, status)
             VALUES ('2026-09-22 10:00:00', 'hy3', '/v1/messages', 60, 200);",
        )
        .unwrap();

        StatsDb::init_schema(&conn).unwrap();

        let db = StatsDb::from_conn(conn);
        let item = db
            .query_request_logs(&RequestLogFilter::default())
            .unwrap()
            .items
            .into_iter()
            .next()
            .expect("the legacy row survives the migration");
        assert_eq!(item.ttft_ms, 0, "legacy row has no measurable TTFT");
        assert_eq!(item.model_ms, 0, "legacy row has no measurable span");
        assert!(!item.ended_with_tool_call);
    }

    /// The session aggregate sums tokens and time, and attributes tool time
    /// across the window.
    ///
    /// This is the property that makes the card stable: two turns at very
    /// different speeds must combine into one time-weighted rate, not the
    /// latest turn's (jittery) figure.
    #[test]
    fn session_metrics_aggregate_tokens_over_time() {
        let db = StatsDb::in_memory().unwrap();

        // Turn 1: 500 tokens over 1 s (fast), ends 10:00:00, asks for a tool.
        insert_turn(
            &db,
            "claude:s1",
            "2026-09-28 10:00:00",
            3_000,
            200,
            1_000,
            500,
            true,
        );
        // Turn 2: 100 tokens over 3 s (slow), ends 10:00:20 having taken 5 s.
        insert_turn(
            &db,
            "claude:s1",
            "2026-09-28 10:00:20",
            5_000,
            400,
            3_000,
            100,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s1", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, 2);
        assert_eq!(m.measured_turns, 2);
        assert_eq!(m.output_tokens, 600, "tokens sum");
        assert_eq!(m.model_ms, 4_000, "generation time sums");

        // Time-weighted: 600 / 4 s = 150 tok/s. Averaging the two per-turn rates
        // would give (500 + 33.3)/2 ≈ 267 — wrong, because it would let a
        // 100-token reply count as much as a 500-token one.
        let tps = m.tps().expect("measurable");
        assert!((tps - 150.0).abs() < 0.001, "600 tokens / 4s, got {tps}");

        // Mean TTFT across both measured turns: (200 + 400) / 2.
        assert_eq!(m.avg_ttft_ms(), Some(300));

        // Turn 1 asked for a tool; the gap to turn 2's start is 20 s minus
        // turn 2's own 5 s = 15 s.
        assert_eq!(m.tool_wait_ms(), Some(15_000));
        assert_eq!(m.tool_waits, 1);

        // The panel's context fields describe the newest turn.
        assert_eq!(m.model.as_deref(), Some("hy3"));
        assert_eq!(m.last_at.as_deref(), Some("2026-09-28 10:00:20"));

        // An unknown session yields an empty result rather than an error.
        let empty = db.query_session_metrics_on("nobody", "2026-09-28").unwrap();
        assert_eq!(empty.turns, 0);
        assert_eq!(empty.tps(), None);
        assert_eq!(empty.avg_ttft_ms(), None);
        assert_eq!(empty.tool_wait_ms(), None);
        assert_eq!(
            db.query_session_metrics_on("", "2026-09-28").unwrap().turns,
            0
        );
    }

    /// Unmeasured turns do not drag the aggregate down.
    ///
    /// A legacy row or a plain non-streamed reply has `model_ms == 0`. Counting
    /// it would add zero time *and* whatever tokens it reports, wildly inflating
    /// the rate; it must be excluded from both sums while still being counted as
    /// a turn.
    #[test]
    fn session_metrics_exclude_unmeasured_turns_from_the_rate() {
        let db = StatsDb::in_memory().unwrap();

        // Measured: 400 tokens over 2 s.
        insert_turn(
            &db,
            "claude:s4",
            "2026-09-28 11:00:00",
            2_500,
            100,
            2_000,
            400,
            false,
        );
        // Unmeasured: no span at all, but a large token count that must not count.
        insert_turn(
            &db,
            "claude:s4",
            "2026-09-28 11:00:10",
            500,
            0,
            0,
            9_999,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s4", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, 2, "both rows are turns");
        assert_eq!(m.measured_turns, 1, "only one carried a span");
        assert_eq!(
            m.output_tokens, 400,
            "the unmeasured row's tokens are excluded"
        );
        assert_eq!(m.model_ms, 2_000);
        let tps = m.tps().expect("measurable");
        assert!((tps - 200.0).abs() < 0.001, "400 / 2s, got {tps}");
        // Its zero TTFT is not a sample either.
        assert_eq!(m.avg_ttft_ms(), Some(100));
    }

    /// A window with nothing measurable reports no speed at all, so the UI shows
    /// "—" instead of a fabricated number.
    #[test]
    fn session_metrics_report_nothing_when_unmeasured() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:s5",
            "2026-09-28 12:00:00",
            1_000,
            0,
            0,
            50,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s5", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, 1);
        assert_eq!(m.measured_turns, 0);
        assert_eq!(m.tps(), None);
        assert_eq!(m.avg_ttft_ms(), None);
        assert_eq!(m.tool_wait_ms(), None);
        // ...but the panel can still name the conversation.
        assert_eq!(m.model.as_deref(), Some("hy3"));
    }

    /// The window keeps the *recent* turns when a session exceeds the cap.
    ///
    /// `LIMIT` runs newest-first, so getting the order wrong would aggregate the
    /// oldest turns of a long conversation instead of its current behaviour.
    /// A parent's aggregate includes its subagent children's traffic.
    ///
    /// This is the whole point of lineage: before it, a conversation that
    /// delegated work to three subagents showed the parent's tokens only, and
    /// the panel listed each child as an unrelated column.
    #[test]
    fn a_parent_session_absorbs_its_subagent_children() {
        let db = StatsDb::in_memory().unwrap();
        let parent_tokens = TokenRecord {
            input: 1000,
            output: 200,
            ..Default::default()
        };
        let child_tokens = TokenRecord {
            input: 500,
            output: 300,
            ..Default::default()
        };
        let grandchild_tokens = TokenRecord {
            input: 10,
            output: 10,
            ..Default::default()
        };
        let mid_tokens = TokenRecord {
            input: 20,
            output: 40,
            ..Default::default()
        };

        db.record_request_log(session_outcome("dsh:p", "", &parent_tokens, 1000))
            .unwrap();
        // Two children…
        db.record_request_log(session_outcome("dsh:c1", "dsh:p", &child_tokens, 2000))
            .unwrap();
        db.record_request_log(session_outcome("dsh:c2", "dsh:p", &mid_tokens, 500))
            .unwrap();
        // …and a grandchild, whose parent is c1 rather than the root.
        db.record_request_log(session_outcome("dsh:g", "dsh:c1", &grandchild_tokens, 100))
            .unwrap();

        let metrics = db
            .query_session_metrics_on("dsh:p", &local_date_string())
            .unwrap();

        // Parent's own turn plus both direct children's; the grandchild is
        // folded into c1, which this row then absorbs. One level at a time is
        // what makes the totals add up exactly once instead of twice.
        assert_eq!(metrics.turns, 3, "parent + two direct children");
        assert_eq!(metrics.subagent_sessions, 2, "two direct children");
        assert!(!metrics.is_subagent, "the root conversation is not a child");
        assert_eq!(metrics.input_tokens, 1000 + 500 + 20);
        assert_eq!(metrics.output_tokens, 200 + 300 + 40);
        assert_eq!(metrics.model_ms, 1000 + 2000 + 500);

        // The rate is tokens over time across the whole tree, not an average of
        // three per-session rates.
        let tps = metrics.tps().expect("tree has a measurable span");
        let expected = (200.0 + 300.0 + 40.0) * 1000.0 / (1000.0 + 2000.0 + 500.0);
        assert!((tps - expected).abs() < 1e-9, "{tps} vs {expected}");

        // A child is still readable on its own, and says so.
        let child = db
            .query_session_metrics_on("dsh:c1", &local_date_string())
            .unwrap();
        assert!(child.is_subagent, "c1 was spawned by p");
        assert_eq!(child.turns, 2, "its own turn plus the grandchild's");
        assert_eq!(child.subagent_sessions, 1);
    }

    /// A subagent child never becomes a column of its own.
    ///
    /// The panel compares conversations; a child is part of one. If it kept its
    /// own column, the parent's column and the child's would double-count the
    /// same tokens in the same table.
    #[test]
    fn a_subagent_child_is_not_listed_as_its_own_recent_session() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord {
            input: 10,
            output: 5,
            ..Default::default()
        };
        db.record_request_log(session_outcome("dsh:root", "", &tokens, 100))
            .unwrap();
        db.record_request_log(session_outcome("dsh:kid", "dsh:root", &tokens, 100))
            .unwrap();
        db.record_request_log(session_outcome("dsh:other", "", &tokens, 100))
            .unwrap();

        let recent = db.query_recent_session_metrics(10).unwrap();
        let ids: Vec<&str> = recent.iter().map(|m| m.session_id.as_str()).collect();
        assert!(ids.contains(&"dsh:root"), "{ids:?}");
        assert!(ids.contains(&"dsh:other"), "{ids:?}");
        assert!(
            !ids.contains(&"dsh:kid"),
            "a subagent child must not get its own column: {ids:?}"
        );
        // Two top-level conversations, and the child's traffic lives in `root`.
        assert_eq!(recent.len(), 2, "{ids:?}");
    }

    /// Rows recorded before lineage existed stay top-level, and say so honestly.
    ///
    /// They cannot be backfilled, so a historical child keeps its own column.
    /// What must NOT happen is the aggregate claiming the conversation had no
    /// subagents: `subagent_sessions == 0` here means "none recorded", and the
    /// UI's wording depends on that distinction.
    #[test]
    fn a_pre_lineage_child_still_counts_as_a_top_level_conversation() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord {
            input: 7,
            output: 3,
            ..Default::default()
        };
        // An empty parent id is exactly what a pre-lineage row carries.
        db.record_request_log(session_outcome("dsh:legacy", "", &tokens, 100))
            .unwrap();

        let recent = db.query_recent_session_metrics(10).unwrap();
        assert_eq!(recent.len(), 1);
        assert_eq!(recent[0].session_id, "dsh:legacy");
        assert_eq!(recent[0].subagent_sessions, 0);
        assert!(!recent[0].is_subagent);
    }

    /// The speed card describes a conversation, never one of its subagents.
    ///
    /// A child's request is almost always the newest row, so picking "latest by
    /// id" without the lineage filter made the card show a helper's traffic
    /// under the parent's id — the panel was labelled with one conversation and
    /// measuring another, which no label on screen could reveal.
    #[test]
    fn the_latest_session_is_never_a_subagent_child() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord {
            input: 10,
            output: 5,
            ..Default::default()
        };
        db.record_request_log(session_outcome("dsh:parent", "", &tokens, 100))
            .unwrap();
        // Inserted last, so it holds the highest id.
        db.record_request_log(session_outcome("dsh:child", "dsh:parent", &tokens, 100))
            .unwrap();

        assert_eq!(
            db.latest_session_id_on(&local_date_string())
                .unwrap()
                .as_deref(),
            Some("dsh:parent"),
            "the card must describe the conversation, not its helper"
        );

        // And the card's figures then include the child, since they are the
        // conversation's cost.
        let metrics = db.query_latest_session_metrics().unwrap();
        assert_eq!(metrics.session_id, "dsh:parent");
        assert_eq!(metrics.turns, 2, "parent's own turn plus the child's");
        assert_eq!(metrics.subagent_sessions, 1);
    }

    /// The lineage column survives a write/read round trip.
    #[test]
    fn parent_session_id_round_trips_through_the_log() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord::default();
        db.record_request_log(session_outcome("dsh:kid", "dsh:parent", &tokens, 250))
            .unwrap();

        let res = db
            .query_request_logs(&RequestLogFilter {
                limit: Some(10),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(res.items[0].session_id, "dsh:kid");
        assert_eq!(res.items[0].parent_session_id, "dsh:parent");
    }

    #[test]
    fn session_metrics_window_keeps_the_recent_turns() {
        let db = StatsDb::in_memory().unwrap();

        // One ancient turn that must fall out of the window...
        insert_turn(
            &db,
            "claude:s6",
            "2026-09-28 09:00:00",
            1_000,
            50,
            1_000,
            1_000_000,
            false,
        );
        // ...then enough recent turns to exceed the cap.
        let extra = 5;
        for i in 0..(SESSION_METRICS_MAX_TURNS + extra) {
            let secs = i % 60;
            let mins = (i / 60) % 60;
            let created = format!("2026-09-28 13:{mins:02}:{secs:02}");
            insert_turn(&db, "claude:s6", &created, 2_000, 100, 1_000, 10, false);
        }

        let m = db
            .query_session_metrics_on("claude:s6", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, SESSION_METRICS_MAX_TURNS, "window is capped");
        // The huge ancient row would dominate the sum if it were included.
        assert_eq!(
            m.output_tokens,
            10 * SESSION_METRICS_MAX_TURNS,
            "only the recent turns are summed"
        );
        assert_eq!(m.model_ms, 1_000 * SESSION_METRICS_MAX_TURNS);
        let tps = m.tps().expect("measurable");
        assert!((tps - 10.0).abs() < 0.001, "10 tokens per 1s, got {tps}");
    }

    /// The recent-sessions list returns the most recently *active* conversations,
    /// newest first, and excludes unidentified requests.
    ///
    /// Ordering is by each session's latest activity, not by any single row's id:
    /// a long-running conversation must outrank one that merely started later.
    #[test]
    fn recent_sessions_are_ordered_by_latest_activity() {
        let db = StatsDb::in_memory().unwrap();

        // "old" starts first and keeps receiving turns, so its last row is the
        // newest overall — it must come first.
        insert_turn(
            &db,
            "claude:old",
            "2026-09-28 10:00:00",
            1_000,
            100,
            1_000,
            10,
            false,
        );
        insert_turn(
            &db,
            "claude:mid",
            "2026-09-28 10:00:05",
            1_000,
            100,
            1_000,
            10,
            false,
        );
        insert_turn(
            &db,
            "claude:old",
            "2026-09-28 10:00:10",
            1_000,
            100,
            1_000,
            10,
            false,
        );

        // An unidentified request must never appear: it is not a conversation.
        let no_tokens = TokenRecord::default();
        let mut anon = outcome("hy3", "/v1/messages", &no_tokens, 200, None, true);
        anon.session_id = "";
        let _ = db.record_request_log(anon);

        let recent = db.query_recent_session_metrics_on(3, "2026-09-28").unwrap();
        let ids: Vec<&str> = recent.iter().map(|m| m.session_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["claude:old", "claude:mid"],
            "newest activity first"
        );

        // `turns` counts only that session's rows.
        assert_eq!(recent[0].turns, 2, "old has two turns");
        assert_eq!(recent[1].turns, 1);

        // The limit is honoured, and 0 asks for nothing.
        let only_one = db.query_recent_session_metrics_on(1, "2026-09-28").unwrap();
        assert_eq!(only_one.len(), 1);
        assert_eq!(only_one[0].session_id, "claude:old");
        assert!(db
            .query_recent_session_metrics_on(0, "2026-09-28")
            .unwrap()
            .is_empty());

        // A database with no identified sessions yields an empty list, not an
        // error or a nameless row.
        let empty = StatsDb::in_memory().unwrap();
        assert!(empty
            .query_recent_session_metrics_on(3, "2026-09-28")
            .unwrap()
            .is_empty());
    }

    /// A session in the `recent` list reports exactly the same aggregate as the
    /// dedicated query — the panel compares them side by side, so a divergence
    /// would make the highlighted row disagree with the list.
    #[test]
    fn recent_sessions_agree_with_the_single_session_query() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:a",
            "2026-09-28 10:00:00",
            2_000,
            100,
            1_000,
            400,
            true,
        );
        insert_turn(
            &db,
            "claude:a",
            "2026-09-28 10:00:20",
            3_000,
            300,
            2_000,
            200,
            false,
        );

        let single = db
            .query_session_metrics_on("claude:a", "2026-09-28")
            .unwrap();
        let listed = db.query_recent_session_metrics_on(5, "2026-09-28").unwrap();
        let found = listed
            .iter()
            .find(|m| m.session_id == "claude:a")
            .expect("session is listed");

        // Aggregate fields must match; the fold reads the same window either way.
        assert_eq!(found.turns, single.turns);
        assert_eq!(found.measured_turns, single.measured_turns);
        assert_eq!(found.output_tokens, single.output_tokens);
        assert_eq!(found.model_ms, single.model_ms);
        assert_eq!(found.ttft_sum_ms, single.ttft_sum_ms);
        assert_eq!(found.ttft_samples, single.ttft_samples);
        assert_eq!(found.tool_wait_ms, single.tool_wait_ms);
        assert_eq!(found.tool_waits, single.tool_waits);
        assert_eq!(found.tps(), single.tps());
        assert_eq!(found.avg_ttft_ms(), single.avg_ttft_ms());
        assert_eq!(found.model, single.model);
        assert_eq!(found.last_at, single.last_at);
    }

    /// A turn not preceded by a tool request contributes no tool time —
    /// otherwise every ordinary turn would claim the previous turn's spacing.
    #[test]
    fn session_metrics_ignore_a_non_tool_predecessor() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:s2",
            "2026-09-28 10:00:00",
            1_000,
            100,
            500,
            100,
            false,
        );
        insert_turn(
            &db,
            "claude:s2",
            "2026-09-28 10:00:30",
            1_000,
            100,
            500,
            100,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s2", "2026-09-28")
            .unwrap();
        assert_eq!(m.turns, 2);
        assert_eq!(
            m.tool_wait_ms(),
            None,
            "a text-only predecessor is not a tool wait"
        );
        assert_eq!(m.tool_waits, 0);
    }

    /// Tool time accumulates across several tool round-trips in one window.
    #[test]
    fn session_metrics_sum_tool_time_across_turns() {
        let db = StatsDb::in_memory().unwrap();

        // Each pair: a tool-requesting turn, then its successor 10 s later that
        // itself took 2 s → a 8 s tool wait each time.
        insert_turn(
            &db,
            "claude:s7",
            "2026-09-28 10:00:00",
            2_000,
            100,
            500,
            10,
            true,
        );
        insert_turn(
            &db,
            "claude:s7",
            "2026-09-28 10:00:10",
            2_000,
            100,
            500,
            10,
            false,
        );
        insert_turn(
            &db,
            "claude:s7",
            "2026-09-28 10:00:20",
            2_000,
            100,
            500,
            10,
            true,
        );
        insert_turn(
            &db,
            "claude:s7",
            "2026-09-28 10:00:30",
            2_000,
            100,
            500,
            10,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:s7", "2026-09-28")
            .unwrap();
        assert_eq!(m.tool_waits, 2, "two tool round-trips");
        assert_eq!(m.tool_wait_ms(), Some(16_000), "two 8s waits summed");
    }

    /// Insert one row with an explicit `created_at`, so timing arithmetic can be
    /// tested without depending on the wall clock.
    #[allow(clippy::too_many_arguments)]
    fn insert_turn(
        db: &StatsDb,
        session: &str,
        created_at: &str,
        duration_ms: i64,
        ttft_ms: i64,
        model_ms: i64,
        output_tokens: i64,
        tool: bool,
    ) {
        let conn = db.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO request_logs (
                date, created_at, model, route, output_tokens, duration_ms,
                ttft_ms, model_ms, ended_with_tool_call, streamed, status,
                session_id, client
             ) VALUES (?1, ?2, 'hy3', '/v1/messages', ?3, ?4, ?5, ?6, ?7, 1, 200, ?8, 'claude')",
            params![
                &created_at[..10],
                created_at,
                output_tokens,
                duration_ms,
                ttft_ms,
                model_ms,
                if tool { 1 } else { 0 },
                session
            ],
        )
        .expect("insert test turn");
    }

    /// [`insert_turn`] with prompt-side token counts, for the cache-hit tests.
    ///
    /// Kept separate so the dozen existing call sites do not have to pass two
    /// more zeroes to express "this fixture never cared about prompt tokens".
    #[allow(clippy::too_many_arguments)]
    fn insert_turn_with_prompt(
        db: &StatsDb,
        session: &str,
        created_at: &str,
        input_tokens: i64,
        cache_read_tokens: i64,
        duration_ms: i64,
        ttft_ms: i64,
        model_ms: i64,
        output_tokens: i64,
    ) {
        let conn = db.conn.lock().unwrap_or_else(|p| p.into_inner());
        conn.execute(
            "INSERT INTO request_logs (
                date, created_at, model, route, input_tokens, cache_read_tokens,
                output_tokens, duration_ms, ttft_ms, model_ms,
                ended_with_tool_call, streamed, status, session_id, client
             ) VALUES (?1, ?2, 'hy3', '/v1/messages', ?3, ?4, ?5, ?6, ?7, ?8, 0, 1, 200, ?9, 'claude')",
            params![
                &created_at[..10],
                created_at,
                input_tokens,
                cache_read_tokens,
                output_tokens,
                duration_ms,
                ttft_ms,
                model_ms,
                session
            ],
        )
        .expect("insert test turn with prompt");
    }

    /// A session's figures are restricted to the given day.
    ///
    /// This is what makes the panel describe "today": turns from an earlier date
    /// must not reach the aggregate, even for the same session id.
    #[test]
    fn session_metrics_are_restricted_to_the_requested_day() {
        let db = StatsDb::in_memory().unwrap();

        // Yesterday: a huge, fast turn that would dominate any figure.
        insert_turn(
            &db,
            "claude:day",
            "2026-09-28 10:00:00",
            1_000,
            10,
            1_000,
            100_000,
            false,
        );
        // Today: two ordinary turns.
        insert_turn(
            &db,
            "claude:day",
            "2026-09-29 10:00:00",
            2_000,
            100,
            1_000,
            100,
            false,
        );
        insert_turn(
            &db,
            "claude:day",
            "2026-09-29 10:00:30",
            2_000,
            100,
            1_000,
            100,
            false,
        );

        let today = db
            .query_session_metrics_on("claude:day", "2026-09-29")
            .unwrap();
        assert_eq!(today.turns, 2, "yesterday's turn must be excluded");
        assert_eq!(today.output_tokens, 200);

        // The same session, read as of yesterday, sees only that day.
        let yesterday = db
            .query_session_metrics_on("claude:day", "2026-09-28")
            .unwrap();
        assert_eq!(yesterday.turns, 1);
        assert_eq!(yesterday.output_tokens, 100_000);

        // A day with no traffic is empty rather than an error.
        let empty = db
            .query_session_metrics_on("claude:day", "2026-09-27")
            .unwrap();
        assert_eq!(empty.turns, 0);
        assert_eq!(empty.tps(), None);
    }

    /// The day filter applies to the comparison columns too, so a session that
    /// ran only on an earlier day is not listed beside today's.
    #[test]
    fn recent_sessions_are_restricted_to_the_requested_day() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:old",
            "2026-09-28 10:00:00",
            1_000,
            10,
            1_000,
            500,
            false,
        );
        insert_turn(
            &db,
            "claude:new",
            "2026-09-29 10:00:00",
            1_000,
            10,
            1_000,
            500,
            false,
        );

        let today = db.query_recent_session_metrics_on(5, "2026-09-29").unwrap();
        assert_eq!(today.len(), 1, "yesterday-only session must not be listed");
        assert_eq!(today[0].session_id, "claude:new");

        let yesterday = db.query_recent_session_metrics_on(5, "2026-09-28").unwrap();
        assert_eq!(yesterday.len(), 1);
        assert_eq!(yesterday[0].session_id, "claude:old");
    }

    /// The card's session is today's last one, not the most recent ever.
    #[test]
    fn latest_session_id_ignores_an_earlier_day() {
        let db = StatsDb::in_memory().unwrap();
        // Inserted second, so it also has the higher row id — only the day
        // filter can keep it out.
        insert_turn(
            &db,
            "claude:today",
            "2026-09-29 09:00:00",
            1_000,
            10,
            1_000,
            100,
            false,
        );
        insert_turn(
            &db,
            "claude:yesterday",
            "2026-09-28 23:00:00",
            1_000,
            10,
            1_000,
            100,
            false,
        );

        assert_eq!(
            db.latest_session_id_on("2026-09-29").unwrap().as_deref(),
            Some("claude:today")
        );
        assert_eq!(
            db.latest_session_id_on("2026-09-28").unwrap().as_deref(),
            Some("claude:yesterday")
        );
        assert_eq!(db.latest_session_id_on("2026-09-27").unwrap(), None);
    }

    /// Request count and cache-hit rate over the window.
    ///
    /// The rate uses the same formula as the overview's daily figure
    /// (`cache_read / (input + cache_read)`), so a session's number and the
    /// day's number mean the same thing.
    #[test]
    fn session_metrics_report_request_count_and_cache_hit_rate() {
        let db = StatsDb::in_memory().unwrap();
        // 300 fresh input + 700 cached = 70% hit, over two requests.
        insert_turn_with_prompt(
            &db,
            "claude:cache",
            "2026-09-29 10:00:00",
            100,
            400,
            1_000,
            50,
            1_000,
            10,
        );
        insert_turn_with_prompt(
            &db,
            "claude:cache",
            "2026-09-29 10:00:10",
            200,
            300,
            1_000,
            50,
            1_000,
            10,
        );

        let m = db
            .query_session_metrics_on("claude:cache", "2026-09-29")
            .unwrap();
        assert_eq!(m.turns, 2, "requests = turns in the day window");
        assert_eq!(m.input_tokens, 300);
        assert_eq!(m.cache_read_tokens, 700);
        assert_eq!(m.cache_hit_pct(), Some(70));
    }

    /// An unmeasured turn still counts toward the token totals.
    ///
    /// These are quantities, not rates: a plain non-streamed reply contributes
    /// no generation time but its prompt tokens are real traffic, and dropping
    /// them would understate the session's input.
    #[test]
    fn prompt_tokens_count_even_on_an_unmeasured_turn() {
        let db = StatsDb::in_memory().unwrap();
        // model_ms = 0 => unmeasurable for the rate, but tokens are recorded.
        insert_turn_with_prompt(
            &db,
            "claude:u",
            "2026-09-29 10:00:00",
            100,
            900,
            500,
            0,
            0,
            50,
        );

        let m = db
            .query_session_metrics_on("claude:u", "2026-09-29")
            .unwrap();
        assert_eq!(m.turns, 1);
        assert_eq!(m.measured_turns, 0);
        assert_eq!(m.tps(), None, "no generation span, so no rate");
        assert_eq!(m.input_tokens, 100);
        assert_eq!(m.cache_read_tokens, 900);
        assert_eq!(m.cache_hit_pct(), Some(90));
    }

    /// No prompt traffic means no hit rate — `None`, not 0%.
    ///
    /// A session with requests but no token accounting must not claim it cached
    /// nothing; the panel renders `None` as `—`.
    #[test]
    fn cache_hit_is_none_rather_than_zero_without_prompt_traffic() {
        let db = StatsDb::in_memory().unwrap();
        insert_turn(
            &db,
            "claude:notokens",
            "2026-09-29 10:00:00",
            1_000,
            50,
            1_000,
            10,
            false,
        );

        let m = db
            .query_session_metrics_on("claude:notokens", "2026-09-29")
            .unwrap();
        assert_eq!(m.turns, 1);
        assert_eq!(m.cache_hit_pct(), None);
        // A defaulted aggregate reports nothing at all.
        assert_eq!(SessionMetrics::default().cache_hit_pct(), None);
    }

    /// The defaulted metrics report nothing rather than a misleading zero.
    #[test]
    fn default_session_metrics_have_no_cache_or_turns() {
        let m = SessionMetrics::default();
        assert_eq!(m.turns, 0);
        assert_eq!(m.cache_hit_pct(), None);
        assert_eq!(m.input_tokens, 0);
        assert_eq!(m.cache_read_tokens, 0);
    }

    /// A turn written through the real logging path is found by "today".
    ///
    /// The other session-metric tests insert rows directly with an explicit
    /// date, which cannot catch the failure this one guards: `record_request_log`
    /// stamping a `date` the day filter does not match (wrong format, wrong
    /// timezone, empty). That would make the whole panel silently show "—".
    #[test]
    fn a_turn_recorded_now_is_visible_to_the_today_filter() {
        let db = StatsDb::in_memory().unwrap();

        let tokens = TokenRecord {
            input: 40,
            cache_read: 60,
            cache_write: 0,
            output: 25,
        };
        let mut out = outcome(
            "claude-3-5-sonnet-20241022",
            "/v1/messages",
            &tokens,
            200,
            None,
            true,
        );
        out.session_id = "claude:live";
        out.client = "claude";
        db.record_request_log(out).unwrap();

        // `query_session_metrics` (no explicit date) is exactly what the GUI
        // command calls, so this covers the production path end to end.
        let m = db.query_session_metrics("claude:live").unwrap();
        assert_eq!(m.turns, 1, "the row must fall inside today's window");
        assert_eq!(m.input_tokens, 40);
        assert_eq!(m.cache_read_tokens, 60);
        assert_eq!(m.cache_hit_pct(), Some(60));

        // And the card's session is found the same way.
        assert_eq!(
            db.latest_session_id().unwrap().as_deref(),
            Some("claude:live")
        );
        assert_eq!(db.query_recent_session_metrics(3).unwrap().len(), 1);
    }

    #[test]
    fn daily_stats_are_derived_from_request_logs() {
        let db = StatsDb::in_memory().unwrap();

        let ok = TokenRecord {
            input: 100,
            cache_read: 900,
            cache_write: 0,
            output: 50,
        };
        db.record_request_log(outcome(
            "claude-3-5-sonnet-20241022",
            "/v1/messages",
            &ok,
            200,
            None,
            true,
        ))
        .unwrap();

        let failed = TokenRecord::default();
        db.record_request_log(outcome(
            "gpt-4o",
            "/v1/chat/completions",
            &failed,
            502,
            Some("Upstream gateway error"),
            false,
        ))
        .unwrap();

        let today = db.query_today().unwrap();
        assert_eq!(today.requests_total, 2);
        assert_eq!(today.requests_success, 1);
        assert_eq!(today.requests_failed, 1);
        assert_eq!(today.tokens_input, 100);
        assert_eq!(today.tokens_cache_read, 900);
        assert_eq!(today.tokens_output, 50);
        assert_eq!(today.cache_hit_pct(), 90); // 900/(100+900)*100
        assert_eq!(today.tokens_total(), 1050);
        assert!(!today.date.is_empty());
    }

    /// Only rows whose `override_key` is actually set count as overrides.
    ///
    /// A request served exactly as configured must not inflate the count: the
    /// overview's 今日 override 次数 card is read as "how often did the proxy
    /// have to deviate today", and counting clean requests would make that
    /// number meaningless.
    #[test]
    fn override_count_only_counts_rows_with_an_override_key() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord::default();

        let mut overridden = outcome("m", "/v1/messages", &tokens, 200, None, true);
        overridden.override_key = "k-3ba06c";
        overridden.override_model = "hy4-preview";
        overridden.override_reason = "401 from wb-5735d0 (降级密钥池)";
        db.record_request_log(overridden).unwrap();

        db.record_request_log(outcome("m", "/v1/messages", &tokens, 200, None, true))
            .unwrap();

        // A failed request can still be an override — the failover did happen,
        // the request just did not succeed afterwards.
        let mut failed_override = outcome("m", "/v1/messages", &tokens, 502, Some("boom"), true);
        failed_override.override_key = "wb-a85801";
        db.record_request_log(failed_override).unwrap();

        let today = db.query_today().unwrap();
        assert_eq!(today.requests_total, 3);
        assert_eq!(today.overrides_total, 2, "clean rows must not count");

        db.clear_request_logs().unwrap();
        assert_eq!(db.query_today().unwrap().overrides_total, 0);
    }

    #[test]
    fn request_logs_crud_and_filtering() {
        let db = StatsDb::in_memory().unwrap();

        // 1. Record success log
        let ok_tokens = TokenRecord {
            input: 150,
            cache_read: 50,
            cache_write: 0,
            output: 200,
        };
        db.record_request_log(outcome(
            "claude-3-5-sonnet-20241022",
            "/v1/messages",
            &ok_tokens,
            200,
            None,
            true,
        ))
        .unwrap();

        // 2. Record failure log
        let err_tokens = TokenRecord::default();
        db.record_request_log(outcome(
            "gpt-4o",
            "/v1/chat/completions",
            &err_tokens,
            502,
            Some("Upstream gateway error"),
            false,
        ))
        .unwrap();

        // Query all
        let all = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert_eq!(all.total, 2);
        assert_eq!(all.items.len(), 2);
        assert_eq!(all.models.len(), 2);
        assert_eq!(all.items[0].model, "gpt-4o"); // Most recent first (id DESC)
        assert_eq!(all.items[0].status, 502);
        assert!(!all.items[0].streamed);
        assert_eq!(all.items[1].model, "claude-3-5-sonnet-20241022");
        assert_eq!(all.items[1].status, 200);
        assert!(all.items[1].streamed);
        assert_eq!(all.items[1].input_tokens, 150);

        // Filter by model
        let claude_only = db
            .query_request_logs(&RequestLogFilter {
                model: Some("claude-3-5-sonnet-20241022".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(claude_only.total, 1);
        assert_eq!(claude_only.items[0].model, "claude-3-5-sonnet-20241022");

        // Filter by status success (2xx)
        let success_only = db
            .query_request_logs(&RequestLogFilter {
                status_group: Some("2xx".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(success_only.total, 1);
        assert_eq!(success_only.items[0].status, 200);

        // Filter by status error
        let error_only = db
            .query_request_logs(&RequestLogFilter {
                status_group: Some("error".to_string()),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(error_only.total, 1);
        assert_eq!(error_only.items[0].status, 502);

        // Filter by streamed
        let stream_only = db
            .query_request_logs(&RequestLogFilter {
                streamed: Some(true),
                ..Default::default()
            })
            .unwrap();
        assert_eq!(stream_only.total, 1);
        assert!(stream_only.items[0].streamed);

        // Clear logs
        db.clear_request_logs().unwrap();
        let empty = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert_eq!(empty.total, 0);
        assert_eq!(empty.items.len(), 0);
    }

    #[test]
    fn daily_stats_reflect_cleared_logs() {
        // The two sources can no longer disagree: clearing the log zeroes the
        // counters, because the counters are computed from the log itself.
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord {
            input: 10,
            cache_read: 0,
            cache_write: 0,
            output: 5,
        };
        db.record_request_log(outcome("m", "/v1/messages", &tokens, 200, None, false))
            .unwrap();
        assert_eq!(db.query_today().unwrap().requests_total, 1);

        db.clear_request_logs().unwrap();
        assert_eq!(db.query_today().unwrap().requests_total, 0);
    }

    #[test]
    fn clearing_a_file_backed_database_actually_removes_the_rows() {
        // Regression: `clear_request_logs` now borrows through the read pool, so
        // it must still delete from the same database the writer thread fills.
        // The in-memory test cannot cover this — it shares a single connection.
        let dir = std::env::temp_dir().join(format!("proxy-rs-clear-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.db");
        let db = StatsDb::open_at(&path).unwrap();

        for _ in 0..5 {
            db.record_request_log(outcome(
                "m",
                "/v1/messages",
                &TokenRecord::default(),
                200,
                None,
                true,
            ))
            .unwrap();
        }

        // Wait for the writer thread to commit before clearing.
        let mut total = 0;
        for _ in 0..200 {
            total = db.query_today().unwrap().requests_total;
            if total == 5 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(total, 5, "rows should be committed before the clear");

        db.clear_request_logs().unwrap();

        assert_eq!(
            db.query_today().unwrap().requests_total,
            0,
            "the delete must be visible to a subsequent read"
        );
        let listed = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert_eq!(listed.total, 0, "and to the request-log listing");
        assert!(listed.items.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn migration_drops_legacy_daily_stats_and_backfills_dates() {
        // Simulate a database written by the previous schema: no `date` column,
        // plus the now-redundant `daily_stats` table.
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE daily_stats (
                date TEXT PRIMARY KEY,
                requests_total INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL,
                model TEXT NOT NULL,
                route TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                streamed INTEGER NOT NULL DEFAULT 0,
                status INTEGER NOT NULL DEFAULT 0,
                error TEXT
             );
             INSERT INTO request_logs (created_at, model, route, status)
             VALUES ('2026-09-22 10:00:00', 'hy3', '/v1/responses', 200);",
        )
        .unwrap();

        StatsDb::init_schema(&conn).unwrap();

        let db = StatsDb::from_conn(conn);
        let stats = db.query_date("2026-09-22").unwrap();
        assert_eq!(stats.requests_total, 1, "date backfilled from created_at");
        assert_eq!(stats.requests_success, 1);
    }

    /// The override trio round-trips through the DB, so the detail view can
    /// explain *why* a non-default credential served a request.
    #[test]
    fn override_fields_round_trip_through_the_database() {
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord::default();
        let mut o = outcome("hy3", "/v1/responses", &tokens, 200, None, true);
        o.override_key = "wb-b1";
        o.override_model = "glm-5.3";
        o.override_reason = "429 from wb-a1; content_blocked 400";
        let _ = db.record_request_log(o);

        let res = db
            .query_request_logs(&RequestLogFilter {
                limit: Some(10),
                ..Default::default()
            })
            .unwrap();
        let item = &res.items[0];
        assert_eq!(item.override_key, "wb-b1");
        assert_eq!(item.override_model, "glm-5.3");
        assert_eq!(item.override_reason, "429 from wb-a1; content_blocked 400");
    }

    /// An older database gains the `override_reason` column (defaulting to the
    /// empty string) instead of failing to open.
    #[test]
    fn migration_adds_the_override_reason_column() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE request_logs (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                created_at TEXT NOT NULL,
                model TEXT NOT NULL,
                route TEXT NOT NULL,
                input_tokens INTEGER NOT NULL DEFAULT 0,
                output_tokens INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens INTEGER NOT NULL DEFAULT 0,
                duration_ms INTEGER NOT NULL DEFAULT 0,
                streamed INTEGER NOT NULL DEFAULT 0,
                status INTEGER NOT NULL DEFAULT 0,
                error TEXT,
                session_id TEXT NOT NULL DEFAULT '',
                client TEXT NOT NULL DEFAULT '',
                override_key TEXT NOT NULL DEFAULT '',
                override_model TEXT NOT NULL DEFAULT ''
             );",
        )
        .unwrap();

        StatsDb::init_schema(&conn).unwrap();

        // The new column exists and old rows read back as an empty reason.
        let db = StatsDb::from_conn(conn);
        let res = db.query_request_logs(&RequestLogFilter::default()).unwrap();
        assert!(res.items.is_empty());
    }

    #[test]
    fn sessions_are_listed_db_wide_not_just_from_the_current_page() {
        // The dropdown must offer conversations that are not on the visible
        // page, otherwise an older chat cannot be selected at all.
        let db = StatsDb::in_memory().unwrap();
        let tokens = TokenRecord::default();

        let mut outcomes = Vec::new();
        for (session, client) in [
            ("claude:aaa", "claude"),
            ("claude:aaa", "claude"), // a second turn of the same chat
            ("codex:bbb", "codex"),
            ("", "unknown"), // unidentified: must not appear as a session
        ] {
            let mut o = outcome("hy3", "/v1/messages", &tokens, 200, None, true);
            o.session_id = session;
            o.client = client;
            outcomes.push(o);
        }
        for o in &outcomes {
            db.record_request_log(o.clone()).unwrap();
        }

        // Ask for a single row: the session list must still be complete.
        let res = db
            .query_request_logs(&RequestLogFilter {
                limit: Some(1),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(res.items.len(), 1, "page is limited to one row");
        let ids: Vec<&str> = res.sessions.iter().map(|s| s.session_id.as_str()).collect();
        assert_eq!(ids, vec!["codex:bbb", "claude:aaa"], "newest first");
        assert!(
            !ids.iter().any(|id| id.is_empty()),
            "an unidentified request is not a session"
        );
        assert_eq!(
            res.sessions
                .iter()
                .find(|s| s.session_id == "claude:aaa")
                .unwrap()
                .client,
            "claude"
        );
        // The duplicate turn collapses into one entry.
        assert_eq!(res.sessions.len(), 2);
    }

    #[test]
    fn concurrent_readers_do_not_share_one_connection() {
        // The point of the pool: the GUI polls the stats line and the request
        // table every two seconds, and one shared lock made those queue behind
        // each other. Two readers held at once must therefore use *different*
        // connections.
        let dir = std::env::temp_dir().join(format!("proxy-rs-pool-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.db");
        let db = StatsDb::open_at(&path).unwrap();

        let a = db.read_conn();
        let b = db.read_conn();
        // Pointer identity is the assertion that matters: distinct connections,
        // not merely two guards over the same one.
        assert_ne!(
            a.get() as *const Connection,
            b.get() as *const Connection,
            "concurrent readers must not serialize on one connection"
        );

        drop((a, b));

        // Returned connections are kept for reuse. The pool settles at its
        // high-water mark (2 here, because the second read found an empty pool
        // and opened one) rather than growing with each query.
        for _ in 0..10 {
            let _ = db.read_conn();
        }
        assert_eq!(
            db.readers.lock().unwrap().len(),
            2,
            "the pool must plateau, not grow per query"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_pooled_read_sees_rows_written_by_the_writer_thread() {
        // A pool is only useful if its connections observe the committed data:
        // separate connections to the same file, not snapshots.
        let dir = std::env::temp_dir().join(format!("proxy-rs-poolvis-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.db");
        let db = StatsDb::open_at(&path).unwrap();

        db.record_request_log(outcome(
            "hy3",
            "/v1/messages",
            &TokenRecord::default(),
            200,
            None,
            true,
        ))
        .unwrap();

        // Poll until the writer thread commits (it is asynchronous by design).
        let mut seen = 0;
        for _ in 0..200 {
            seen = db.query_today().unwrap().requests_total;
            if seen == 1 {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(seen, 1, "a pooled read must see the committed row");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_in_memory_database_shares_its_single_handle() {
        // A second connection to `:memory:` would be a different, empty
        // database, so in-memory mode must keep sharing one handle.
        let db = StatsDb::in_memory().unwrap();
        db.record_request_log(outcome(
            "hy3",
            "/v1/messages",
            &TokenRecord::default(),
            200,
            None,
            true,
        ))
        .unwrap();
        assert_eq!(
            db.query_today().unwrap().requests_total,
            1,
            "inline writes must be visible to reads"
        );
        assert!(
            db.reader_source.is_none(),
            "in-memory has no path to reopen"
        );
    }

    #[test]
    fn writer_thread_persists_queued_rows() {
        // Persistent mode: record_request_log only queues, a background thread
        // commits. The queue must eventually land every row.
        let dir = std::env::temp_dir().join(format!("proxy-rs-stats-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("stats.db");

        let read_conn = StatsDb::connect(&path).unwrap();
        StatsDb::init_schema(&read_conn).unwrap();
        let write_conn = StatsDb::connect(&path).unwrap();
        let (tx, rx) = channel::<RequestRow>();
        std::thread::spawn(move || writer_loop(write_conn, rx));

        let db = StatsDb {
            readers: Mutex::new(vec![read_conn]),
            reader_source: Some(path.clone()),
            conn: Arc::new(Mutex::new(StatsDb::connect(&path).unwrap())),
            queue: Some(tx),
        };

        let tokens = TokenRecord {
            input: 20,
            cache_read: 80,
            cache_write: 0,
            output: 20,
        };
        for _ in 0..50 {
            db.record_request_log(outcome("hy3", "/v1/responses", &tokens, 200, None, true))
                .unwrap();
        }

        // Poll until the writer thread has drained the queue.
        let mut total = 0;
        for _ in 0..200 {
            std::thread::sleep(Duration::from_millis(10));
            total = db.query_today().unwrap().requests_total;
            if total == 50 {
                break;
            }
        }
        assert_eq!(total, 50);

        let today = db.query_today().unwrap();
        assert_eq!(today.tokens_input, 20 * 50);
        assert_eq!(today.tokens_cache_read, 80 * 50);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
