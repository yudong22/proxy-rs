use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::OnceLock;
use tokio::sync::RwLock;

/// One log entry in the in-memory ring buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    pub ts: String,
    pub level: String,
    pub message: String,
}

/// Size-capped in-memory log history for GUI / dashboard.
pub struct LogBuffer {
    entries: RwLock<Vec<LogEntry>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            entries: RwLock::new(LogBuffer::load_persisted(capacity)),
            capacity,
        }
    }

    pub async fn push(&self, level: &str, message: String) {
        let entry = LogEntry {
            ts: chrono_now(),
            level: level.to_string(),
            message,
        };
        {
            let mut entries = self.entries.write().await;
            if entries.len() >= self.capacity {
                let overflow = entries.len() + 1 - self.capacity;
                entries.drain(..overflow);
            }
            entries.push(entry.clone());
        }
        // Mirror to ~/.proxy-rs/logs/proxy.log so history survives restarts.
        append_log_line(&format!("{} [{}] {}", entry.ts, entry.level, entry.message));
    }

    /// Recent lines from the on-disk log (used to seed the buffer at startup).
    pub fn load_persisted(capacity: usize) -> Vec<LogEntry> {
        let Some(path) = log_file_path() else {
            return Vec::new();
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            return Vec::new();
        };
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(capacity);
        lines[start..]
            .iter()
            .filter_map(|line| {
                // Format: "YYYY-MM-DD HH:MM:SS.mmm [LEVEL] message"
                let (ts, rest) = line.split_once(" [")?;
                let (level, message) = rest.split_once("] ")?;
                Some(LogEntry {
                    ts: ts.to_string(),
                    level: level.to_string(),
                    message: message.to_string(),
                })
            })
            .collect()
    }

    pub async fn snapshot(&self) -> Vec<LogEntry> {
        self.entries.read().await.clone()
    }

    pub async fn clear(&self) {
        self.entries.write().await.clear();
        // Truncate through the writer thread rather than writing the file
        // directly. A direct truncate races the queue: lines already sent but
        // not yet flushed land *after* the truncate and the log refills itself,
        // which is why "清空" appeared to do nothing. Queueing a truncate keeps
        // it ordered after everything already queued, and drops the writer's
        // handle so it reopens against the emptied file.
        //
        // The wait runs on a blocking thread: `recv_timeout` must not park a
        // runtime worker, and correctness does not depend on the ack anyway
        // (the truncate is ordered regardless of who waits for it).
        let (tx, rx) = channel::<()>();
        if log_writer()
            .send(LogInstruction::Truncate(Box::new(move || {
                let _ = tx.send(());
            })))
            .is_ok()
        {
            let _ = tokio::task::spawn_blocking(move || {
                rx.recv_timeout(std::time::Duration::from_secs(5))
            })
            .await;
        }
    }
}

/// Local wall-clock timestamp for a log line: `YYYY-MM-DD HH:MM:SS.mmm`.
/// Local here means the machine's timezone (`PROXY_TZ_OFFSET_HOURS` overrides).
fn chrono_now() -> String {
    crate::util::local_datetime_millis()
}

/// Default listen port.
///
/// A busy port is a hard error rather than a silent `+1`: the client CLIs are
/// configured against this one fixed URL (see `ServiceController`), so a dev
/// instance that needs to run alongside the installed app sets its own port
/// instead of being moved off this one.
pub const DEFAULT_PORT: u16 = 3456;

/// Persisted proxy and GUI settings in `~/.proxy-rs/gui-settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuiSettings {
    pub provider_id: String,
    /// Custom endpoint override (empty = use preset default)
    pub custom_url: String,
    pub api_key: String,
    pub port: u16,
    pub bind: String,
    #[serde(default)]
    pub reasoning_model: String,
    #[serde(default)]
    pub completion_model: String,
    #[serde(default)]
    pub model_map: String,
    #[serde(default)]
    pub launch_at_login: bool,
    /// Optional semicolon-separated list of fixed-template phrases to neutralize
    /// in the outbound request (WorkBuddy/CodeBuddy content-filter fingerprints).
    /// Forwarded to the Rust sanitize pass via `Config::sanitize_fingerprints`;
    /// exposed in the GUI so it can be edited without restarting via env vars.
    #[serde(default)]
    pub sanitize_terms: String,
    /// Whether the upstream only serves streaming bodies.
    ///
    /// `None` follows the provider preset's own `force_stream` flag (the
    /// default, and what a newly added provider gets from `builtin_presets`);
    /// `Some(true/false)` is the GUI switch, which also covers a custom URL
    /// whose preset we do not know. When enabled, every request goes upstream
    /// as a stream and a non-streaming client still gets one JSON body.
    #[serde(default)]
    pub force_stream: Option<bool>,
}

impl Default for GuiSettings {
    fn default() -> Self {
        Self {
            provider_id: "workbuddy-cn".to_string(),
            custom_url: String::new(),
            api_key: String::new(),
            port: 3456,
            bind: "127.0.0.1".to_string(),
            reasoning_model: String::new(),
            completion_model: String::new(),
            model_map: String::new(),
            launch_at_login: false,
            sanitize_terms: String::new(),
            force_stream: None,
        }
    }
}

/// Canonical data directory: `~/.proxy-rs`.
///
/// [`DATA_DIR_ENV`] relocates the whole directory — settings, `stats.db`,
/// `logs/` and `.env` together — which is what keeps a development run from
/// writing request statistics into the installed app's database.
pub fn data_dir() -> Option<PathBuf> {
    let dir = match env_data_dir() {
        Some(dir) => dir,
        None => {
            let home = std::env::var("HOME").ok()?;
            PathBuf::from(home).join(".proxy-rs")
        }
    };
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// Override the data directory, for a second instance that must not share
/// state with the installed app. `task dev` points this at `~/.proxy-rs-dev`.
const DATA_DIR_ENV: &str = "PROXY_DATA_DIR";
const LEGACY_DATA_DIR_ENV: &str = "ANTHROPIC_PROXY_DATA_DIR";

/// The [`DATA_DIR_ENV`] override, `~` expanded. `None` when unset or blank.
fn env_data_dir() -> Option<PathBuf> {
    let raw = std::env::var(DATA_DIR_ENV)
        .or_else(|_| std::env::var(LEGACY_DATA_DIR_ENV))
        .ok()?;
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match raw.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var("HOME").ok()?;
            Some(PathBuf::from(home).join(rest))
        }
        None => Some(PathBuf::from(raw)),
    }
}

/// Stable settings path: `~/.proxy-rs/gui-settings.json`.
///
/// Relocating the file is done by relocating [`data_dir`], not by a second
/// override here: one knob keeps settings, `stats.db`, `logs/` and `.env`
/// together, so a second instance can never end up half-isolated.
pub fn settings_path() -> PathBuf {
    if let Some(dir) = data_dir() {
        return dir.join("gui-settings.json");
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("gui-settings.json")
}

/// `.env` config path: `~/.proxy-rs/.env`.
pub fn dotenv_path() -> Option<PathBuf> {
    data_dir().map(|dir| dir.join(".env"))
}

/// Log directory: `~/.proxy-rs/logs`.
pub fn log_dir() -> Option<PathBuf> {
    let dir = data_dir()?.join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir)
}

/// One item for the log writer: a line to append, or a barrier to run once
/// everything queued before it has been written.
enum LogInstruction {
    Line(String),
    /// Empty the file, ordered after every line queued so far. The callback runs
    /// once the truncate has happened, so a caller can wait for it.
    Truncate(Box<dyn FnOnce() + Send>),
    /// Test-only: see [`flush_log_writer`]. Never constructed in production, so
    /// the variant is gated to keep the lib build free of dead-code warnings.
    #[cfg(test)]
    Flush(Box<dyn FnOnce() + Send>),
}

/// Single writer for the log file.
///
/// Mirrors the stats writer deliberately: one owner thread drains a queue, so
/// every line has exactly one writer and nothing has to coordinate a lock.
/// Callers only `send`, which never blocks on disk I/O — the previous design
/// had every request handler lock a mutex, write and flush inline, putting
/// synchronous file I/O on the async request path.
fn log_writer() -> &'static Sender<LogInstruction> {
    static WRITER: OnceLock<Sender<LogInstruction>> = OnceLock::new();
    WRITER.get_or_init(|| {
        let (tx, rx) = channel::<LogInstruction>();
        std::thread::Builder::new()
            .name("log-writer".to_string())
            .spawn(move || writer_loop_log(rx))
            .expect("failed to spawn log writer thread");
        tx
    })
}

/// Drain the queue, appending lines and acting on barriers in order.
fn writer_loop_log(rx: Receiver<LogInstruction>) {
    let mut file: Option<(PathBuf, std::fs::File)> = None;

    while let Ok(first) = rx.recv() {
        // Coalesce a run of lines into one write, so a burst of requests costs
        // one syscall batch instead of one per line. A barrier ends the run, so
        // it observes everything queued before it.
        let mut batch = String::new();
        let mut pending = Some(first);

        // The instruction that stopped the run, if any: either an explicit
        // barrier or a truncate. `None` means the queue simply ran dry.
        let stop: Option<LogInstruction> = loop {
            match pending.take() {
                Some(LogInstruction::Line(line)) => {
                    if !batch.is_empty() {
                        batch.push('\n');
                    }
                    batch.push_str(&line);
                }
                Some(other) => break Some(other),
                None => match rx.try_recv() {
                    Ok(next) => pending = Some(next),
                    Err(_) => break None,
                },
            }
        };

        if !batch.is_empty() {
            batch.push('\n');
            write_batch(&mut file, &batch);
        }

        match stop {
            Some(LogInstruction::Truncate(done)) => {
                truncate_file(&mut file);
                done();
            }
            #[cfg(test)]
            Some(LogInstruction::Flush(done)) => done(),
            // `Line` can only be consumed by the loop above, and `None` is the
            // queue running dry.
            Some(LogInstruction::Line(_)) | None => {}
        }
    }
}

/// Empty the log file and drop the cached handle so the next append reopens it.
///
/// Writing the file directly from the caller instead would race the queue: a
/// line already sent but not yet flushed would land after the truncate and the
/// log would refill itself.
fn truncate_file(file: &mut Option<(PathBuf, std::fs::File)>) {
    if let Some((path, _)) = file.take() {
        let _ = std::fs::write(path, "");
    } else if let Some(path) = log_file_path() {
        let _ = std::fs::write(path, "");
    }
}

/// Append one coalesced batch, reopening the handle if the path changed.
fn write_batch(file: &mut Option<(PathBuf, std::fs::File)>, batch: &str) {
    use std::io::Write;
    let Some(path) = log_file_path() else {
        return;
    };
    // Reopen only when the target actually changed. In production it never
    // does; under test the override moves between cases, and a stale handle
    // would silently keep writing to the previous test's file.
    if file.as_ref().map(|(p, _)| p != &path).unwrap_or(true) {
        *file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()
            .map(|f| (path, f));
    }
    if let Some((_, f)) = file.as_mut() {
        // One write call per batch: this thread is the only writer, so no other
        // line can land in the middle of one.
        let _ = f.write_all(batch.as_bytes());
        // Flush per batch so a crash does not lose recent history.
        let _ = f.flush();
    }
}

fn append_log_line(line: &str) {
    // A dead writer must never fail a request; the in-memory buffer and the GUI
    // view do not depend on this mirror.
    let _ = log_writer().send(LogInstruction::Line(line.to_string()));
}

/// Block until every line queued so far has been written and flushed.
///
/// Production code never needs this — the buffer is the live view and the file
/// is a mirror. Tests do: appending is asynchronous, so asserting on file
/// contents right after a push is a race that passes only when the writer
/// thread happens to keep up. This is the deterministic counterpart to the poll
/// loop the stats tests use.
#[cfg(test)]
fn flush_log_writer() {
    let (tx, rx) = channel::<()>();
    if log_writer()
        .send(LogInstruction::Flush(Box::new(move || {
            let _ = tx.send(());
        })))
        .is_ok()
    {
        let _ = rx.recv_timeout(std::time::Duration::from_secs(5));
    }
}

/// Test hook: forces log mirroring to a temp file instead of the user's
/// `~/.proxy-rs/logs/proxy.log`. Re-settable so two tests can each redirect it.
static LOG_FILE_OVERRIDE: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);

/// Rolling log file: `~/.proxy-rs/logs/proxy.log`.
///
/// Under `cargo test` this defaults to a per-process temp file. Many unit tests
/// build a `LogBuffer` and push request-shaped lines through the real handlers;
/// without the redirect those synthetic lines append to the developer's real,
/// long-lived `~/.proxy-rs/logs/proxy.log`, which is both confusing and (for
/// tests that log credential-shaped values) a place secrets should never land.
/// Tests that need a specific file call [`set_log_file_override`], which still
/// wins.
pub fn log_file_path() -> Option<PathBuf> {
    if let Ok(guard) = LOG_FILE_OVERRIDE.lock() {
        if let Some(p) = guard.as_ref() {
            return Some(p.clone());
        }
    }
    if cfg!(test) {
        return Some(test_log_path());
    }
    Some(log_dir()?.join("proxy.log"))
}

/// Default log destination while running tests: one file per process, in the
/// temp directory, so parallel test binaries cannot share a file.
///
/// Not `#[cfg(test)]`-gated so `cfg!(test)` above still compiles in the normal
/// build; it is simply never called there.
fn test_log_path() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("proxy-rs-logs-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir.join("proxy.test.log")
}

/// Point log mirroring at `path` (tests only).
///
/// Only effective while no line has been written yet: the writer thread opens
/// its handle once and keeps it, so a later redirect cannot move an open file.
///
/// Exported (but hidden) so integration tests in `tests/` — which are separate
/// crates and cannot see `#[cfg(test)]` items — can keep their log lines out of
/// the developer's real `~/.proxy-rs/logs/proxy.log`. Without this an
/// integration test that exercises a logging handler silently appends its
/// synthetic credentials to a real, long-lived file.
#[doc(hidden)]
pub fn set_log_file_override_for_tests(path: PathBuf) {
    if let Ok(mut g) = LOG_FILE_OVERRIDE.lock() {
        *g = Some(path);
    }
}

#[cfg(test)]
fn set_log_file_override(path: PathBuf) {
    set_log_file_override_for_tests(path);
}

impl GuiSettings {
    pub fn load() -> Self {
        let path = settings_path();
        let mut settings = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).unwrap_or_default(),
            Err(_) => {
                // Migrate a legacy settings file from an old working directory.
                let legacy = std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join("gui-settings.json");
                match std::fs::read_to_string(&legacy) {
                    Ok(text) => {
                        let s: GuiSettings = serde_json::from_str(&text).unwrap_or_default();
                        let _ = std::fs::write(&path, serde_json::to_string_pretty(&s).unwrap());
                        s
                    }
                    Err(_) => Self::default(),
                }
            }
        };
        settings.port = if settings.port == 0 {
            DEFAULT_PORT
        } else {
            settings.port
        };
        settings
    }

    pub fn save(&self) -> Result<()> {
        let path = settings_path();
        let text = serde_json::to_string_pretty(self)?;
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Effective upstream chat completions URL.
    pub fn chat_url(&self, presets: &[crate::providers::ProviderPreset]) -> String {
        if !self.custom_url.trim().is_empty() {
            return normalize_chat_url(self.custom_url.trim());
        }
        if let Some(p) = presets.iter().find(|p| p.id == self.provider_id) {
            return p.chat_completions_url.clone();
        }
        // Unknown provider falls back to workbuddy-cn
        crate::providers::builtin_presets()[0]
            .chat_completions_url
            .clone()
    }

    /// Whether upstream requests must be sent as a stream.
    ///
    /// The GUI switch wins when set; otherwise the provider preset decides.
    /// A custom URL keeps plain semantics unless the user flips the switch or
    /// the runtime `11101` detector discovers it.
    pub fn force_stream(&self, presets: &[crate::providers::ProviderPreset]) -> bool {
        self.force_stream
            .unwrap_or_else(|| crate::providers::preset_force_stream(presets, &self.provider_id))
    }

    pub fn models_preset(&self) -> crate::providers::ProviderPreset {
        let presets = crate::providers::builtin_presets();
        presets
            .iter()
            .find(|p| p.id == self.provider_id)
            .cloned()
            .unwrap_or_else(|| presets[0].clone())
    }
}

/// Accepts base URL, versioned base, or full endpoint; returns full chat URL.
pub fn normalize_chat_url(input: &str) -> String {
    let trimmed = input.trim().trim_end_matches('/');
    if trimmed.ends_with("/chat/completions") {
        return trimmed.to_string();
    }
    // Versioned base like .../v2
    let last = trimmed.rsplit('/').next().unwrap_or("");
    if last.len() > 1
        && (last.starts_with('v') || last.starts_with('V'))
        && last[1..].chars().all(|c| c.is_ascii_digit())
    {
        return format!("{}/chat/completions", trimmed);
    }
    format!("{}/v1/chat/completions", trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Terminator for the long bodies in the concurrency test, so a line that
    /// got cut short is detectable rather than looking merely shorter.
    const END_MARKER: &str = " |end|";

    /// The log destination is process-global, so tests that assert on the file
    /// must not run concurrently with each other.
    static LOG_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Redirect log mirroring to a temp file so tests never touch (or fill)
    /// the user's real `~/.proxy-rs/logs/proxy.log`.
    ///
    /// Returns the temp path plus a guard held for the duration of the test.
    fn use_temp_log_file(name: &str) -> (std::path::PathBuf, std::sync::MutexGuard<'static, ()>) {
        let guard = LOG_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let dir = std::env::temp_dir().join(format!("proxy-rs-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = std::fs::remove_file(&path);
        set_log_file_override(path.clone());
        (path, guard)
    }

    #[test]
    fn normalize_full_url() {
        assert_eq!(
            normalize_chat_url("https://copilot.tencent.com/v2/chat/completions"),
            "https://copilot.tencent.com/v2/chat/completions"
        );
    }

    /// The data-directory override is process-global, so tests that set it must
    /// not run concurrently with each other.
    static DATA_DIR_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Run `body` with `PROXY_DATA_DIR` set to `value`, then restore
    /// whatever was there. Returns the guard so the caller keeps exclusivity.
    ///
    /// Env vars are process-wide, so a leaked value would change what every
    /// other test — and any `data_dir()` call in this one — resolves to.
    fn with_data_dir_env(
        value: Option<&str>,
    ) -> (
        std::sync::MutexGuard<'static, ()>,
        (Option<std::ffi::OsString>, Option<std::ffi::OsString>),
    ) {
        let guard = DATA_DIR_TEST_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let previous = std::env::var_os(DATA_DIR_ENV);
        let legacy_previous = std::env::var_os(LEGACY_DATA_DIR_ENV);
        std::env::remove_var(LEGACY_DATA_DIR_ENV);
        match value {
            Some(v) => std::env::set_var(DATA_DIR_ENV, v),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
        (guard, (previous, legacy_previous))
    }

    fn restore_data_dir_env(previous: (Option<std::ffi::OsString>, Option<std::ffi::OsString>)) {
        match previous.0 {
            Some(v) => std::env::set_var(DATA_DIR_ENV, v),
            None => std::env::remove_var(DATA_DIR_ENV),
        }
        match previous.1 {
            Some(v) => std::env::set_var(LEGACY_DATA_DIR_ENV, v),
            None => std::env::remove_var(LEGACY_DATA_DIR_ENV),
        }
    }

    /// The whole point of the override: settings, stats and logs move together.
    #[test]
    fn data_dir_env_relocates_the_whole_directory() {
        let (_guard, previous) = with_data_dir_env(Some("~/.proxy-rs-test-settings"));

        let home = std::env::var("HOME").unwrap();
        assert_eq!(
            data_dir().unwrap(),
            PathBuf::from(&home).join(".proxy-rs-test-settings")
        );
        // settings_path and dotenv_path both derive from data_dir, so they must
        // follow it rather than pointing back at the shared ~/.proxy-rs.
        assert_eq!(
            settings_path(),
            PathBuf::from(&home).join(".proxy-rs-test-settings/gui-settings.json")
        );
        assert_eq!(
            dotenv_path().unwrap(),
            PathBuf::from(&home).join(".proxy-rs-test-settings/.env")
        );
        assert_eq!(
            log_dir().unwrap(),
            PathBuf::from(&home).join(".proxy-rs-test-settings/logs")
        );

        restore_data_dir_env(previous);
    }

    /// An absolute path is used as given, without HOME expansion.
    #[test]
    fn data_dir_env_accepts_an_absolute_path() {
        let (_guard, previous) = with_data_dir_env(Some("/tmp/proxy-rs-abs"));

        assert_eq!(data_dir().unwrap(), PathBuf::from("/tmp/proxy-rs-abs"));

        restore_data_dir_env(previous);
    }

    /// Unset (and blank) must fall back to the default, so an exported-but-empty
    /// variable cannot silently relocate the app's state.
    #[test]
    fn blank_data_dir_env_falls_back_to_default() {
        let (_guard, previous) = with_data_dir_env(Some("   "));

        let home = std::env::var("HOME").unwrap();
        assert_eq!(data_dir().unwrap(), PathBuf::from(&home).join(".proxy-rs"));

        restore_data_dir_env(previous);
    }

    /// Without the override the default location is unchanged.
    #[test]
    fn unset_data_dir_env_uses_default() {
        let (_guard, previous) = with_data_dir_env(None);

        let home = std::env::var("HOME").unwrap();
        assert_eq!(data_dir().unwrap(), PathBuf::from(&home).join(".proxy-rs"));

        restore_data_dir_env(previous);
    }

    #[test]
    fn normalize_versioned_base() {
        assert_eq!(
            normalize_chat_url("https://gateway.example.com/v2/"),
            "https://gateway.example.com/v2/chat/completions"
        );
    }

    #[test]
    fn normalize_plain_base() {
        assert_eq!(
            normalize_chat_url("https://api.openai.com"),
            "https://api.openai.com/v1/chat/completions"
        );
    }

    #[test]
    fn concurrent_pushes_never_interleave_lines() {
        let (log_path, _guard) = use_temp_log_file("concurrent.log");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let expected = 40 * 50;
        rt.block_on(async {
            let buf = LogBuffer::new(expected + 500);
            let mut tasks = Vec::new();
            let buf = std::sync::Arc::new(buf);
            for i in 0..40 {
                let buf = buf.clone();
                tasks.push(tokio::spawn(async move {
                    for j in 0..50 {
                        // A long body makes a torn write obvious, and the
                        // trailing marker makes a truncated one obvious.
                        let body = format!("{}{}", "x".repeat(300), END_MARKER);
                        buf.push("INFO", format!("task {i} line {j} {body}")).await;
                    }
                }));
            }
            for t in tasks {
                let _ = t.await;
            }

            // The buffer is seeded from the log file, which other tests share,
            // so only this test's own lines are asserted on.
            let mine: Vec<_> = buf
                .snapshot()
                .await
                .into_iter()
                .filter(|e| e.message.starts_with("task "))
                .collect();
            assert_eq!(mine.len(), expected);
            for entry in &mine {
                let msg = &entry.message;
                assert!(msg.ends_with(END_MARKER), "truncated line: {msg:?}");
                assert!(msg.len() > 300, "shortened line: {msg:?}");
            }
        });

        // Appending is asynchronous, so wait for the queue to drain before
        // reading; otherwise this is a race that only passes when the writer
        // thread keeps up.
        flush_log_writer();

        // The real regression was on disk: every written line must carry
        // exactly one "[INFO] " marker and end with this test's body. A torn
        // line splices two writes together and therefore shows two markers.
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        let mine: Vec<&str> = text.lines().filter(|l| l.contains("task ")).collect();
        assert_eq!(mine.len(), expected, "expected every push to reach disk");
        for line in mine {
            assert_eq!(
                line.matches(" [INFO] ").count(),
                1,
                "torn log line: {line:?}"
            );
            assert!(line.ends_with(END_MARKER), "truncated log line: {line:?}");
        }
    }

    #[test]
    fn clear_empties_the_log_file_for_real() {
        // Regression: `clear` used to truncate the file directly while the
        // writer thread still had lines queued. Those landed after the
        // truncate, the file refilled, and "清空" looked like it did nothing.
        let (log_path, _guard) = use_temp_log_file("clear.log");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let buf = LogBuffer::new(500);
            for i in 0..30 {
                buf.push("INFO", format!("before clear {i}")).await;
            }

            // Clear immediately, with writes still in flight.
            buf.clear().await;

            assert!(buf.snapshot().await.is_empty(), "buffer must be emptied");

            // Anything queued after the clear must survive; anything queued
            // before it must not.
            for i in 0..5 {
                buf.push("INFO", format!("after clear {i}")).await;
            }
        });
        flush_log_writer();

        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        assert!(
            !text.contains("before clear"),
            "pre-clear lines came back: {text}"
        );
        assert_eq!(
            text.lines().filter(|l| l.contains("after clear")).count(),
            5,
            "post-clear lines must still be written: {text}"
        );
    }

    #[test]
    fn log_buffer_trims_to_capacity() {
        let (_path, _guard) = use_temp_log_file("capacity.log");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let buf = LogBuffer::new(3);
            for i in 0..10 {
                buf.push("INFO", format!("line {}", i)).await;
            }
            let snap = buf.snapshot().await;
            assert_eq!(snap.len(), 3);
            assert_eq!(snap[0].message, "line 7");
            assert_eq!(snap[2].message, "line 9");
        });
    }

    #[test]
    fn unset_force_stream_follows_the_provider_preset() {
        let presets = crate::providers::builtin_presets();
        let mut s = GuiSettings::default();
        assert_eq!(s.force_stream, None, "default must follow the preset");

        s.provider_id = "workbuddy-cn".to_string();
        assert!(s.force_stream(&presets));

        s.provider_id = "openai".to_string();
        assert!(!s.force_stream(&presets));

        // A provider with no preset (custom URL) keeps plain semantics.
        s.provider_id = "custom-gateway".to_string();
        assert!(!s.force_stream(&presets));
    }

    #[test]
    fn gui_switch_overrides_the_preset_in_both_directions() {
        let presets = crate::providers::builtin_presets();

        // Turn streaming ON for a provider that serves plain bodies.
        let mut s = GuiSettings {
            provider_id: "openai".to_string(),
            force_stream: Some(true),
            ..Default::default()
        };
        assert!(s.force_stream(&presets));

        // ...and OFF for one whose preset forces it.
        s.provider_id = "workbuddy-cn".to_string();
        s.force_stream = Some(false);
        assert!(!s.force_stream(&presets));
    }

    #[test]
    fn force_stream_survives_a_settings_round_trip() {
        let s = GuiSettings {
            force_stream: Some(true),
            ..Default::default()
        };
        let text = serde_json::to_string(&s).unwrap();
        let back: GuiSettings = serde_json::from_str(&text).unwrap();
        assert_eq!(back.force_stream, Some(true));

        // Older settings files have no such key: they must load as "follow the
        // preset" rather than failing to deserialize.
        let legacy: GuiSettings = serde_json::from_str(
            r#"{"provider_id":"workbuddy-cn","custom_url":"","api_key":"","port":3456,"bind":"127.0.0.1"}"#,
        )
        .unwrap();
        assert_eq!(legacy.force_stream, None);
        assert!(legacy.force_stream(&crate::providers::builtin_presets()));
    }
}
