//! The shared application context and tray menu item handles.

use crate::*;

pub(crate) struct AppContext {
    pub(crate) settings: Arc<RwLock<GuiSettings>>,
    pub(crate) logs: Arc<LogBuffer>,
    pub(crate) service_ctrl: Arc<service::ServiceController>,
    pub(crate) client: reqwest::Client,
    pub(crate) bound_port: Arc<AtomicU16>,
    pub(crate) started_at: std::time::Instant,
    pub(crate) stats: Arc<StatsDb>,
    /// Cancellation token for the currently-running proxy server task.
    pub(crate) server_shutdown: std::sync::Mutex<CancellationToken>,
    /// Monotonic id of the active server task; bumped on every (re)start.
    pub(crate) server_epoch: Arc<AtomicU16>,
    /// Upstream credential pool (WorkBuddy login states / multi-key). Rebuilt
    /// on every proxy (re)start so saved credential changes hot-apply.
    pub(crate) credential_pool: Arc<proxy_rs::session_pool::CredentialPool>,
    /// **Inbound** access-key gate (`x-proxy-rs-key`).
    ///
    /// Deliberately not rebuilt on restart: the GUI/HTTP CRUD commands mutate it
    /// in place and call `reload_from_store`, so a key created or disabled takes
    /// effect on the very next request. Rebuilding it per start would also reset
    /// the throttled `last_used_ms` bookkeeping for no benefit.
    pub(crate) client_auth: Arc<proxy_rs::client_accounts::ClientAuthState>,
    /// Console sessions for the desktop's in-process `/admin/` surface.
    ///
    /// Lives on the context (not rebuilt per listener start) so the webview's
    /// cookie survives the listener restarts that saving settings triggers.
    /// Sessions are minted by `desktop-boot` — there is no token login on the
    /// desktop, because the process itself is the operator.
    pub(crate) admin_sessions: Arc<proxy_rs::admin_auth::AdminSessions>,
    /// Growth-run progress lines, shared with the admin RPC layer (polled by
    /// the console instead of pushed as Tauri events).
    pub(crate) growth_progress: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
    /// Port the **console** listens on, separate from the proxy's own port.
    ///
    /// The console must outlive the proxy: "启动服务" lives on it, so if the
    /// two shared a listener, stopping the service would take away the button
    /// that starts it again. It gets its own port (`proxy port + 1`) and its
    /// own long-lived listener, started at boot and never stopped.
    pub(crate) console_port: Arc<std::sync::atomic::AtomicU16>,
}

pub(crate) struct TrayState {
    pub(crate) status_item: MenuItem<tauri::Wry>,
    pub(crate) start_item: MenuItem<tauri::Wry>,
    pub(crate) stop_item: MenuItem<tauri::Wry>,
}
