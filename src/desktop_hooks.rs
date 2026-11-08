//! Process-local hooks for the desktop app's own capabilities.
//!
//! ## Why this exists
//!
//! The desktop console is served over HTTP by the gateway itself, and the
//! webview loads it from `http://127.0.0.1:<port>/admin/` — an origin the
//! Tauri runtime does **not** inject its IPC into (a page on that origin has
//! no `window.__TAURI__`; only `tauri://localhost` gets one). So the desktop
//! cannot reach its own native capabilities over Tauri IPC from the console
//! page, and cannot reach them over HTTP either — there is no HTTP handler
//! that can call `std::process::Command`.
//!
//! This module closes that gap without a second transport: the desktop
//! **registers** its capabilities here at startup, and the RPC layer calls
//! them in-process. The request still arrives over HTTP (one wire format, one
//! authorisation path); only the *last hop* is a direct function call rather
//! than a message.
//!
//! ## What is registered
//!
//! Exactly the five capabilities that are process/OS state rather than
//! business logic:
//!
//! * start / stop the service (they control the very listener that serves the
//!   console — a self-locking call over HTTP would need a separate channel)
//! * restart the service, which is how a saved setting reaches the running
//!   router (`Config` is a snapshot taken at listener build time)
//! * open the log directory / reveal a path in the file manager
//!
//! Everything else (settings, credentials, sessions, growth, OAuth, …) is
//! ordinary business logic and lives in [`crate::admin_rpc`] for **both**
//! builds — which is the point of the whole exercise: one implementation, not
//! two.
//!
//! ## The server build
//!
//! Registers nothing. The RPC arms then answer with the "systemd owns the
//! lifecycle" explanation, which is the truth for a headless deployment.

use std::sync::{Mutex, OnceLock};

pub type SimpleHook = std::sync::Arc<dyn Fn() -> Result<String, String> + Send + Sync>;
pub type ArgHook = std::sync::Arc<dyn Fn(String) -> Result<String, String> + Send + Sync>;

/// The desktop capabilities the RPC layer can invoke.
#[derive(Clone, Default)]
pub struct DesktopHooks {
    /// Start the proxy listener. Returns a human-readable outcome.
    pub start_service: Option<SimpleHook>,
    /// Stop the proxy listener.
    pub stop_service: Option<SimpleHook>,
    /// Rebuild the **running** proxy listener from the settings just saved.
    ///
    /// `Config` is a snapshot taken when the listener is built
    /// (`Config::from_settings`), so everything derived from settings — the
    /// upstream URL, `model_map`, `sanitize_terms`, `force_stream`,
    /// `intercept_safety_classifier` — only exists inside that snapshot. Saving
    /// the file therefore changes nothing until the listener is rebuilt, and
    /// this hook is what rebuilds it.
    ///
    /// Why a hook rather than something in this crate: only the desktop owns a
    /// restartable listener. A headless server registers nothing, because
    /// systemd is what restarts it (`systemctl restart proxy-rs`), and its
    /// console says so.
    ///
    /// A `None` answer means "this build has no restartable listener", which the
    /// caller reports with the systemd wording rather than as an error: the save
    /// itself succeeded either way.
    pub restart_service: Option<SimpleHook>,
    /// Open the log directory in the OS file manager.
    pub open_logs_dir: Option<SimpleHook>,
    /// Reveal a path in the OS file manager.
    pub reveal_path: Option<ArgHook>,
    /// The console's status payload, built by the desktop host.
    ///
    /// A server reports a fixed shape (`managed_by: "systemd"`, always
    /// running). The desktop's truth is different and dynamic: whether the
    /// service is running right now, whether launchd owns the login item, and
    /// whether this is a dev build that must not write one. The desktop
    /// registers this and the RPC layer prefers it — that is what lets
    /// `get_status` exist in **one** place instead of two.
    pub status: Option<StatusHook>,
}

/// A hook that produces the console status payload.
pub type StatusHook = std::sync::Arc<dyn Fn() -> serde_json::Value + Send + Sync>;

static HOOKS: OnceLock<Mutex<Option<DesktopHooks>>> = OnceLock::new();

fn slot() -> &'static Mutex<Option<DesktopHooks>> {
    HOOKS.get_or_init(|| Mutex::new(None))
}

/// Register the desktop capabilities. Called once, at desktop startup.
///
/// Overwrites any previous registration, which is what a restart of the
/// in-process listener implies.
pub fn register(hooks: DesktopHooks) {
    let mut guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    *guard = Some(hooks);
}

/// Forget the desktop capabilities.
///
/// Used when the desktop tears its listener down, so a later RPC cannot reach
/// a capability whose process state is gone.
pub fn unregister() {
    let mut guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    *guard = None;
}

/// Whether the desktop registered its capabilities.
pub fn is_registered() -> bool {
    slot().lock().unwrap_or_else(|p| p.into_inner()).is_some()
}

/// Invoke one hook, or report that this build has none.
///
/// The error text names the systemd alternative, because that is what a server
/// operator should be told — and it is also what the desktop's own UI shows
/// when the capability is genuinely unavailable.
pub fn call(
    what: &str,
    f: impl FnOnce(
        &DesktopHooks,
    ) -> Option<std::sync::Arc<dyn Fn() -> Result<String, String> + Send + Sync>>,
) -> Result<String, String> {
    let guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    let Some(registered) = guard.as_ref() else {
        return Err(format!(
            "服务器模式下服务由 systemd 托管，请用 systemctl {what} proxy-rs"
        ));
    };
    match f(registered) {
        Some(h) => h(),
        None => Err(format!(
            "服务器模式下服务由 systemd 托管，请用 systemctl {what} proxy-rs"
        )),
    }
}

/// The desktop's status payload, or `None` when this build has none (a server).
pub fn status() -> Option<serde_json::Value> {
    let guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    guard.as_ref().and_then(|h| h.status.as_ref()).map(|f| f())
}

/// The same, for the hook that takes an argument (`reveal_path`).
pub fn call_with_arg(
    what: &str,
    arg: String,
    f: impl FnOnce(
        &DesktopHooks,
    )
        -> Option<std::sync::Arc<dyn Fn(String) -> Result<String, String> + Send + Sync>>,
) -> Result<String, String> {
    let guard = slot().lock().unwrap_or_else(|p| p.into_inner());
    let Some(registered) = guard.as_ref() else {
        return Err(format!(
            "服务器模式下服务由 systemd 托管，请用 systemctl {what} proxy-rs"
        ));
    };
    match f(registered) {
        Some(h) => h(arg),
        None => Err(format!(
            "服务器模式下服务由 systemd 托管，请用 systemctl {what} proxy-rs"
        )),
    }
}
