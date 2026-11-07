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
    VALUES.get_or_init(load_dotenv_file)
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
    // `home_dir` rather than `HOME`: Windows does not set `HOME`, and without
    // this the documented `~/.proxy-rs/.env` location is never even probed
    // there.
    if let Some(home) = crate::settings::home_dir() {
        candidates.push(home.join(".proxy-rs").join(".env"));
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
    dotenv_values()
        .get(name)
        .cloned()
        .or_else(|| std::env::var(name).ok())
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
