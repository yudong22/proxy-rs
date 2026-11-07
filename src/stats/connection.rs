//! Database handle, schema, and the read-connection pool.

use super::*;

/// Persistent request-log store backed by a local SQLite database.
///
/// The database lives at `~/.proxy-rs/stats.db` and holds a single
/// `request_logs` table — one row per finished request. Daily aggregates are
/// computed from it with `GROUP BY date`.
///
/// Writes never run on a request handler: `record_request_log` pushes the row
/// onto an unbounded queue and returns, and a dedicated writer thread drains
/// that queue in batches inside a transaction. So a slow disk or a lock
/// contention with the GUI's read queries can never stall a proxied request.
///
/// Queries use their own connection; WAL mode lets them read while the writer
/// thread holds the write lock.
///
/// For a file-backed database, reads draw from a small pool instead of sharing
/// one connection. WAL already permits concurrent readers at the SQLite level,
/// so the bottleneck was the single `Mutex` serializing them: the GUI polls
/// both the stats line and the request-log table every two seconds, and one
/// shared lock made those queries queue behind each other.
///
/// An in-memory database cannot be shared across connections — a second
/// connection would be a *different*, empty database — so those fall back to
/// the single shared handle, which is correct for tests and the non-persistent
/// fallback the GUI uses when the real file cannot be opened.
pub struct StatsDb {
    /// Read connections, checked out for the duration of a query. Only used
    /// when `reader_source` is set; otherwise `conn` serves every read.
    pub(crate) readers: Mutex<Vec<Connection>>,
    /// Path to open additional read connections from. `None` for in-memory
    /// databases, which cannot be reopened.
    pub(crate) reader_source: Option<PathBuf>,
    /// The connection used for writes in inline mode (no writer thread) and for
    /// every read when the database is in-memory.
    pub(crate) conn: Arc<Mutex<Connection>>,
    /// Queue feeding the background writer thread. `None` in inline mode,
    /// where writes go straight through `conn`.
    pub(crate) queue: Option<Sender<RequestRow>>,
}

/// Upper bound on pooled read connections.
///
/// The GUI issues two pollers; a handful of spare connections covers that plus
/// an ad-hoc query, without opening unbounded handles on a desktop app.
const MAX_READ_CONNS: usize = 4;

/// Where a read should come from: a pooled connection (file-backed) or the
/// single shared handle (in-memory).
pub(crate) enum ReadHandle<'a> {
    Pooled(ReadConn<'a>),
    Shared(std::sync::MutexGuard<'a, Connection>),
}

impl ReadHandle<'_> {
    pub(crate) fn get(&self) -> &Connection {
        match self {
            ReadHandle::Pooled(r) => r.get(),
            ReadHandle::Shared(g) => g,
        }
    }
}

/// A read connection borrowed from the pool, returned on drop.
pub(crate) struct ReadConn<'a> {
    pool: &'a Mutex<Vec<Connection>>,
    conn: Option<Connection>,
}

impl ReadConn<'_> {
    fn get(&self) -> &Connection {
        // `Some` for the guard's whole life; `take` only happens in `Drop`.
        self.conn.as_ref().expect("read connection checked out")
    }
}

impl Drop for ReadConn<'_> {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if let Ok(mut pool) = self.pool.lock() {
                pool.push(conn);
            }
        }
    }
}

impl StatsDb {
    /// Open (or create) the statistics database, ensure the schema exists, and
    /// spawn the background writer thread.
    pub fn open() -> Result<Arc<Self>> {
        let path = match data_dir() {
            Some(dir) => dir.join("stats.db"),
            None => {
                return Err(anyhow::anyhow!(
                    "Cannot determine data directory for stats.db"
                ))
            }
        };
        Self::open_at(&path)
    }

    /// Open a file-backed database at an explicit path.
    ///
    /// Split from [`open`] so tests can use a temp file instead of the user's
    /// real `~/.proxy-rs/stats.db`.
    pub fn open_at(path: &std::path::Path) -> Result<Arc<Self>> {
        let seed_conn = Self::connect(path)?;
        Self::init_schema(&seed_conn)?;
        let write_conn = Self::connect(path)?;

        let (tx, rx) = channel::<RequestRow>();
        std::thread::Builder::new()
            .name("stats-writer".to_string())
            .spawn(move || writer_loop(write_conn, rx))?;

        Ok(Arc::new(Self {
            readers: Mutex::new(vec![seed_conn]),
            reader_source: Some(path.to_path_buf()),
            conn: Arc::new(Mutex::new(Connection::open_in_memory()?)),
            queue: Some(tx),
        }))
    }

    /// Construct from an existing connection (e.g. an in-memory DB for tests).
    ///
    /// Inline mode: no writer thread, so writes are synchronous and immediately
    /// visible to queries.
    pub fn from_conn(conn: Connection) -> Self {
        Self {
            readers: Mutex::new(Vec::new()),
            reader_source: None,
            conn: Arc::new(Mutex::new(conn)),
            queue: None,
        }
    }

    /// Open a fresh in-memory database with the schema applied. Useful as a
    /// non-persistent fallback and for tests.
    pub fn in_memory() -> Result<Arc<Self>> {
        let conn = Connection::open_in_memory()?;
        Self::init_schema(&conn)?;
        Ok(Arc::new(Self {
            readers: Mutex::new(Vec::new()),
            reader_source: None,
            conn: Arc::new(Mutex::new(conn)),
            queue: None,
        }))
    }

    /// Borrow a connection for a read query.
    ///
    /// A file-backed database hands out a pooled connection so concurrent
    /// readers do not serialize on one lock. In-memory databases share the
    /// single handle: a second connection would be a different, empty database.
    pub(crate) fn read_conn(&self) -> ReadHandle<'_> {
        let Some(path) = self.reader_source.as_ref() else {
            return ReadHandle::Shared(self.conn.lock().unwrap_or_else(|p| p.into_inner()));
        };

        if let Some(conn) = self.readers.lock().unwrap_or_else(|p| p.into_inner()).pop() {
            return ReadHandle::Pooled(ReadConn {
                pool: &self.readers,
                conn: Some(conn),
            });
        }

        // Pool empty: open another up to the cap, else share the single handle
        // rather than making the reader wait.
        let room = self
            .readers
            .lock()
            .map(|p| p.len() < MAX_READ_CONNS)
            .unwrap_or(false);
        match room.then(|| Self::connect(path).ok()).flatten() {
            Some(conn) => ReadHandle::Pooled(ReadConn {
                pool: &self.readers,
                conn: Some(conn),
            }),
            None => ReadHandle::Shared(self.conn.lock().unwrap_or_else(|p| p.into_inner())),
        }
    }

    pub(crate) fn connect(path: &std::path::Path) -> Result<Connection> {
        let conn = Connection::open(path)?;
        // Enable WAL mode so the GUI can read while the writer thread writes.
        conn.execute_batch("PRAGMA journal_mode=WAL;")?;
        // Wait rather than fail outright if the write lock is momentarily held.
        conn.busy_timeout(Duration::from_secs(5))?;
        Ok(conn)
    }

    pub(crate) fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS request_logs (
                id                  INTEGER PRIMARY KEY AUTOINCREMENT,
                date                TEXT NOT NULL DEFAULT '',
                created_at          TEXT NOT NULL,
                model               TEXT NOT NULL,
                client_model        TEXT NOT NULL DEFAULT '',
                route               TEXT NOT NULL,
                input_tokens        INTEGER NOT NULL DEFAULT 0,
                output_tokens       INTEGER NOT NULL DEFAULT 0,
                cache_read_tokens   INTEGER NOT NULL DEFAULT 0,
                cache_write_tokens  INTEGER NOT NULL DEFAULT 0,
                duration_ms         INTEGER NOT NULL DEFAULT 0,
                ttft_ms             INTEGER NOT NULL DEFAULT 0,
                model_ms            INTEGER NOT NULL DEFAULT 0,
                ended_with_tool_call INTEGER NOT NULL DEFAULT 0,
                streamed            INTEGER NOT NULL DEFAULT 0,
                status              INTEGER NOT NULL DEFAULT 0,
                error               TEXT,
                session_id          TEXT NOT NULL DEFAULT '',
                parent_session_id   TEXT NOT NULL DEFAULT '',
                client              TEXT NOT NULL DEFAULT '',
                override_key        TEXT NOT NULL DEFAULT '',
                override_model      TEXT NOT NULL DEFAULT '',
                override_reason     TEXT NOT NULL DEFAULT ''
            );
            CREATE INDEX IF NOT EXISTS idx_request_logs_id_desc ON request_logs(id DESC);
            CREATE INDEX IF NOT EXISTS idx_request_logs_created_at ON request_logs(created_at DESC);
            CREATE INDEX IF NOT EXISTS idx_request_logs_model ON request_logs(model);
            CREATE INDEX IF NOT EXISTS idx_request_logs_status ON request_logs(status);",
        )?;
        // Must run before the `date` index is created, since the column only
        // exists after migration on databases from older builds.
        Self::migrate(conn)?;
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_date ON request_logs(date);
             CREATE INDEX IF NOT EXISTS idx_request_logs_session_id
                 ON request_logs(session_id);",
        )?;
        Ok(())
    }

    /// Bring a database written by an older version up to the current schema.
    ///
    /// Older builds kept a separate `daily_stats` table and had no `date`
    /// column; both are reconciled here so existing installs keep their history.
    pub(crate) fn migrate(conn: &Connection) -> Result<()> {
        let has_date = conn
            .prepare("PRAGMA table_info(request_logs)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .flatten()
            .any(|col| col == "date");

        if !has_date {
            conn.execute("ALTER TABLE request_logs ADD COLUMN date TEXT", [])?;
        }

        // `session_id`/`client` arrived with session-aware logging. SQLite has
        // no `ADD COLUMN IF NOT EXISTS`, so each is probed first and old rows
        // keep the empty-string default.
        for column in [
            "session_id",
            "parent_session_id",
            "client",
            "override_key",
            "override_model",
            "override_reason",
            "client_model",
        ] {
            let exists = conn
                .prepare("PRAGMA table_info(request_logs)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .flatten()
                .any(|col| col == column);
            if !exists {
                conn.execute(
                    &format!(
                        "ALTER TABLE request_logs ADD COLUMN {column} TEXT NOT NULL DEFAULT ''"
                    ),
                    [],
                )?;
            }
        }

        // Backfill rows written before the column existed.
        conn.execute(
            "UPDATE request_logs
                SET date = substr(created_at, 1, 10)
              WHERE date IS NULL OR date = ''",
            [],
        )?;

        // DSH ids used to be stored with the harness's fixed `session-` prefix
        // (`dsh:session-<uuid>`); `session::detect` now strips it so a DSH id
        // reads like the bare-uuid `claude:`/`codex:` ones. Rewrite the existing
        // rows too, or the filter would list the same conversation twice — once
        // under each spelling — after the format change. The `session-` word is
        // fixed, so this only ever shortens keys and never merges two ids.
        conn.execute(
            "UPDATE request_logs
                SET session_id = 'dsh:' || substr(session_id, length('dsh:session-') + 1)
              WHERE session_id LIKE 'dsh:session-%'",
            [],
        )?;

        // Upstream timings arrived with the overview's output-speed card. The
        // loop above only handles TEXT columns, so these get their own numeric
        // pass; old rows keep 0, which the UI renders as "unknown" rather than
        // inventing a speed for requests we never measured.
        for column in ["ttft_ms", "model_ms", "ended_with_tool_call"] {
            let exists = conn
                .prepare("PRAGMA table_info(request_logs)")?
                .query_map([], |row| row.get::<_, String>(1))?
                .flatten()
                .any(|col| col == column);
            if !exists {
                conn.execute(
                    &format!(
                        "ALTER TABLE request_logs ADD COLUMN {column} INTEGER NOT NULL DEFAULT 0"
                    ),
                    [],
                )?;
            }
        }

        // The `client_model` lookups back the new "requested model" filter; the
        // index is created here (after the column exists) so it is added on both
        // fresh installs and upgraded databases without racing `init_schema`.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_request_logs_client_model ON request_logs(client_model);",
        )?;

        // daily_stats is now derived from request_logs on demand.
        conn.execute_batch("DROP TABLE IF EXISTS daily_stats;")?;

        Ok(())
    }
}
