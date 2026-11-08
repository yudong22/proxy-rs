//! Environment lookup with `.env` support.
//!
//! Split out of [`crate::config`] so that modules which need to read an
//! environment variable — `client_accounts` (the inbound-gate switch),
//! `api_token` (the console token) — do not have to depend on the whole proxy
//! configuration to do it. That dependency was both directions
//! (`config → client_accounts` to fold the switch, and
//! `client_accounts → config` to read it), which made the two modules
//! inseparable and untestable in isolation.
//!
//! The behaviour is unchanged: the same `.env` files, the same search order,
//! the same precedence.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Values parsed from the `.env` file, loaded once per process.
///
/// Deliberately *not* pushed into the process environment. `dotenvy::dotenv()`
/// exists to call `std::env::set_var`, which is unsound to run while other
/// threads read the environment — and [`crate::config::Config::from_settings`]
/// runs on a tokio worker on every proxy (re)start, i.e. on every settings save,
/// while request handlers concurrently read env (`PROXY_TZ_OFFSET_HOURS`, the
/// upstream keys). Parsing the file into a map keeps the documented `.env`
/// behaviour without mutating shared process state.
fn dotenv_values() -> &'static HashMap<String, String> {
    static VALUES: OnceLock<HashMap<String, String>> = OnceLock::new();
    VALUES.get_or_init(|| {
        // Under `cargo test` the same reasoning as `settings::log_file_path`
        // applies, only more so: a unit test that happens to read configuration
        // must not inherit the developer's `.env`. The repository's own `.env`
        // sits in the *first* candidate slot and usually contains a real
        // upstream port and key, so a unit test would silently run against
        // live-looking settings and depend on files outside the crate.
        // Integration tests (other crates) cannot see `cfg!(test)`, so they
        // close the same hole explicitly through
        // [`set_dotenv_override_for_tests`].
        if cfg!(test) {
            return HashMap::new();
        }
        load_dotenv_file()
    })
}

/// Test-only replacement for the whole `.env` search: `None` = no override (use
/// the real search order and the process-wide cache), `Some(None)` = read no file
/// at all, `Some(Some(path))` = read exactly that file.
type DotenvOverride = Option<Option<PathBuf>>;

/// Test-only `.env` override, process-global because the thing it replaces is.
///
/// `None` (the outer option) means "behave exactly like production".
static DOTENV_OVERRIDE: std::sync::Mutex<DotenvOverride> = std::sync::Mutex::new(None);

fn dotenv_override() -> DotenvOverride {
    DOTENV_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone()
}

/// The `.env` value for `name`, honouring the test override.
///
/// Split out of [`lookup`] because the override cannot return a `&'static` map:
/// it has to be consulted on every call (see
/// [`set_dotenv_override_for_tests`]).
fn dotenv_lookup(name: &str) -> Option<String> {
    match dotenv_override() {
        // No override: the production path, one parse per process.
        None => dotenv_values().get(name).cloned(),
        // Override: bypass the candidate table *and* the `OnceLock`. Parsing per
        // lookup is fine here — this branch only exists under test, where the
        // file is either absent or a few lines in a temp directory.
        Some(path) => path
            .as_ref()
            .and_then(|path| parse_dotenv(path))
            .and_then(|vars| vars.get(name).cloned()),
    }
}

/// Point `.env` lookups at `path`, or at nothing, for tests only.
///
/// Integration tests live in `tests/` — separate crates, so they cannot reach
/// `#[cfg(test)]` items — and they need two things this provides:
///
///   * the developer's real `~/.proxy-rs/.env` (or the repository's own `.env`,
///     which is the *first* candidate) must not leak in. A `PROXY_API_TOKEN`
///     there makes every "no token is configured" test authenticate instead, and
///     the failure reads as a logic bug rather than as leaked fixture data;
///   * the answer must be switchable between tests. `PROXY_DATA_DIR` already
///     relocates the data directory, so the candidate table would be redirected
///     too — but only if nothing read `.env` before the test set the variable,
///     and that is exactly what the process-wide cache below cannot promise.
///
/// ## Why not express this through the `OnceLock`
///
/// The cache exists for production: parse once, never repeat the IO. Tests need
/// the opposite — a different answer per case — and any scheme that "reloads"
/// the cache either needs interior mutation of a `&'static` map or a reset that
/// races whatever `lookup` happens to be running concurrently. Bypassing the
/// cache for the override keeps production's one-parse guarantee untouched: the
/// `OnceLock` is still the only thing ordinary lookups touch.
///
/// `None` is **not** "restore the real search order" — it is "read no file".
/// There is no way back to the candidate table on purpose: restoring it would
/// silently re-open the leak a test just closed, and a test binary must never
/// pick up a developer's `.env` by accident. Every test in a binary is expected
/// to call this (with `None`) before it reads configuration.
#[doc(hidden)]
pub fn set_dotenv_override_for_tests(path: Option<PathBuf>) {
    *DOTENV_OVERRIDE
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
}

/// Parse a `.env` file's contents into a map, first occurrence winning.
pub fn parse_dotenv(path: &std::path::Path) -> Option<HashMap<String, String>> {
    let iter = dotenvy::from_path_iter(path).ok()?;
    Some(iter.flatten().fold(HashMap::new(), |mut acc, (k, v)| {
        acc.entry(k).or_insert(v);
        acc
    }))
}

/// Read the first `.env` that exists, in the documented search order.
///
/// Uses `from_path_iter`, whose iterator only *parses* — the mutating
/// `set_var` calls live in the separate `load`/`load_override` methods, which
/// this never calls.
fn load_dotenv_file() -> HashMap<String, String> {
    let mut candidates = vec![PathBuf::from(".env")];
    // The documented location is `~/.proxy-rs/.env`, asked for through
    // `data_dir()` rather than by joining `home_dir()` here: `data_dir()` is the
    // one function that honours `PROXY_DATA_DIR`, so a relocated data directory
    // relocates its `.env` too. That is production-equivalent (with the variable
    // unset, `data_dir()` *is* `~/.proxy-rs`) and it is what lets a test that
    // redirects the data directory stop reading a developer's real `.env` as a
    // side effect. `dotenv_path()` returns `None` when there is no home at all.
    if let Some(path) = crate::settings::dotenv_path() {
        candidates.push(path);
    }
    // `home_dir` rather than `HOME`: Windows does not set `HOME`, and without
    // this the documented `~/.proxy-rs.env` location is never even probed
    // there.
    if let Some(home) = crate::settings::home_dir() {
        candidates.push(home.join(".proxy-rs.env"));
        candidates.push(home.join(".anthropic-proxy.env"));
    }
    candidates.push(PathBuf::from("/etc/proxy-rs/.env"));
    candidates.push(PathBuf::from("/etc/anthropic-proxy/.env"));

    candidates
        .iter()
        .find_map(|path| path.exists().then(|| parse_dotenv(path)).flatten())
        .unwrap_or_default()
}

/// Look up a configuration value: the `.env` file first, then the real process
/// environment.
///
/// `.env` wins because it is the more specific, project-local setting, and
/// because it is the documented override for `gui-settings.json`. The process
/// environment is still consulted, so a variable exported by the shell or set
/// by launchd keeps working.
pub fn lookup(name: &str) -> Option<String> {
    dotenv_lookup(name).or_else(|| std::env::var(name).ok())
}

/// A configuration value from `.env` or the process environment, verbatim.
pub fn value(name: &str) -> Option<String> {
    lookup(name)
}

/// Trimmed, non-empty configuration value, if set.
pub fn non_empty(name: &str) -> Option<String> {
    lookup(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// Truthy configuration flag (`1` or `true`, case-insensitive).
pub fn flag(name: &str) -> bool {
    lookup(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Environment variable parsed as a port number.
pub fn u16_of(name: &str) -> Option<u16> {
    non_empty(name).and_then(|v| v.parse().ok())
}
