//! The state the admin surfaces share.
//!
//! ## Why this lives apart from the routes
//!
//! `admin_state` used to sit inside [`crate::admin_routes`], while the
//! transport-agnostic implementation layer ([`crate::admin`]) took
//! `Arc<AdminState>` in three signatures. That made the two modules import each
//! other — a real cycle, not a cosmetic one: neither could be read, tested or
//! moved without the other.
//!
//! State is not a route. It is the small, stable set of handles every admin
//! surface needs (sessions, stats, logs, the inbound gate, the credential pool,
//! process info), and putting it in its own module makes the dependency
//! direction one-way: routes and implementations both point at the state, and
//! the state points at neither.

use std::sync::Arc;

use serde_json::{json, Value};

use crate::admin_auth::AdminSessions;
use crate::client_accounts::{self, ClientAuthState};
use crate::settings::{self, GuiSettings, LogBuffer};
use crate::stats::StatsDb;
use crate::workbuddy_auth;

/// Read-only view of the running process, for the console's status strip.
///
/// A struct rather than reading the environment at render time: the values must
/// all describe *the same moment*, and a payload assembled from three separate
/// reads can straddle a restart.
#[derive(Debug, Clone)]
pub struct ProcessInfo {
    pub version: &'static str,
    /// The port the gateway is listening on, read at call time.
    ///
    /// A live reference rather than a snapshot: the desktop binds its port
    /// *after* the admin state is built (the state is built first, so the
    /// console exists even when the bind fails), and a snapshot taken then
    /// would report the configured port instead of the one actually in use —
    /// which is how the console came to show "3456" while serving on 3457.
    ///
    /// The server build passes a fixed value (its port never changes after
    /// start), so the shape is the same for both.
    pub bound_port: std::sync::Arc<std::sync::atomic::AtomicU16>,
    pub started_at: std::time::Instant,
    pub bind_addr: String,
}

impl ProcessInfo {
    /// The port the gateway is listening on right now.
    pub fn port(&self) -> u16 {
        self.bound_port.load(std::sync::atomic::Ordering::SeqCst)
    }
}

impl ProcessInfo {
    /// The status payload the console's header renders.
    pub fn status_json(&self) -> Value {
        let settings = GuiSettings::load();
        let presets = crate::providers::builtin_presets();
        json!({
            // A headless gateway has no start/stop control — it is always
            // "running" if this code is answering at all. Reporting `true`
            // rather than omitting the field keeps the console's shape aligned
            // with the desktop payload.
            "running": true,
            "service_running": true,
            // Read by the shared front-end to label the process lifecycle. The
            // console cannot hide the start/stop button (that file is shared
            // with the desktop app), so clicking it surfaces the bridge's
            // explanation instead of a silent no-op.
            "managed_by": "systemd",
            "can_control_service": false,
            "uptime_secs": self.started_at.elapsed().as_secs(),
            "version": self.version,
            "provider": settings.provider_id,
            "upstream_url": settings.chat_url(&presets),
            "config_path": settings::settings_path().display().to_string(),
            "log_path": settings::log_file_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            // Desktop-only fields the shared front-end reads. They are answered
            // honestly rather than omitted: a missing key renders as the empty
            // string in a `<code>` element, which looks like a bug, while an
            // explicit "由 systemd 托管" reads as the truth.
            "launch_at_login": false,
            "launch_at_login_stale": false,
            "managed_by": "systemd",
            "port": self.port(),
            "configured_port": settings.port,
            "bind": self.bind_addr,
            "client_auth_enabled": client_accounts::enabled_from(settings.client_auth_enabled),
            "data_dir": settings::data_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            // Whether an upstream key is configured, so the settings form can say
            // "already set, leave blank to keep" instead of looking empty.
            "api_key_set": !settings.api_key.trim().is_empty(),
            "env_path": settings::dotenv_path().map(|p| p.display().to_string()),
            // **The overview's 剩余积分 / 当前账号 cards read this.**
            //
            // It was missing from the server payload, so on the web console those
            // two cards rendered empty forever while the desktop app showed them
            // — the same front-end, a hole in the data behind it. The shape must
            // match the desktop command exactly (`id`/`label`/`points`).
            "current_identity": current_identity_summary(),
            // `is_dev` gates a couple of desktop-only affordances; a server is
            // never a dev build.
            "is_dev": false,
            "configured_launch_at_login": false,
            "launch_at_login_plist_present": false,
        })
    }
}

/// State the admin handlers need.
///
/// A distinct type from the data plane's extensions on purpose: the admin surface
/// has no `Config` (it does not forward anything) and no credential pool *for
/// forwarding* — it reads the pool only to report sticky-session counts.
pub struct AdminState {
    pub sessions: Arc<AdminSessions>,
    pub stats: Arc<StatsDb>,
    pub logs: Arc<LogBuffer>,
    pub client_auth: Arc<ClientAuthState>,
    pub pool: crate::session_pool::SharedCredentialPool,
    pub process: ProcessInfo,
    /// Outbound HTTP client, shared with the proxy so the console's occasional
    /// refreshes (points, model list, upstream test) reuse the same connection
    /// pool instead of opening a second one per click.
    pub client: reqwest::Client,
    /// Whether this state belongs to the **desktop app's in-process console**.
    ///
    /// Only a desktop-built state exposes `GET /admin/api/desktop-boot`, the
    /// endpoint that mints a session without a token exchange (the desktop
    /// process *is* the operator, so there is nobody to type a token in). A
    /// server build keeps it `false` and the endpoint answers 404 — the route
    /// does not exist for a remote operator, and nothing about the server
    /// deployment's trust model changes. See `console::desktop_boot`.
    pub desktop: bool,
    /// Growth-run progress lines, polled by the console instead of pushed as
    /// events.
    ///
    /// The desktop used to deliver these over Tauri events; a shared HTTP
    /// console has no push channel, and the payload is small and append-only,
    /// so a buffer the UI drains on a poll is the simplest faithful
    /// replacement. Writers append during a run; the reader takes-and-clears.
    /// Empty in steady state.
    pub growth_progress: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

/// The identity currently in force, for the overview header's three cards.
///
/// Resolved through `effective_default_identity` rather than the stored
/// preference: with no explicit default the first usable entry wins, and the
/// card must describe what is *actually* serving traffic.
pub fn current_identity_summary() -> Value {
    let id = workbuddy_auth::effective_default_identity();
    let (label, points) = match id.as_str() {
        id if id.starts_with("wb-") => workbuddy_auth::load_credentials()
            .into_iter()
            .find(|c| c.id == id)
            .map(|c| (c.label, c.points))
            .unwrap_or_else(|| ("未知账号".to_string(), None)),
        id if id.starts_with("k-") => workbuddy_auth::load_api_keys()
            .into_iter()
            .find(|k| k.id == id)
            .map(|k| (k.label, k.points))
            .unwrap_or_else(|| ("未知密钥".to_string(), None)),
        _ => ("静态 API Key".to_string(), None),
    };
    json!({ "id": id, "label": label, "points": points })
}
