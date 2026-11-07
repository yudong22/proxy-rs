//! Claude Code local session transcripts, for the console's 会话 page.
//!
//! ## What it reads
//!
//! Claude Code writes one append-only JSONL transcript per conversation at
//! `~/.claude/projects/<encoded-cwd>/<session-id>.jsonl`. The file name is the
//! session id, and that is the same id the gateway sees in
//! `x-claude-code-session-id` (namespaced `claude:<uuid>` in `stats.db`) — which
//! is what makes the usage join in [`crate::admin::claude_sessions_list`]
//! possible without any new bookkeeping.
//!
//! Subagent dispatches leave `agent-*.jsonl` files under
//! `<session-id>/subagents/`; they are deliberately **not** listed as sessions
//! (the directory walk never descends), because the subagent's own turns are
//! already inside the parent transcript as `isSidechain: true` entries.
//!
//! ## Why this module is synchronous
//!
//! Every function here does blocking file IO and knows nothing about Tokio or
//! Tauri. Callers wrap a whole request in one `spawn_blocking` — see
//! [`crate::admin::claude_sessions_list`]. Putting the async adaptation inside
//! would invite a `spawn_blocking` per file, which is the one thing this must
//! not do: the blocking pool is a small, shared resource.
//!
//! ## The three parsing traps in real data
//!
//! 1. **One API turn, several `assistant` lines.** Claude Code splits a response
//!    into one line per content block, and every split line carries the *same*
//!    `message.usage`. Summing per line inflates tokens several-fold, so usage
//!    is folded once per turn key `(message.id | uuid, requestId)`.
//! 2. **Resumed sessions replay their history.** The same `uuid` can be
//!    appended again; entries are deduplicated by `uuid`.
//! 3. **`cost-state` is a snapshot that half the files never write.** Nothing
//!    the page presents as authoritative is derived from it; it is shown only
//!    as "Claude Code 自报" when present.
//!
//! Timestamps are returned as epoch milliseconds and formatted by the browser:
//! the server's timezone is not the viewer's, and a UTC string printed with the
//! wrong offset is a silent, plausible-looking error.
//!
//! ## Known non-goals
//!
//! DSH sessions (`~/.dsh/sessions/**/session.vN.jsonl.zstd`, ~492 MB on a
//! typical machine) are zstd-compressed and are not read here. The console
//! shows that source as disabled with an explanation.
//!
//! ## File layout
//!
//! Split from one 1,876-line file (1,394 production + 482 tests). The split moves
//! items only; not a line of logic changed, and every historical
//! `crate::claude_sessions::X` path resolves through the re-exports below, so
//! `src/admin/` and `src/admin_routes.rs` needed no edits.
//!
//! | file | owns |
//! |---|---|
//! | [`types`] | the limits/constants and every serialized shape: `SessionSummary`, `SessionDetail`, `ListArgs`, `TimelineEntry`… |
//! | [`paths`] | `ClaudeSessionsSource`, id validation, resolving a session id to a file, and the directory walk |
//! | [`parse`] | line-level helpers: `parse_iso_ms`, the JSON field accessors, text capping, `read_lines` |
//! | [`summarize`] | `Accum` — the one pass that folds a transcript into `SessionSummary`, and the subagent count |
//! | [`read`] | the public entry points: `list_sessions`, `read_session`, `read_subagent`, the gateway join |
//!
//! `use super::*;` in each submodule is deliberate: the pieces share the imports
//! and call each other freely (a summary is folded by `summarize`, paged by
//! `read`, and described by `types`), so re-declaring imports per file would be
//! churn with no isolation benefit. The test module stays whole at the bottom of
//! this file, exactly as it was.

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use crate::settings::home_dir;
use crate::stats::SessionUsageTotal;

mod parse;
mod paths;
mod read;
mod summarize;
mod types;

pub use parse::*;
pub use paths::*;
pub use read::*;
pub(crate) use summarize::*;
pub use types::*;

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        root: PathBuf,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root =
                std::env::temp_dir().join(format!("proxy-rs-claude-{}-{name}", std::process::id()));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self { root }
        }

        fn src(&self) -> ClaudeSessionsSource {
            ClaudeSessionsSource::new(&self.root)
        }

        fn write(&self, project: &str, file: &str, lines: &[String]) -> PathBuf {
            let dir = self.root.join(project);
            fs::create_dir_all(&dir).unwrap();
            let path = dir.join(file);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent).unwrap();
            }
            let mut body = lines.join("\n");
            if !body.is_empty() {
                body.push('\n');
            }
            fs::write(&path, body).unwrap();
            path
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    const SID: &str = "b9c3800a-311e-4c89-9a38-c533d8969b5d";
    const PROJ: &str = "-Users-denis-Sites-proxy-rs";

    fn user(uuid: &str, ts: &str, text: &str) -> String {
        serde_json::json!({
            "type": "user", "uuid": uuid, "parentUuid": null,
            "timestamp": ts, "cwd": "/Users/denis/Sites/proxy-rs", "version": "2.1.277",
            "gitBranch": "main",
            "message": {"role": "user", "content": text}
        })
        .to_string()
    }

    fn tool_result_user(uuid: &str, ts: &str) -> String {
        serde_json::json!({
            "type": "user", "uuid": uuid, "timestamp": ts,
            "message": {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_1", "content": "ok"}
            ]}
        })
        .to_string()
    }

    fn assistant(uuid: &str, ts: &str, msg_id: &str, block: serde_json::Value) -> String {
        serde_json::json!({
            "type": "assistant", "uuid": uuid, "timestamp": ts,
            "message": {
                "id": msg_id, "role": "assistant", "model": "deepseek-v4.1-flash",
                "usage": {"input_tokens": 100, "output_tokens": 50,
                          "cache_read_input_tokens": 10, "cache_creation_input_tokens": 5},
                "content": [block]
            }
        })
        .to_string()
    }

    fn title(t: &str) -> String {
        serde_json::json!({"type": "ai-title", "aiTitle": t, "sessionId": SID}).to_string()
    }

    #[test]
    fn list_finds_uuid_files_only() {
        let f = Fixture::new("list-files");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hi")],
        );
        f.write(
            PROJ,
            "not-a-session.jsonl",
            &[user("x", "2026-11-01T10:00:00.000Z", "x")],
        );
        f.write(PROJ, "memory/MEMORY.md", &[]);
        let page = list_sessions(&f.src(), &ListArgs::default()).unwrap();
        assert_eq!(page.total, 1);
        assert_eq!(page.sessions[0].summary.id, SID);
    }

    #[test]
    fn cwd_comes_from_the_line_not_the_directory_name() {
        let f = Fixture::new("cwd");
        f.write(
            "-Users-a-b-c",
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hi")],
        );
        let page = list_sessions(&f.src(), &ListArgs::default()).unwrap();
        // The transcript says /Users/denis/Sites/proxy-rs, which the dir name cannot encode.
        assert_eq!(page.sessions[0].summary.cwd, "/Users/denis/Sites/proxy-rs");
    }

    #[test]
    fn title_is_the_last_ai_title_and_falls_back_to_first_prompt() {
        let f = Fixture::new("title");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[
                title("first"),
                user("u1", "2026-11-01T10:00:00.000Z", "the real question"),
                title("second"),
                title("third"),
            ],
        );
        let page = list_sessions(&f.src(), &ListArgs::default()).unwrap();
        assert_eq!(page.sessions[0].summary.title, "third");

        let f2 = Fixture::new("title-fallback");
        f2.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hello world")],
        );
        let page = list_sessions(&f2.src(), &ListArgs::default()).unwrap();
        assert_eq!(page.sessions[0].summary.title, "hello world");
    }

    #[test]
    fn assistant_lines_sharing_message_id_count_tokens_once() {
        let f = Fixture::new("turn-dedupe");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[
                user("u1", "2026-11-01T10:00:00.000Z", "go"),
                assistant("a1", "2026-11-01T10:00:01.000Z", "msg_1", serde_json::json!({"type":"text","text":"部分一"})),
                assistant("a2", "2026-11-01T10:00:01.500Z", "msg_1", serde_json::json!({"type":"tool_use","id":"toolu_1","name":"Bash","input":{"cmd":"ls"}})),
            ],
        );
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.assistant_turns, 1);
        assert_eq!(d.tokens.output, 50);
        assert_eq!(d.tokens.input, 100);
        assert_eq!(d.summary.tool_calls, 1);
        // Usage appears on exactly one entry.
        assert_eq!(d.timeline.iter().filter(|e| e.tokens.is_some()).count(), 1);
    }

    #[test]
    fn tool_results_and_noise_are_not_turns() {
        let f = Fixture::new("noise");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[
                user(
                    "u0",
                    "2026-11-01T09:00:00.000Z",
                    "<system-reminder>ignore me</system-reminder>",
                ),
                user("u1", "2026-11-01T10:00:00.000Z", "real prompt"),
                tool_result_user("u2", "2026-11-01T10:00:05.000Z"),
            ],
        );
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.turns, 1);
        assert_eq!(d.ignored.user_noise, 1);
        assert!(d.timeline.iter().any(|e| e.kind == EntryKind::ToolResult));
    }

    #[test]
    fn duplicate_uuids_are_dropped() {
        let f = Fixture::new("dedupe");
        let base = vec![
            user("u1", "2026-11-01T10:00:00.000Z", "one"),
            user("u2", "2026-11-01T10:00:10.000Z", "two"),
        ];
        let mut replay = base.clone();
        replay.push(user("u1", "2026-11-01T10:00:00.000Z", "one"));
        f.write(PROJ, &format!("{SID}.jsonl"), &replay);
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.turns, 2);
        assert_eq!(d.ignored.duplicate_uuid, 1);
    }

    #[test]
    fn malformed_lines_do_not_abort_the_file() {
        let f = Fixture::new("malformed");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[
                "{".to_string(),
                "null".to_string(),
                "[]".to_string(),
                "\"str\"".to_string(),
                "{\"type\":\"brand-new\"}".to_string(),
                user("u1", "2026-11-01T10:00:00.000Z", "survivor"),
            ],
        );
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.turns, 1);
        // `{` is not JSON (malformed); `null` / `[]` / `"str"` are JSON but not
        // transcript lines, and `brand-new` is an unknown type — all `other`.
        assert_eq!(d.ignored.malformed, 1);
        assert_eq!(d.ignored.other, 4);
        assert_eq!(d.timeline.len(), 1);
    }

    #[test]
    fn stringified_message_and_usage_do_not_panic() {
        let f = Fixture::new("stringified");
        let line = serde_json::json!({
            "type": "assistant", "uuid": "a1", "timestamp": "2026-11-01T10:00:00.000Z",
            "message": {"id": "m", "model": "x", "usage": "{\"output_tokens\": 7}",
                        "content": "plain string content"}
        })
        .to_string();
        let user_line = serde_json::json!({
            "type": "user", "uuid": "u1", "timestamp": "2026-11-01T09:00:00.000Z",
            "message": "just a string"
        })
        .to_string();
        f.write(PROJ, &format!("{SID}.jsonl"), &[user_line, line]);
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.tokens.output, 7);
        assert!(d.timeline.iter().any(|e| e.text == "plain string content"));
    }

    #[test]
    fn cost_state_last_wins_and_model_usage_parsed() {
        let f = Fixture::new("cost");
        let cost = |usd: f64| {
            serde_json::json!({
                "type": "cost-state", "totalCostUSD": usd, "totalDuration": 1000,
                "startTime": 1792925722078i64,
                "modelUsage": {"hy3": {"inputTokens": 10, "outputTokens": 20}}
            })
            .to_string()
        };
        f.write(PROJ, &format!("{SID}.jsonl"), &[cost(1.0), cost(2.5)]);
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        let c = d.cost_state.expect("cost state");
        assert_eq!(c.total_cost_usd, 2.5);
        assert_eq!(c.models[0].tokens.output, 20);
    }

    #[test]
    fn head_tail_listing_uses_tail_for_title() {
        let f = Fixture::new("headtail");
        let big = "x".repeat(500);
        let mut lines: Vec<String> = Vec::new();
        lines.push(user("u0", "2026-11-01T10:00:00.000Z", "opening"));
        // Pad well past 2 * HEAD_TAIL_BYTES.
        for i in 0..1200 {
            lines.push(
                serde_json::json!({"type":"attachment","uuid":format!("x{i}"),"rendered":big})
                    .to_string(),
            );
        }
        lines.push(title("tail title"));
        lines.push(user("u_last", "2026-11-01T12:00:00.000Z", "closing"));
        let path = f.write(PROJ, &format!("{SID}.jsonl"), &lines);
        assert!(fs::metadata(&path).unwrap().len() > HEAD_TAIL_BYTES * 2);
        let page = list_sessions(&f.src(), &ListArgs::default()).unwrap();
        let s = &page.sessions[0].summary;
        assert_eq!(s.title, "tail title");
        assert!(s.counts_partial);
        assert_eq!(s.cwd, "/Users/denis/Sites/proxy-rs", "cwd from the head");
    }

    #[test]
    fn timeline_pagination_is_stable() {
        let f = Fixture::new("paging");
        let mut lines = Vec::new();
        for i in 0..10 {
            let ts = format!("2026-11-01T10:00:{i:02}.000Z");
            lines.push(user(&format!("u{i}"), &ts, &format!("prompt {i}")));
        }
        f.write(PROJ, &format!("{SID}.jsonl"), &lines);
        let newest = read_session(
            &f.src(),
            SID,
            &DetailArgs {
                offset: Some(0),
                limit: Some(3),
            },
        )
        .unwrap();
        assert_eq!(newest.timeline.len(), 3);
        assert_eq!(newest.timeline.last().unwrap().text, "prompt 9");
        assert!(newest.has_more);
        assert_eq!(newest.timeline_total, 10);
        let older = read_session(
            &f.src(),
            SID,
            &DetailArgs {
                offset: Some(3),
                limit: Some(3),
            },
        )
        .unwrap();
        assert_eq!(older.timeline.last().unwrap().text, "prompt 6");
        let last = read_session(
            &f.src(),
            SID,
            &DetailArgs {
                offset: Some(9),
                limit: Some(3),
            },
        )
        .unwrap();
        assert_eq!(last.timeline.len(), 1);
        assert!(!last.has_more);
    }

    #[test]
    fn invalid_ids_are_rejected() {
        let f = Fixture::new("unsafe");
        f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hi")],
        );
        for bad in [
            "../../etc/passwd",
            "..",
            "/etc/passwd",
            "x",
            &"a".repeat(100),
            "B9C3800A-311E-4C89-9A38-C533D8969B5D",
        ] {
            assert!(
                read_session(&f.src(), bad, &DetailArgs::default()).is_err(),
                "{bad} must be rejected"
            );
            assert!(resolve_session_path(&f.src(), bad).is_err());
        }
        // Valid but absent.
        assert!(read_session(
            &f.src(),
            "00000000-0000-4000-8000-000000000000",
            &DetailArgs::default()
        )
        .is_err());
    }

    #[test]
    fn subagent_read_path_is_confined() {
        let f = Fixture::new("subagent");
        let path = f.write(
            PROJ,
            &format!("{SID}.jsonl"),
            &[user("u1", "2026-11-01T10:00:00.000Z", "hi")],
        );
        let sub_dir = path.with_extension("").join("subagents");
        fs::create_dir_all(&sub_dir).unwrap();
        fs::write(
            sub_dir.join("agent-abc123.jsonl"),
            user("s1", "2026-11-01T10:00:01.000Z", "sub"),
        )
        .unwrap();
        fs::write(
            sub_dir.join("agent-abc123.meta.json"),
            r#"{"agentType":"Explore","name":"scout"}"#,
        )
        .unwrap();

        let ok = read_subagent(&f.src(), SID, "abc123", &DetailArgs::default()).unwrap();
        assert_eq!(ok.summary.turns, 1);
        assert_eq!(ok.agent_id.as_deref(), Some("abc123"));

        let list = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(list.summary.subagents, 1);
        assert_eq!(list.subagents[0].agent_type.as_deref(), Some("Explore"));

        assert!(read_subagent(&f.src(), SID, "../x", &DetailArgs::default()).is_err());
        assert!(read_subagent(&f.src(), SID, "missing", &DetailArgs::default()).is_err());
    }

    #[test]
    fn non_utf8_bytes_become_replacement_chars() {
        let f = Fixture::new("nonutf8");
        let dir = f.root.join(PROJ);
        fs::create_dir_all(&dir).unwrap();
        let mut bytes = user("u1", "2026-11-01T10:00:00.000Z", "ok").into_bytes();
        bytes.push(b'\n');
        bytes.extend_from_slice(b"{\"type\":\"user\",\"uuid\":\"u2\",\"text\":\"\xff\xfe\"}\n");
        fs::write(dir.join(format!("{SID}.jsonl")), bytes).unwrap();
        let d = read_session(&f.src(), SID, &DetailArgs::default()).unwrap();
        assert_eq!(d.summary.turns, 1);
    }

    #[test]
    fn parse_iso_ms_handles_offsets_and_fractions() {
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(parse_iso_ms("1970-01-01T00:00:00.123456Z"), Some(123));
        assert_eq!(parse_iso_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(
            parse_iso_ms("2026-10-25T11:34:10.152Z"),
            parse_iso_ms("2026-10-25T11:34:10.152Z")
        );
        assert!(parse_iso_ms("garbage").is_none());
    }

    #[test]
    fn human_prompt_filters_machinery() {
        assert!(!human_prompt("<system-reminder>x</system-reminder>"));
        assert!(!human_prompt("<task-notification>"));
        assert!(!human_prompt("   "));
        assert!(human_prompt("规划会话管理"));
    }

    #[test]
    fn gateway_join_uses_the_claude_namespace() {
        use crate::stats::{RequestOutcome, StatsDb, TokenRecord, UpstreamTiming};
        let db = StatsDb::in_memory().unwrap();
        let sid = format!("claude:{SID}");
        let tokens = TokenRecord {
            input: 100,
            output: 50,
            cache_read: 10,
            cache_write: 5,
        };
        db.record_request_log(RequestOutcome {
            model: "deepseek-v4.1-flash",
            client_model: "deepseek-v4.1-flash",
            route: "/v1/messages",
            tokens: &tokens,
            duration_ms: 10,
            timing: UpstreamTiming::default(),
            streamed: true,
            status: 200,
            error: None,
            session_id: &sid,
            parent_session_id: "",
            client: "claude",
            override_key: "",
            override_model: "",
            override_reason: "",
        })
        .unwrap();

        let mut rows = vec![
            SessionRow {
                summary: SessionSummary {
                    id: SID.to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
            SessionRow {
                summary: SessionSummary {
                    id: "00000000-0000-4000-8000-000000000000".to_string(),
                    ..Default::default()
                },
                ..Default::default()
            },
        ];
        let ids = gateway_ids(&rows);
        let totals = db.session_usage_totals_for(&ids).unwrap();
        attach_gateway(&mut rows, &totals);
        assert_eq!(rows[0].gateway.as_ref().unwrap().requests, 1);
        assert_eq!(rows[0].gateway.as_ref().unwrap().output_tokens, 50);
        assert!(rows[1].gateway.is_none(), "unknown id is absent, not zero");
    }
}
