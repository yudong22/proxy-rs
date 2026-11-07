//! Writing and querying the `request_logs` table.

use super::*;

impl StatsDb {
    /// Record the outcome of one finished request.
    ///
    /// Non-blocking in persistent mode: the row is queued for the writer thread.
    /// Call this exactly once per request, after the response has completed —
    /// never per streamed chunk.
    pub fn record_request_log(&self, outcome: RequestOutcome<'_>) -> Result<()> {
        let row = outcome.to_row(&local_date_string(), &local_datetime_string());
        match &self.queue {
            Some(tx) => tx
                .send(row)
                .map_err(|_| anyhow::anyhow!("stats writer thread stopped")),
            None => {
                let conn = self.conn.lock().unwrap_or_else(|p| p.into_inner());
                let mut stmt = conn.prepare(RequestRow::insert_sql())?;
                row.bind_to(&mut stmt)?;
                Ok(())
            }
        }
    }

    /// Query request logs with optional filtering and pagination.
    pub fn query_request_logs(&self, filter: &RequestLogFilter) -> Result<RequestLogsResult> {
        // Pooled: this runs five queries in a row, so holding the one shared
        // lock would block the GUI's other poller for that whole span.
        let handle = self.read_conn();
        let conn = handle.get();

        // 1. Fetch distinct models for filter dropdown
        let mut model_stmt =
            conn.prepare("SELECT DISTINCT model FROM request_logs ORDER BY model ASC")?;
        let models_iter = model_stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut models = Vec::new();
        for m in models_iter.flatten() {
            if !m.is_empty() {
                models.push(m);
            }
        }

        // 1a. Distinct requested models for the filter dropdown.
        let mut client_model_stmt = conn
            .prepare("SELECT DISTINCT client_model FROM request_logs ORDER BY client_model ASC")?;
        let client_models_iter = client_model_stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut client_models = Vec::new();
        for cm in client_models_iter.flatten() {
            if !cm.is_empty() {
                client_models.push(cm);
            }
        }

        // 1b. Data-driven client list, so the dropdown only offers dialects
        //     that actually appear in this database.
        let mut client_stmt = conn.prepare(
            "SELECT DISTINCT client FROM request_logs WHERE client != '' ORDER BY client ASC",
        )?;
        let clients_iter = client_stmt.query_map([], |row| row.get::<_, String>(0))?;
        let mut clients = Vec::new();
        for c in clients_iter.flatten() {
            if !c.is_empty() {
                clients.push(c);
            }
        }

        // 1c. Conversations for the session filter, newest first. Capped so a
        //     long-lived database cannot make the dropdown itself the slow part;
        //     the cap is generous enough to cover any realistic recent history.
        let mut session_stmt = conn.prepare(
            "SELECT session_id, MAX(client) FROM request_logs
              WHERE session_id != ''
              GROUP BY session_id
              ORDER BY MAX(id) DESC
              LIMIT 200",
        )?;
        let sessions_iter = session_stmt.query_map([], |row| {
            Ok(RequestLogSession {
                session_id: row.get::<_, String>(0)?,
                client: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            })
        })?;
        let mut sessions: Vec<RequestLogSession> = sessions_iter.flatten().collect();
        sessions.retain(|s| !s.session_id.is_empty());

        // 2. Build WHERE clause
        let mut where_clauses = Vec::new();
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();

        if let Some(ref m) = filter.model {
            let m_trimmed = m.trim();
            if !m_trimmed.is_empty() && m_trimmed != "all" {
                where_clauses.push("model = ?".to_string());
                params.push(Box::new(m_trimmed.to_string()));
            }
        }

        if let Some(ref m) = filter.client_model {
            let m_trimmed = m.trim();
            if !m_trimmed.is_empty() && m_trimmed != "all" {
                where_clauses.push("client_model = ?".to_string());
                params.push(Box::new(m_trimmed.to_string()));
            }
        }

        if let Some(ref s) = filter.status_group {
            match s.as_str() {
                "2xx" | "success" => {
                    where_clauses
                        .push("status >= 200 AND status < 300 AND error IS NULL".to_string());
                }
                "4xx" | "client_error" => {
                    where_clauses.push("status >= 400 AND status < 500".to_string());
                }
                "5xx" | "server_error" => {
                    where_clauses.push("status >= 500".to_string());
                }
                "error" => {
                    where_clauses.push("(status >= 400 OR error IS NOT NULL)".to_string());
                }
                _ => {}
            }
        }

        for (value, column) in [
            (&filter.client, "client"),
            (&filter.session_id, "session_id"),
        ] {
            if let Some(ref v) = value {
                let v = v.trim();
                if !v.is_empty() && v != "all" {
                    where_clauses.push(format!("{column} = ?"));
                    params.push(Box::new(v.to_string()));
                }
            }
        }

        if let Some(streamed) = filter.streamed {
            where_clauses.push("streamed = ?".to_string());
            params.push(Box::new(if streamed { 1 } else { 0 }));
        }

        if let Some(ref q) = filter.search {
            let q_trimmed = q.trim();
            if !q_trimmed.is_empty() {
                where_clauses.push(
                    "(model LIKE ? OR route LIKE ? OR error LIKE ? \
                     OR session_id LIKE ? OR client LIKE ?)"
                        .to_string(),
                );
                let like_pat = format!("%{}%", q_trimmed);
                for _ in 0..5 {
                    params.push(Box::new(like_pat.clone()));
                }
            }
        }

        let where_sql = if where_clauses.is_empty() {
            String::new()
        } else {
            format!("WHERE {}", where_clauses.join(" AND "))
        };

        // 3. Count total matching rows
        let count_sql = format!("SELECT COUNT(*) FROM request_logs {}", where_sql);
        let params_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let total: i64 = conn.query_row(
            &count_sql,
            rusqlite::params_from_iter(params_refs.iter().copied()),
            |row| row.get(0),
        )?;

        // 4. Query page items
        let limit = filter.limit.unwrap_or(50).clamp(1, 500);
        let offset = filter.offset.unwrap_or(0);
        let query_sql = format!(
            "SELECT id, created_at, model, client_model, route,
                    input_tokens, output_tokens, cache_read_tokens, cache_write_tokens,
                    duration_ms, ttft_ms, model_ms, ended_with_tool_call,
                    streamed, status, error, session_id, parent_session_id, client,
                    override_key, override_model, override_reason
             FROM request_logs
             {}
             ORDER BY id DESC
             LIMIT ? OFFSET ?",
            where_sql
        );

        let mut query_params: Vec<&dyn rusqlite::ToSql> = params_refs;
        let limit_i64 = limit as i64;
        let offset_i64 = offset as i64;
        query_params.push(&limit_i64);
        query_params.push(&offset_i64);

        let mut stmt = conn.prepare(&query_sql)?;
        let items_iter = stmt.query_map(rusqlite::params_from_iter(query_params), |row| {
            let tool_int: i64 = row.get(12)?;
            let streamed_int: i64 = row.get(13)?;
            let status_int: i64 = row.get(14)?;
            Ok(RequestLogItem {
                id: row.get(0)?,
                created_at: row.get(1)?,
                model: row.get(2)?,
                client_model: row.get(3)?,
                route: row.get(4)?,
                input_tokens: row.get(5)?,
                output_tokens: row.get(6)?,
                cache_read_tokens: row.get(7)?,
                cache_write_tokens: row.get(8)?,
                duration_ms: row.get(9)?,
                ttft_ms: row.get(10)?,
                model_ms: row.get(11)?,
                ended_with_tool_call: tool_int != 0,
                streamed: streamed_int != 0,
                status: status_int as u16,
                error: row.get(15)?,
                session_id: row.get(16)?,
                parent_session_id: row.get(17)?,
                client: row.get(18)?,
                override_key: row.get(19)?,
                override_model: row.get(20)?,
                override_reason: row.get(21)?,
            })
        })?;

        let mut items = Vec::new();
        for item in items_iter {
            items.push(item?);
        }

        Ok(RequestLogsResult {
            items,
            total,
            models,
            client_models,
            clients,
            sessions,
        })
    }

    /// Clear all request logs from the database.
    pub fn clear_request_logs(&self) -> Result<()> {
        // A write, but it needs a connection that sees the same database; for
        // in-memory that is the shared handle, so this borrows through the same
        // path the readers use.
        let handle = self.read_conn();
        handle.get().execute("DELETE FROM request_logs", [])?;
        Ok(())
    }
}
