//! Per-session metric queries (including the sub-agent tree fold).

use super::*;

impl StatsDb {
    /// The conversation the overview's speed card should describe: the one whose
    /// newest request is the most recent **today**.
    ///
    /// A session with no id (unidentified client) is not a conversation, so it
    /// never wins here — the card would otherwise describe an anonymous request
    /// that cannot be followed up. Likewise a subagent child: it is the newest
    /// row in the table most of the time, so without the lineage exclusion the
    /// card would describe a helper's traffic while the panel beside it is
    /// labelled with the parent conversation's id.
    pub fn latest_session_id(&self) -> Result<Option<String>> {
        self.latest_session_id_on(&local_date_string())
    }

    /// [`Self::latest_session_id`] restricted to one local day.
    ///
    /// The card describes today's traffic, so a conversation whose last request
    /// was yesterday must not win the slot — it would show a stale speed while
    /// today's sessions were still running.
    pub fn latest_session_id_on(&self, date: &str) -> Result<Option<String>> {
        let handle = self.read_conn();
        let id = handle
            .get()
            .query_row(
                // Top-level conversations only: a subagent child's turn is
                // usually the newest row in the table, and taking it would make
                // the overview's "current session" card describe one of the
                // conversation's helpers instead of the conversation.
                "SELECT session_id FROM request_logs
                  WHERE session_id != '' AND date = ?1 AND parent_session_id = ''
                  ORDER BY id DESC
                  LIMIT 1",
                params![date],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        Ok(id)
    }

    /// Metrics for the most recently active conversation, **as of today**.
    ///
    /// Convenience wrapper over [`Self::latest_session_id`] +
    /// [`Self::query_session_metrics`] so the GUI makes one round-trip.
    pub fn query_latest_session_metrics(&self) -> Result<SessionMetrics> {
        self.query_latest_session_metrics_on(&local_date_string())
    }

    /// [`Self::query_latest_session_metrics`] against an explicit date.
    ///
    /// Takes the date rather than reading the clock so the "day" boundary is
    /// testable without freezing time.
    pub fn query_latest_session_metrics_on(&self, date: &str) -> Result<SessionMetrics> {
        match self.latest_session_id_on(date)? {
            Some(id) => self.query_session_metrics_on(&id, date),
            None => Ok(SessionMetrics::default()),
        }
    }

    /// Per-session aggregates for the `limit` most recently active conversations
    /// **that were active today**, newest first — the overview's speed panel
    /// compares them side by side.
    ///
    /// Each row is folded exactly like [`Self::query_session_metrics`] (same
    /// window cap, same weighting, same day), so a session's figure does not
    /// change depending on which query produced it.
    ///
    /// Only identified sessions appear: an empty `session_id` is not a
    /// conversation and cannot be compared with one.
    pub fn query_recent_session_metrics(&self, limit: usize) -> Result<Vec<SessionMetrics>> {
        self.query_recent_session_metrics_on(limit, &local_date_string())
    }

    /// [`Self::query_recent_session_metrics`] against an explicit date.
    pub fn query_recent_session_metrics_on(
        &self,
        limit: usize,
        date: &str,
    ) -> Result<Vec<SessionMetrics>> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        // One handle for the whole method. Calling `query_session_metrics` per id
        // would acquire a *second* handle inside the first — an in-memory
        // database shares a single `Mutex<Connection>`, so that self-deadlocks
        // (and it is the GUI's fallback database when the real file cannot be
        // opened, so this is not a test-only hazard).
        let handle = self.read_conn();
        let conn = handle.get();

        // Restricted to today, for the same reason the current session's figure
        // is: the panel exists to compare *today's* conversations, and a session
        // that ran only yesterday would otherwise sit in the comparison with a
        // full-window figure beside today's partial ones.
        //
        // **Top-level conversations only.** The grouping is by `session_id`, so a
        // subagent child would otherwise become a column of its own and the panel
        // would compare "my conversation" against "one of my subagents" — two
        // different units. A child's traffic is folded into its parent below
        // instead.
        //
        // Rows written before lineage reporting existed carry an empty
        // `parent_session_id`, so a *historical* child still appears as its own
        // top-level conversation. That is unavoidable (the information was never
        // recorded) and is why the aggregate reports a subagent **count** rather
        // than claiming none ran.
        let mut stmt = conn.prepare(
            "SELECT session_id FROM request_logs
              WHERE session_id != '' AND date = ?1
                AND parent_session_id = ''
              GROUP BY session_id
              ORDER BY MAX(id) DESC
              LIMIT ?2",
        )?;
        let ids: Vec<String> = stmt
            .query_map(params![date, limit as i64], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            out.push(Self::fold_session_tree_on(conn, &id, Some(date))?);
        }
        Ok(out)
    }

    /// Fold one top-level session together with its subagent children.
    ///
    /// One level only, deliberately. A grandchild's `parent_session_id` points at
    /// its own parent, so it is folded into that parent — which is itself folded
    /// here. Recursing would walk the same rows twice and, on a corrupt cycle,
    /// never terminate.
    ///
    /// Tool wait is computed **per session and then summed**, never over the
    /// interleaved turns: `fold_session_turns` attributes the gap after a
    /// tool-calling turn to the *next* turn, and a parent and its child can be
    /// alternating turns whose adjacency means nothing. Concatenating them would
    /// invent tool waits that never happened.
    fn fold_session_tree_on(
        conn: &Connection,
        session_id: &str,
        date: Option<&str>,
    ) -> Result<SessionMetrics> {
        let mut metrics = Self::fold_session_on(conn, session_id, date)?;

        // The statement is bound to a `let` rather than built inside the match
        // arms: `query_map` borrows the statement for as long as its iterator
        // lives, so an arm-local `stmt` would be dropped while still borrowed.
        let mut stmt = match date {
            Some(_) => conn.prepare(
                "SELECT DISTINCT session_id FROM request_logs
                  WHERE parent_session_id = ?1 AND date = ?2 AND session_id != ''",
            )?,
            None => conn.prepare(
                "SELECT DISTINCT session_id FROM request_logs
                  WHERE parent_session_id = ?1 AND session_id != ''",
            )?,
        };
        let child_ids: Vec<String> = match date {
            Some(d) => stmt
                .query_map(params![session_id, d], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map(params![session_id], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };

        for child in &child_ids {
            let child_metrics = Self::fold_session_on(conn, child, date)?;
            metrics.absorb(&child_metrics);
        }
        metrics.subagent_sessions = child_ids.len() as i64;
        Ok(metrics)
    }

    /// All-time request/token totals for the given namespaced session ids.
    ///
    /// This is the join behind the console's 会话 page: a local transcript is
    /// keyed `claude:<uuid>`, and this answers "what did the gateway see for
    /// this conversation" in one query. Unlike [`Self::query_session_metrics`]
    /// there is no turn cap and no date scope — a session can span days, and a
    /// usage total that silently stopped at N turns would read as wrong.
    ///
    /// Ids with no rows are simply absent from the result: the caller renders
    /// that as "not seen by the gateway", which is different from "zero".
    pub fn session_usage_totals_for(
        &self,
        session_ids: &[String],
    ) -> Result<Vec<SessionUsageTotal>> {
        if session_ids.is_empty() {
            return Ok(Vec::new());
        }
        // One parameter per id; rusqlite has no array binding.
        let placeholders = std::iter::repeat_n("?", session_ids.len())
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT session_id, COUNT(*), COALESCE(SUM(input_tokens), 0),
                    COALESCE(SUM(output_tokens), 0), COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(cache_write_tokens), 0),
                    MIN(created_at), MAX(created_at),
                    GROUP_CONCAT(DISTINCT model)
             FROM request_logs
             WHERE session_id IN ({placeholders})
             GROUP BY session_id"
        );
        let handle = self.read_conn();
        let conn = handle.get();
        let mut stmt = conn.prepare(&sql)?;
        let params = rusqlite::params_from_iter(session_ids.iter());
        let rows = stmt.query_map(params, |row| {
            let models: String = row.get::<_, Option<String>>(8)?.unwrap_or_default();
            Ok(SessionUsageTotal {
                session_id: row.get(0)?,
                requests: row.get(1)?,
                input_tokens: row.get(2)?,
                output_tokens: row.get(3)?,
                cache_read_tokens: row.get(4)?,
                cache_write_tokens: row.get(5)?,
                first_at: row.get(6)?,
                last_at: row.get(7)?,
                models: models
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(str::to_owned)
                    .collect(),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            let mut total = row?;
            total.models.sort();
            out.push(total);
        }
        Ok(out)
    }

    /// Session-level speed metrics for the overview's 输出速度 card, as of today.
    pub fn query_session_metrics(&self, session_id: &str) -> Result<SessionMetrics> {
        self.query_session_metrics_on(session_id, &local_date_string())
    }

    /// [`Self::query_session_metrics`] against an explicit date.
    ///
    /// Folds the session's most recent [`SESSION_METRICS_MAX_TURNS`] turns
    /// **from that day** into one time-weighted aggregate (see
    /// [`SessionMetrics`]) so the figure is stable instead of changing with
    /// every turn.
    ///
    /// The window is taken **newest-first** by `id` and then reversed, so the cap
    /// always keeps the *recent* turns — the conversation's live behaviour —
    /// rather than the oldest ones.
    ///
    /// Tool time is summed across the window: the proxy never executes tools (the
    /// client does, then reports back in a later request), so the gap between a
    /// tool-requesting turn and its successor is the only observable tool time.
    pub fn query_session_metrics_on(&self, session_id: &str, date: &str) -> Result<SessionMetrics> {
        if session_id.is_empty() {
            return Ok(SessionMetrics::default());
        }
        let handle = self.read_conn();
        // The tree fold, not the plain one: this figure and the comparison
        // columns beside it are rendered on the same panel, so they must use the
        // same rule. A session that reads 42k tokens in its own column and 47k
        // in the current-session column would be the worst kind of wrong —
        // two numbers, one screen, no way to tell which is the conversation.
        Self::fold_session_tree_on(handle.get(), session_id, Some(date))
    }

    /// Read one session's window from `conn` and fold it.
    ///
    /// Takes the connection rather than acquiring its own so callers can reuse a
    /// single handle for many sessions (see `query_recent_session_metrics`).
    ///
    /// `date` restricts the window to one local day; `None` folds the session's
    /// whole history. The panel always passes a date — "today" — because an
    /// unbounded figure would mix yesterday's throughput into today's card.
    fn fold_session_on(
        conn: &Connection,
        session_id: &str,
        date: Option<&str>,
    ) -> Result<SessionMetrics> {
        const COLUMNS: &str = "id, created_at, model, input_tokens, output_tokens,
                               cache_read_tokens, duration_ms, ttft_ms, model_ms,
                               ended_with_tool_call";
        let sql = match date {
            Some(_) => format!(
                "SELECT {COLUMNS}
                   FROM request_logs
                  WHERE session_id = ?1 AND date = ?2
                  ORDER BY id DESC
                  LIMIT ?3"
            ),
            None => format!(
                "SELECT {COLUMNS}
                   FROM request_logs
                  WHERE session_id = ?1
                  ORDER BY id DESC
                  LIMIT ?2"
            ),
        };

        let mut stmt = conn.prepare(&sql)?;
        let read_row = |row: &rusqlite::Row<'_>| {
            let tool_int: i64 = row.get(9)?;
            Ok(SessionTurn {
                id: row.get(0)?,
                created_at: row.get(1)?,
                model: row.get(2)?,
                input_tokens: row.get(3)?,
                output_tokens: row.get(4)?,
                cache_read_tokens: row.get(5)?,
                duration_ms: row.get(6)?,
                ttft_ms: row.get(7)?,
                model_ms: row.get(8)?,
                ended_with_tool_call: tool_int != 0,
            })
        };

        let rows = match date {
            Some(d) => stmt
                .query_map(params![session_id, d, SESSION_METRICS_MAX_TURNS], read_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
            None => stmt
                .query_map(params![session_id, SESSION_METRICS_MAX_TURNS], read_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        };

        // Newest-first from SQL (so LIMIT keeps the recent end); the fold reads
        // chronologically, which is what makes "previous turn" meaningful.
        let mut turns: Vec<SessionTurn> = rows;
        turns.reverse();

        let mut metrics = fold_session_turns(session_id, &turns);
        // Whether this id is a child is a property of the rows, not of the fold:
        // ask the database rather than threading the column through every turn.
        let parent: Option<String> = conn
            .query_row(
                "SELECT parent_session_id FROM request_logs
                  WHERE session_id = ?1 AND parent_session_id != ''
                  LIMIT 1",
                params![session_id],
                |row| row.get(0),
            )
            .optional()?;
        metrics.is_subagent = parent.is_some();
        Ok(metrics)
    }
}
