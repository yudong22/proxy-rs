//! Headless `proxy-rs` gateway — the same router as the desktop app, without Tauri.
//!
//! ## Why this exists
//!
//! The desktop app (`src-tauri/src/main.rs`) is the primary front-end, but it
//! cannot run on a server: it needs a graphical session for its window and tray
//! icon, WebView2 on Windows, and it exits when the login session ends. A Linux
//! box has none of those and would not keep it alive anyway. [`crate::router`]
//! was written to be "free of GUI types so a headless front-end can reuse it
//! unchanged"; this binary is that front-end.
//!
//! It contains **no** Tauri dependency, so the release artifact is a single
//! self-contained executable. Everything it needs is configuration:
//!
//! * `~/.proxy-rs/gui-settings.json` — provider, port, bind, model map. Read by
//!   [`GuiSettings::load`], exactly like the desktop app, so an existing
//!   installation's settings, credentials and `stats.db` carry over untouched.
//! * `~/.proxy-rs/.env` — the same keys the desktop app honours, plus the
//!   server-only ones (`PROXY_BIND`, `PROXY_CLIENT_AUTH`, `PROXY_API_TOKEN`).
//! * `PROXY_DATA_DIR` — relocate the whole state directory (settings, stats,
//!   logs, `.env` together), which is how a deployment keeps state out of a
//!   home directory that belongs to another user.
//!
//! ## What it deliberately does not do
//!
//! There is no hot reload, no tray, no start/stop control and no GUI command
//! surface. A server's configuration changes by editing the file and restarting
//! the unit — that is the interface systemd already provides, and a second way
//! to mutate settings would mean a second copy of the GUI's validation logic.
//!
//! The three background loops (daily check-in, login-state scan, growth tasks)
//! *are* started here, because they belong to the account pool rather than to
//! the window, and a server that holds WorkBuddy credentials must maintain them
//! or their tokens silently age out.

use proxy_rs::{
    admin, admin_auth, admin_routes, console, metrics, router, scheduler, service,
    settings::{self, GuiSettings, LogBuffer, DEFAULT_PORT},
    Config, StatsDb,
};

use anyhow::{Context, Result};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Build the outbound HTTP client, identically to the desktop app.
///
/// The idle read timeout (rather than a total one) is load-bearing: a total
/// timeout kills any upstream SSE stream that outlives it, cutting long
/// Codex/Claude turns off mid-response. The read timeout only fires when the
/// upstream stops sending, which is the stall actually worth aborting.
fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .read_timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(10)
        .build()
        .unwrap_or_default()
}

/// Migrate the legacy single `api_key` field into the key pool.
///
/// A server that starts from a `gui-settings.json` copied off a desktop install
/// has the key in the old field and nothing in `api-keys.json`; without this the
/// gateway would come up with no upstream credential at all. Mirrors the desktop
/// app's startup step, including the dedupe: the id is content-derived, so a key
/// already in the pool is left alone.
fn migrate_legacy_api_key(settings: &GuiSettings) {
    let key = settings.api_key.trim();
    if key.is_empty() {
        return;
    }
    let id = proxy_rs::workbuddy_auth::api_key_id(key);
    let mut items = proxy_rs::workbuddy_auth::load_api_keys();
    if items.iter().any(|k| k.id == id) {
        return;
    }
    items.push(proxy_rs::workbuddy_auth::ApiKeyEntry {
        id,
        label: "默认 API Key".to_string(),
        key: key.to_string(),
        enabled: true,
        points: None,
        points_fetched_at_ms: None,
    });
    if let Err(e) = proxy_rs::workbuddy_auth::save_api_keys(&items) {
        eprintln!("proxy-rs-server: could not migrate legacy api_key: {e}");
    }
}

/// Report every background task that ends, so a silently dead loop is visible.
///
/// The desktop app routes this to the GUI log; here it goes to stderr, which
/// systemd captures into the journal. A loop returning early is a *warning* (it
/// is allowed to end), a panic is an error — the distinction matters because a
/// panic in a task is otherwise completely invisible.
fn spawn_supervised(
    logs: Arc<LogBuffer>,
    label: &'static str,
    fut: impl std::future::Future<Output = ()> + Send + 'static,
) {
    tokio::spawn(async move {
        match tokio::spawn(fut).await {
            Ok(()) => {
                logs.push("WARN", format!("后台任务 {} 已提前结束", label))
                    .await;
                tracing::warn!("background task {} returned early", label);
            }
            Err(join_err) => {
                let msg = format!("后台任务 {} 异常终止: {}", label, join_err);
                logs.push("ERROR", msg.clone()).await;
                tracing::error!("{}", msg);
            }
        }
    });
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialise tracing once. `RUST_LOG` is honoured, defaulting to `info`, and
    // the writer is stderr so systemd's journal owns rotation and timestamps
    // rather than the app inventing a second log file.
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();

    let settings = GuiSettings::load();
    let data_dir = settings::data_dir();

    // Re-key credentials to the stable account identity (uid) before anything
    // reads them, exactly as the desktop app does. Idempotent and silent when
    // there is nothing to change; a failure must not stop the server, because
    // the old token-hash ids remain usable — they just cannot survive a refresh.
    match proxy_rs::workbuddy_auth::migrate_credential_ids() {
        Ok(0) => {}
        Ok(n) => tracing::info!("re-keyed {n} credential(s) to stable account ids"),
        Err(e) => tracing::warn!("credential id migration failed (continuing): {e}"),
    }

    migrate_legacy_api_key(&settings);

    let cfg = Config::from_settings(&settings).context("invalid proxy configuration")?;
    let bind_addr = cfg.bind.clone();
    let port = if cfg.port == 0 {
        DEFAULT_PORT
    } else {
        cfg.port
    };

    // An inbound access-key gate is *off* by default in the desktop app, and that
    // default is load-bearing there. On a server reachable from the network the
    // opposite is true, so this binary requires the operator to have decided:
    // `PROXY_CLIENT_AUTH=true` in `.env` or the environment. It is not silently
    // forced on, because turning it on without any registered key would lock
    // every client out — the log line below is what tells an operator which
    // state they are in.
    let client_auth_enabled = proxy_rs::client_accounts::enabled_from(settings.client_auth_enabled);

    let client = build_client();

    let logs = Arc::new(LogBuffer::new(1000));
    let service_ctrl = service::ServiceController::new(false);
    let stats = StatsDb::open().unwrap_or_else(|e| {
        // An in-memory fallback keeps the gateway serving when the data
        // directory is unwritable; losing stats is far better than a dead
        // gateway, and the warning names the real cause.
        tracing::error!("failed to open stats.db: {e}; falling back to in-memory stats");
        StatsDb::in_memory().expect("in-memory SQLite failed")
    });

    // Credentials are read once at startup, ordered so the GUI-selected default
    // is first (the pool picks the first usable entry). A server has no GUI to
    // reorder them, so a restart is what applies a changed default.
    let credential_pool = Arc::new(proxy_rs::session_pool::CredentialPool::new(
        proxy_rs::workbuddy_auth::ordered_credentials(),
    ));
    let client_auth = Arc::new(proxy_rs::client_accounts::ClientAuthState::new(
        client_auth_enabled,
    ));

    let metrics_handle = metrics::install();

    // The server-mode admin console. The desktop app has no equivalent — it uses
    // Tauri IPC — and passing `None` there is what keeps its served bytes
    // identical to before this existed.
    //
    // Built before the listener binds so a broken admin router is a startup
    // failure, not a 404 that only shows up when somebody opens the console.
    let admin_sessions = Arc::new(admin_auth::AdminSessions::new());
    let admin_state = Arc::new(admin_routes::AdminState {
        sessions: admin_sessions.clone(),
        stats: stats.clone(),
        logs: logs.clone(),
        client_auth: client_auth.clone(),
        pool: credential_pool.clone(),
        process: admin::ProcessInfo {
            version: env!("CARGO_PKG_VERSION"),
            bound_port: port,
            started_at: std::time::Instant::now(),
            bind_addr: bind_addr.clone(),
        },
        client: client.clone(),
    });
    // The REST surface and the console share one `AdminState` (and therefore one
    // session map): logging in at the console must authorise the REST calls its
    // own bridge makes.
    let admin_router = admin_routes::build_admin_router(admin_state.clone())
        .merge(console::build_console_router(admin_state));

    let app_router = router::build_app_router(
        service_ctrl.clone(),
        logs.clone(),
        Arc::new(cfg),
        client.clone(),
        stats,
        metrics_handle,
        credential_pool,
        client_auth,
    );
    // Wrapped AFTER the data-plane router is built, so the admin surface sits
    // outside the data plane's permissive CORS layer. See `router::with_admin`.
    let app_router = router::with_admin(app_router, Some(admin_router));

    // Bind before starting the loops: a busy port is a hard failure the operator
    // must see immediately, and there is no fallback port — every client CLI is
    // pointed at this exact address, so silently binding `port + 1` breaks them
    // with no visible cause.
    let (listener, bound_port) = service::ServiceController::bind(&bind_addr, port, false)
        .await
        .with_context(|| {
            format!("cannot bind {bind_addr}:{port} (is another instance running?)")
        })?;
    service_ctrl.mark_running();

    let shutdown = CancellationToken::new();

    // The loops live for the whole process. They must run whether or not the
    // `/v1/*` listener is answering, so they are not tied to the server future.
    spawn_supervised(
        logs.clone(),
        "每日打卡",
        scheduler::run_daily_checkin(client.clone(), logs.clone(), CancellationToken::new()),
    );
    spawn_supervised(
        logs.clone(),
        "登录态巡检",
        scheduler::run_login_state_scanner(client.clone(), logs.clone(), CancellationToken::new()),
    );
    spawn_supervised(
        logs.clone(),
        "成长任务",
        scheduler::run_growth_tasks(client.clone(), logs.clone(), CancellationToken::new()),
    );

    logs.push(
        "INFO",
        format!("代理服务已在 http://{bind_addr}:{bound_port} 启动（headless）"),
    )
    .await;
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data_dir = %data_dir
            .as_ref()
            .map(|p| p.display().to_string())
            .unwrap_or_else(|| "<unknown>".into()),
        %bind_addr,
        bound_port,
        client_auth = client_auth_enabled,
        "proxy-rs headless gateway listening"
    );
    if !client_auth_enabled {
        tracing::warn!(
            "inbound access-key gate is OFF (PROXY_CLIENT_AUTH unset): anyone who can reach \
             {bind_addr}:{bound_port} can use the upstream credentials. Bind to 127.0.0.1 \
             behind a reverse proxy, or set PROXY_CLIENT_AUTH=true."
        );
    }

    // SIGTERM is what systemd sends on `stop`/`restart`; SIGINT covers a
    // foreground run. Both cancel the loops and let axum drain in-flight
    // requests, which matters because a half-written SSE stream is worse than a
    // refused connection: the client has already been told HTTP 200.
    let shutdown_for_signals = shutdown.clone();
    tokio::spawn(async move {
        let ctrl_c = async {
            let _ = tokio::signal::ctrl_c().await;
        };
        #[cfg(unix)]
        let terminate = async {
            match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
                Ok(mut sig) => {
                    sig.recv().await;
                }
                Err(e) => {
                    tracing::error!("cannot install SIGTERM handler: {e}");
                    // Never resolve: falling through would shut the gateway
                    // down immediately at startup.
                    std::future::pending::<()>().await;
                }
            }
        };
        #[cfg(not(unix))]
        let terminate = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => tracing::info!("SIGINT received, shutting down"),
            _ = terminate => tracing::info!("SIGTERM received, shutting down"),
        }
        shutdown_for_signals.cancel();
    });

    let server = axum::serve(listener, app_router)
        .with_graceful_shutdown(async move { shutdown.cancelled().await });

    if let Err(e) = server.await {
        logs.push("ERROR", format!("代理服务运行异常: {e}")).await;
        return Err(anyhow::anyhow!("server error: {e}"));
    }

    service_ctrl.mark_stopped();
    tracing::info!("proxy-rs headless gateway stopped");
    Ok(())
}
