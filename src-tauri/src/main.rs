#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use proxy_rs::{
    launch_agent, metrics, providers, router, service,
    settings::{self, GuiSettings, LogBuffer, DEFAULT_PORT},
    Config, StatsDb,
};
use serde_json::{json, Value};
use std::sync::{atomic::AtomicU16, Arc};
use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State,
};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

struct AppContext {
    settings: Arc<RwLock<GuiSettings>>,
    logs: Arc<LogBuffer>,
    service_ctrl: Arc<service::ServiceController>,
    client: reqwest::Client,
    bound_port: Arc<AtomicU16>,
    started_at: std::time::Instant,
    stats: Arc<StatsDb>,
    /// Cancellation token for the currently-running proxy server task.
    server_shutdown: std::sync::Mutex<CancellationToken>,
    /// Monotonic id of the active server task; bumped on every (re)start.
    server_epoch: Arc<AtomicU16>,
    /// Upstream credential pool (WorkBuddy login states / multi-key). Rebuilt
    /// on every proxy (re)start so saved credential changes hot-apply.
    credential_pool: Arc<proxy_rs::session_pool::CredentialPool>,
    /// **Inbound** access-key gate (`x-proxy-rs-key`).
    ///
    /// Deliberately not rebuilt on restart: the GUI/HTTP CRUD commands mutate it
    /// in place and call `reload_from_store`, so a key created or disabled takes
    /// effect on the very next request. Rebuilding it per start would also reset
    /// the throttled `last_used_ms` bookkeeping for no benefit.
    client_auth: Arc<proxy_rs::client_accounts::ClientAuthState>,
    /// Console sessions for the desktop's in-process `/admin/` surface.
    ///
    /// Lives on the context (not rebuilt per listener start) so the webview's
    /// cookie survives the listener restarts that saving settings triggers.
    /// Sessions are minted by `desktop-boot` — there is no token login on the
    /// desktop, because the process itself is the operator.
    admin_sessions: Arc<proxy_rs::admin_auth::AdminSessions>,
    /// Growth-run progress lines, shared with the admin RPC layer (polled by
    /// the console instead of pushed as Tauri events).
    growth_progress: Arc<std::sync::Mutex<Vec<serde_json::Value>>>,
}

struct TrayState {
    status_item: MenuItem<tauri::Wry>,
    start_item: MenuItem<tauri::Wry>,
    stop_item: MenuItem<tauri::Wry>,
}

fn update_tray_state(app: &tauri::AppHandle, ctx: &AppContext) {
    let running = ctx.service_ctrl.is_running();
    let port = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst);

    if let Some(tray) = app.tray_by_id("main-tray") {
        let active_bytes = include_bytes!("../icons/tray_active.png");
        let stopped_bytes = include_bytes!("../icons/tray_stopped.png");
        let img_bytes: &[u8] = if running { active_bytes } else { stopped_bytes };
        if let Ok(img) = tauri::image::Image::from_bytes(img_bytes) {
            let _ = tray.set_icon(Some(img));
            let _ = tray.set_icon_as_template(false);
        }
        let tooltip = if running {
            format!("Proxy RS · 运行中 (端口: {})", port)
        } else {
            "Proxy RS · 已停止".to_string()
        };
        let _ = tray.set_tooltip(Some(tooltip));
    }

    if let Some(tray_state) = app.try_state::<TrayState>() {
        if running {
            let _ = tray_state
                .status_item
                .set_text(format!("🟢 状态: 运行中 (端口: {})", port));
            let _ = tray_state.start_item.set_enabled(false);
            let _ = tray_state.stop_item.set_enabled(true);
        } else {
            let _ = tray_state.status_item.set_text("⚪️ 状态: 已停止");
            let _ = tray_state.start_item.set_enabled(true);
            let _ = tray_state.stop_item.set_enabled(false);
        }
    }
}

/// Tell the console the service state changed.
///
/// Replaces the UI's 2-second `get_status` poll: the front-end listens for
/// `service-state` and re-reads status only when this fires. Every caller
/// already calls `update_tray_state` at the same moment — the tray and the
/// console show the same fact — so the two are kept adjacent deliberately.
fn emit_service_state(app: &tauri::AppHandle) {
    let _ = app.emit("service-state", ());
}

/// The identity currently serving requests, for the overview header.
///
/// Mirrors the resolution the request path applies
/// ([`proxy_rs::workbuddy_auth::effective_default_identity`]) and attaches a
/// human-readable label plus the last-known remaining points. `points` is
/// `null` when it has never been queried — distinct from a real 0 balance.
///
/// An account's label is preferred; a key's label next; the static-key path and
/// an explicitly empty pool fall back to a fixed description, because there is
/// no account to name in either case.
fn current_identity_summary() -> Value {
    let id = proxy_rs::workbuddy_auth::effective_default_identity();

    // (label, points) for the named identity, or a fallback when the id is the
    // static-key path / the "no pool" sentinel and there is no account to name.
    let (label, points) = match id.as_str() {
        id if id.starts_with("wb-") => proxy_rs::workbuddy_auth::load_credentials()
            .into_iter()
            .find(|c| c.id == id)
            .map(|c| (c.label, c.points))
            .unwrap_or_else(|| ("未知账号".to_string(), None)),
        id if id.starts_with("k-") => proxy_rs::workbuddy_auth::load_api_keys()
            .into_iter()
            .find(|k| k.id == id)
            .map(|k| (k.label, k.points))
            .unwrap_or_else(|| ("未知密钥".to_string(), None)),
        _ => ("静态 API Key".to_string(), None),
    };

    json!({ "id": id, "label": label, "points": points })
}

/// Run a fire-and-forget task under supervision, so its death is visible.
///
/// The two long-lived background tasks (the proxy listener and the daily
/// check-in scheduler) used to be spawned with their `JoinHandle` dropped on
/// the floor. A panic inside either one therefore vanished without a trace:
/// the scheduler's symptom was "my daily points stopped accruing" with nothing
/// anywhere to explain it, and a listener panic was worse than silent — the
/// service stayed flagged 🟢 running in the tray and console while the port
/// was actually closed.
///
/// This awaits the handle and reports both failure modes: a panic (the join
/// error) and an early return. Release builds are `panic = "abort"`, so a panic
/// takes the process with it; this is what makes the *non*-panic exits — and
/// any future non-abort build — observable.
fn spawn_supervised<F>(ctx: Arc<AppContext>, label: &'static str, fut: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    tauri::async_runtime::spawn(async move {
        match tauri::async_runtime::spawn(fut).await {
            Ok(()) => {
                ctx.logs
                    .push("WARN", format!("后台任务 {} 已提前结束", label))
                    .await;
                tracing::warn!("background task {} returned early", label);
            }
            Err(join_err) => {
                let msg = format!("后台任务 {} 异常终止: {}", label, join_err);
                ctx.logs.push("ERROR", msg.clone()).await;
                tracing::error!("{}", msg);
            }
        }
    });
}

/// Start (or restart) the proxy service and note it in the log. The server task
/// logs the real outcome once the port is bound, so a failed bind is never
/// reported as success here.
fn desktop_status(ctx: &Arc<AppContext>) -> Value {
    // `try_read`: the status poll must never block on a settings write in
    // flight (a save). The stale-by-milliseconds answer is fine for a header.
    let settings = match ctx.settings.try_read() {
        Ok(g) => g,
        Err(_) => return serde_json::json!({ "running": ctx.service_ctrl.is_running() }),
    };
    let service_running = ctx.service_ctrl.is_running();
    let port = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst);
    let presets = providers::builtin_presets();
    let upstream = settings.chat_url(&presets);

    json!({
        "running": service_running,
        "service_running": service_running,
        "uptime_secs": ctx.started_at.elapsed().as_secs(),
        "version": env!("CARGO_PKG_VERSION"),
        "provider": settings.provider_id,
        "upstream_url": upstream,
        "port": port,
        "configured_port": settings.port,
        "bind": settings.bind,
        "api_key_set": !settings.api_key.is_empty(),
        // Whether launchd actually has the job, not merely whether a plist file
        // is lying around. The two disagree after a user-initiated quit, which
        // deliberately leaves the plist on disk.
        "launch_at_login": launch_agent::is_loaded(),
        "configured_launch_at_login": settings.launch_at_login,
        "launch_at_login_path": launch_agent::plist_path().display().to_string(),
        "launch_at_login_plist_present": launch_agent::plist_exists(),
        "launch_at_login_stale": settings.launch_at_login
            && !launch_agent::is_loaded(),
        "data_dir": settings::data_dir().map(|d| d.display().to_string()),
        "env_path": settings::dotenv_path().map(|p| p.display().to_string()),
        "log_path": settings::log_file_path().map(|p| p.display().to_string()),
        // A debug build (what `tauri dev` produces) shares the installed app's
        // launchd label, so letting it write the login item would point the
        // user's next login at a development binary. The console disables the
        // switch on this flag.
        "is_dev": cfg!(debug_assertions),
        // The overview header's 配置厂商 / 当前账号 / 剩余积分 cards.
        "current_identity": current_identity_summary(),
    })
}

fn request_start(app: tauri::AppHandle, ctx: Arc<AppContext>) {
    start_proxy_server(app.clone(), ctx.clone());
    update_tray_state(&app, &ctx);
    emit_service_state(&app);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.eval("if (window.refreshStatus) { window.refreshStatus(); }");
    }
    tauri::async_runtime::spawn(async move {
        ctx.logs.push("INFO", "正在启动代理服务...".into()).await;
    });
}

/// Stop the proxy service and note it in the log.
fn request_stop(app: tauri::AppHandle, ctx: Arc<AppContext>) {
    stop_proxy_server(ctx.clone());
    update_tray_state(&app, &ctx);
    emit_service_state(&app);
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.eval("if (window.refreshStatus) { window.refreshStatus(); }");
    }
    tauri::async_runtime::spawn(async move {
        ctx.logs.push("INFO", "代理服务已停止".into()).await;
    });
}

// ── Desktop bootstrap IPC (the smallest surface that survives 3.4.0) ──────
//
// The webview now loads the in-process `/admin/` console over HTTP; these two
// commands (plus start/stop, dialogs and the tray) are what remains of the old
// 58-command IPC surface. `ui/bootstrap.html` uses them because it must reach
// the host *before* the gateway is listening — the console it opens cannot.

/// What the bootstrap page needs: whether the gateway is listening, and on
/// which port. Cheap and safe to poll.
#[tauri::command]
async fn get_bootstrap_info(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    // Give a just-issued start a moment to settle, mirroring `await_start_outcome`.
    let running = ctx.service_ctrl.is_running()
        || ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst) != 0;
    Ok(json!({
        "running": running,
        "port": ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// Wait briefly for the startup attempt to settle, so the caller can be told
/// whether the port was actually bound.
///
/// `start_proxy_server` is fire-and-forget: binding happens on a spawned task.
/// Reporting `ok: true` before that task has run is how a busy port used to be
/// presented to the UI as a success while the log said otherwise. Polling the
/// controller for a bounded window is enough, because the bind either succeeds
/// or fails within milliseconds on loopback.
async fn await_start_outcome(ctx: &Arc<AppContext>) -> (bool, u16) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(1500);
    while std::time::Instant::now() < deadline {
        if ctx.service_ctrl.is_running() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    let running = ctx.service_ctrl.is_running();
    let port = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst);
    (running, port)
}

#[tauri::command]
async fn start_service(
    app: tauri::AppHandle,
    ctx: State<'_, Arc<AppContext>>,
) -> Result<Value, String> {
    request_start(app, ctx.inner().clone());
    let (running, port) = await_start_outcome(ctx.inner()).await;
    if !running {
        // The server task has already logged the concrete reason (port busy,
        // invalid config) and the tray reflects "stopped". Tell the caller the
        // truth instead of a blanket success.
        return Err(format!(
            "代理服务启动失败（端口 {} 可能已被占用，请查看日志）",
            ctx.settings.read().await.port
        ));
    }
    Ok(json!({ "ok": true, "running": true, "port": port }))
}

#[tauri::command]
async fn stop_service(
    app: tauri::AppHandle,
    ctx: State<'_, Arc<AppContext>>,
) -> Result<Value, String> {
    request_stop(app, ctx.inner().clone());
    Ok(json!({ "ok": true, "running": false }))
}

// ── WorkBuddy credential pool management ───────────────────────────────────

/// Fold the legacy single `GuiSettings::api_key` into the key pool.
///
/// Called once at startup. The model-provider API Key field is removed from the
/// GUI (the key pool now owns all keys), but existing installs still have a key
/// there; importing it as a key entry means the unified 身份池 selector can
/// manage it and the proxy keeps using it. Existing keys take precedence: a key
/// whose material already exists is left untouched (its id is content-derived),
/// so we only add when the pool has nothing matching yet.
fn migrate_legacy_api_key(settings: &GuiSettings) {
    let key = settings.api_key.trim();
    if key.is_empty() {
        return;
    }
    let id = proxy_rs::workbuddy_auth::api_key_id(key);
    let items = proxy_rs::workbuddy_auth::load_api_keys();
    if items.iter().any(|k| k.id == id) {
        return;
    }
    let entry = proxy_rs::workbuddy_auth::ApiKeyEntry {
        id,
        label: "默认 API Key".to_string(),
        key: key.to_string(),
        enabled: true,
        points: None,
        points_fetched_at_ms: None,
    };
    let mut merged = items;
    merged.push(entry);
    let _ = proxy_rs::workbuddy_auth::save_api_keys(&merged);
}

// ── Upstream API key pool ──────────────────────────────────────────────────

/// Bring the main window to the front, restoring it if it was hidden.
fn focus_main_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}

/// Quit for real, surviving launch-at-login.
///
/// The LaunchAgent plist restarts the job when the process exits *unsuccessfully*
/// (`KeepAlive`/`SuccessfulExit=false`), so a plain `app.exit(0)` is enough to
/// stop the current run. Booting the job out as well is what makes "退出" mean
/// "do not come back": it removes launchd's record of the job, which would
/// otherwise relaunch it at the next login. The plist stays on disk so
/// launch-at-login is re-armed the next time the user starts the app with the
/// setting still enabled.
fn quit_app(app: &tauri::AppHandle) {
    if let Err(e) = launch_agent::suspend() {
        eprintln!("Warning: could not suspend launch-at-login before quitting: {e}");
    }
    app.exit(0);
}

/// Show the log directory in the OS file manager.
#[tauri::command]
fn open_logs_dir() {
    let Some(dir) = settings::log_dir() else {
        return;
    };
    reveal_in_file_manager(&dir);
}

/// Open a path in the OS file manager.
///
/// One `cfg` ladder for both callers (the log directory button and the export
/// success message). The macOS branch passes `-R` when the path is a file, which
/// selects it inside its folder instead of opening the file itself — a JSON
/// bundle is not something to hand to the default editor by accident.
fn reveal_in_file_manager(path: &std::path::Path) {
    #[cfg(target_os = "macos")]
    {
        let mut cmd = std::process::Command::new("open");
        if path.is_file() {
            cmd.arg("-R");
        }
        let _ = cmd.arg(path).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        // No portable "select the file" flag on Linux; opening its directory is
        // the closest equivalent.
        let dir = if path.is_file() {
            path.parent().unwrap_or(path)
        } else {
            path
        };
        let _ = std::process::Command::new("xdg-open").arg(dir).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(path).spawn();
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        let _ = path;
    }
}

// ── Client access accounts (inbound) ────────────────────────────────────────
//
// The GUI half of the same store the HTTP admin routes manage. Both go through
// `proxy_rs::client_accounts` and both reload the live state, so a change made
// in one front-end applies to the other without a restart.

// ── Account-pool import / export ───────────────────────────────────────────

/// Show an exported bundle in the OS file manager.
#[tauri::command]
fn reveal_path(path: String) {
    reveal_in_file_manager(std::path::Path::new(&path));
}

/// Monotonic id for the currently-running proxy server task. Incremented on
/// every (re)start so a stale `stop` request can't cancel a newer server.
fn next_server_id(ctx: &Arc<AppContext>) -> u16 {
    ctx.server_epoch
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        + 1
}

/// Spawn (or respawn) the proxy listener. A fresh `CancellationToken` governs
/// this specific server instance so it can be shut down independently of the
/// whole process. Errors are logged and the service is marked stopped.
fn run_proxy_server(
    app: tauri::AppHandle,
    ctx: Arc<AppContext>,
    server_id: u16,
    shutdown: tokio_util::sync::CancellationToken,
) {
    let ctx_for_watch = ctx.clone();
    spawn_supervised(ctx_for_watch, "代理监听", async move {
        // If a newer start was requested while we were spinning up, bail out.
        if ctx.server_epoch.load(std::sync::atomic::Ordering::SeqCst) != server_id {
            return;
        }

        let initial_settings = ctx.settings.read().await.clone();
        let cfg = match Config::from_settings(&initial_settings) {
            Ok(cfg) => cfg,
            Err(e) => {
                ctx.logs.push("ERROR", format!("代理配置无效: {}", e)).await;
                ctx.service_ctrl.mark_stopped();
                update_tray_state(&app, &ctx);
                emit_service_state(&app);
                return;
            }
        };

        let metrics_handle = metrics::install();
        let config_arc = Arc::new(cfg);

        let app_router = router::build_app_router(
            ctx.service_ctrl.clone(),
            ctx.logs.clone(),
            config_arc.clone(),
            ctx.client.clone(),
            ctx.stats.clone(),
            metrics_handle,
            ctx.credential_pool.clone(),
            ctx.client_auth.clone(),
        );
        // The desktop now serves the SAME `/admin/` console the server binary
        // does, and the webview loads it (see `ui/bootstrap.html`). The P1
        // duplication — ~50 Tauri commands reimplementing `admin_rpc` arms —
        // is the thing this deletes; the in-process console is what replaces
        // it. `desktop: true` is what makes `desktop-boot` mint the webview's
        // session (loopback only, see `console::desktop_boot`); the server
        // build keeps `desktop: false` and its token login untouched.
        let admin_state = Arc::new(proxy_rs::admin_routes::AdminState {
            sessions: ctx.admin_sessions.clone(),
            stats: ctx.stats.clone(),
            logs: ctx.logs.clone(),
            client_auth: ctx.client_auth.clone(),
            pool: ctx.credential_pool.clone(),
            process: proxy_rs::admin::ProcessInfo {
                version: env!("CARGO_PKG_VERSION"),
                // The live handle: the admin state is built before the port is
                // bound, so a snapshot here would report the configured port
                // instead of the one actually serving.
                bound_port: ctx.bound_port.clone(),
                started_at: ctx.started_at,
                bind_addr: config_arc.bind.clone(),
            },
            client: ctx.client.clone(),
            desktop: true,
            growth_progress: ctx.growth_progress.clone(),
        });
        let admin_router = proxy_rs::admin_routes::build_admin_router(admin_state.clone())
            .merge(proxy_rs::console::build_console_router(admin_state));
        let app_router = router::with_admin(app_router, Some(admin_router));

        let bind_addr = config_arc.bind.clone();
        // No `+1` fallback: every client CLI is pointed at one fixed gateway
        // URL, so binding a different port would break them invisibly. The
        // single-instance guard is what keeps this port free.
        let (listener, bound_port) = match service::ServiceController::bind(
            &bind_addr,
            config_arc.port,
            ctx.service_ctrl.allow_port_fallback(),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                let in_use = e.kind() == std::io::ErrorKind::AddrInUse;
                let hint = if in_use {
                    "（端口已被占用：请先退出另一个 Proxy RS 实例，或改用其它端口）"
                } else {
                    ""
                };
                ctx.logs
                    .push(
                        "ERROR",
                        format!("代理监听端口绑定失败 {}: {}{}", config_arc.port, e, hint),
                    )
                    .await;
                ctx.service_ctrl.mark_stopped();
                update_tray_state(&app, &ctx);
                emit_service_state(&app);
                return;
            }
        };

        // Another start may have superseded this one before we bound.
        if ctx.server_epoch.load(std::sync::atomic::Ordering::SeqCst) != server_id {
            let _ = listener.local_addr();
            return;
        }

        ctx.bound_port
            .store(bound_port, std::sync::atomic::Ordering::SeqCst);
        ctx.service_ctrl.mark_running();
        update_tray_state(&app, &ctx);
        emit_service_state(&app);
        ctx.logs
            .push(
                "INFO",
                format!("代理服务已在 http://{}:{} 启动", bind_addr, bound_port),
            )
            .await;

        // Peer-address extraction, same as the server binary: the desktop's
        // desktop-boot endpoint answers loopback requests only.
        let server = axum::serve(
            listener,
            app_router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { shutdown.cancelled().await });

        if let Err(e) = server.await {
            ctx.logs
                .push("ERROR", format!("代理服务运行异常: {}", e))
                .await;
        }

        // Only clear state if this is still the active server instance.
        if ctx.server_epoch.load(std::sync::atomic::Ordering::SeqCst) == server_id {
            ctx.service_ctrl.mark_stopped();
            update_tray_state(&app, &ctx);
            emit_service_state(&app);
        }
    });
}

/// Start (or restart) the proxy listener.
fn start_proxy_server(app: tauri::AppHandle, ctx: Arc<AppContext>) {
    // Already up: nothing to do.
    if ctx.service_ctrl.is_running() {
        return;
    }
    // The listener stays up while the service is stopped (it also serves the
    // console), so "start" after a "stop" is a **resume**: the port is already
    // bound and the routes are already mounted. Re-binding would fail with
    // "address in use" for a port we ourselves are holding.
    //
    // `bound_port` is the discriminator: it is non-zero exactly while a
    // listener exists. It is zero before the first bind and stays non-zero
    // after a stop (the port keeps answering 503).
    if ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst) != 0 {
        ctx.service_ctrl.mark_running();
        update_tray_state(&app, &ctx);
        emit_service_state(&app);
        return;
    }
    // No listener at all (first start): bind one.
    // Cancel any existing server and bump the epoch so stale tasks exit.
    if let Ok(guard) = ctx.server_shutdown.lock() {
        guard.cancel();
    }
    let server_id = next_server_id(&ctx);
    let shutdown = tokio_util::sync::CancellationToken::new();
    if let Ok(mut guard) = ctx.server_shutdown.lock() {
        *guard = shutdown.clone();
    }
    run_proxy_server(app, ctx, server_id, shutdown);
}

/// Stop the proxy listener entirely (graceful shutdown). Unlike `pause`, this
/// actually closes the port so the old 503 behavior is gone. The server task
/// refreshes the tray once it has fully shut down.
fn stop_proxy_server(ctx: Arc<AppContext>) {
    // **The listener stays up.** It also serves the console, so closing it
    // would take the "启动服务" button down together with the service it is
    // supposed to restart — the user could not start it again. The data plane
    // already refuses work while the service is stopped (`proxy.rs` answers
    // 503 `service_unavailable`), so marking the state is what "stopped"
    // means; the port keeps answering, and the console keeps answering with
    // the truth.
    //
    // `bound_port` is deliberately left alone: the port is still listening, and
    // it is what tells a later "start" that it should *resume* rather than try
    // to bind a port we already hold.
    ctx.service_ctrl.mark_stopped();
}

fn main() {
    let settings = GuiSettings::load();
    // Migrate the legacy single API Key field into the key pool so the unified
    // 身份池 selector can manage it alongside the other keys. This runs before
    // the proxy builds its config, so the migrated key is already in force. A
    // key that already exists in the pool (same material) is deduped by id and
    // simply re-enables/re-labels the existing entry.
    migrate_legacy_api_key(&settings);
    // Re-key credentials to the stable account identity (uid) before anything
    // reads them. Runs here, ahead of the pool build below, so the ids the pool
    // and the GUI see are already stable. Idempotent and silent when there is
    // nothing to change; a failure must not stop the app from starting, because
    // the old token-hash ids remain usable — they just cannot survive a refresh.
    match proxy_rs::workbuddy_auth::migrate_credential_ids() {
        Ok(0) => {}
        Ok(n) => eprintln!("proxy-rs: re-keyed {n} credential(s) to stable account ids"),
        Err(e) => eprintln!("proxy-rs: credential id migration failed (continuing): {e}"),
    }
    // No port fallback. Every client CLI is configured against one fixed
    // gateway URL, so silently binding a different port breaks them with no
    // visible cause — the busy port is surfaced as an error instead, and the
    // single-instance guard is what actually keeps the port free. A developer
    // who needs a second concurrent instance changes the configured port.
    let service_ctrl = service::ServiceController::new(false);

    // Whether launchd started this copy. Resolved *before* anything touches the
    // job, because the launchd copy must never re-register its own job: that
    // means `bootout`, and this process *is* the job.
    let is_launchd_child = launch_agent::running_as_launchd_child(std::env::args());
    // Deferred until the single-instance guard has accepted this process; see
    // the `.setup()` closure. Captured here because `settings` moves into `ctx`.
    let wants_launch_at_login = settings.launch_at_login;
    // Resolved before `settings` moves into `ctx`, and run through the same
    // env-aware helper the proxy config uses, so `PROXY_CLIENT_AUTH=true` in
    // `.env` wins over the GUI switch (a headless server has no GUI to click).
    let client_auth_enabled = proxy_rs::client_accounts::enabled_from(settings.client_auth_enabled);

    let client = reqwest::Client::builder()
        // Idle (read) timeout rather than a total one: a total timeout hard-kills
        // any upstream SSE stream that outlives it, cutting long Codex/Claude
        // turns off mid-response. The read timeout only fires when the upstream
        // stops sending data, which is the stall actually worth aborting.
        .read_timeout(std::time::Duration::from_secs(300))
        .connect_timeout(std::time::Duration::from_secs(10))
        .pool_max_idle_per_host(10)
        .build()
        .unwrap_or_default();

    let ctx = Arc::new(AppContext {
        settings: Arc::new(RwLock::new(settings)),
        logs: Arc::new(LogBuffer::new(1000)),
        service_ctrl,
        client,
        bound_port: Arc::new(AtomicU16::new(DEFAULT_PORT)),
        started_at: std::time::Instant::now(),
        stats: StatsDb::open().unwrap_or_else(|e| {
            eprintln!(
                "Warning: failed to open stats.db: {}; using in-memory fallback",
                e
            );
            // Fallback: in-memory DB so the app still starts without stats persistence.
            StatsDb::in_memory().expect("in-memory SQLite failed")
        }),
        server_shutdown: std::sync::Mutex::new(CancellationToken::new()),
        server_epoch: Arc::new(AtomicU16::new(0)),
        // Credentials are re-read on every proxy (re)start, so a credential
        // saved in the GUI hot-applies with the rest of the settings. Ordered:
        // the GUI-selected default goes first, which is what makes it the
        // default (the pool picks the first usable entry).
        credential_pool: Arc::new(proxy_rs::session_pool::CredentialPool::new(
            proxy_rs::workbuddy_auth::ordered_credentials(),
        )),
        // `desktop()`: the desktop console has no PROXY_API_TOKEN — the process
        // is the operator and `desktop-boot` mints its session in-process — so
        // the "no token → console disabled" rule must not apply to it.
        admin_sessions: Arc::new(proxy_rs::admin_auth::AdminSessions::desktop()),
        growth_progress: Arc::new(std::sync::Mutex::new(Vec::new())),
        client_auth: Arc::new(proxy_rs::client_accounts::ClientAuthState::new(
            client_auth_enabled,
        )),
    });

    let ctx_for_setup = ctx.clone();
    let ctx_for_tray = ctx.clone();
    let ctx_for_scheduler = ctx.clone();

    let mut builder = tauri::Builder::default();

    // Native file dialogs, used from Rust only (`pool_export`'s "save as"
    // panel). The plugin also ships webview-callable commands, but
    // `src-tauri/capabilities/default.json` grants no `dialog:` permission, so
    // the frontend cannot reach them: the panel is opened by this process and
    // the chosen path never passes through JS.
    builder = builder.plugin(tauri_plugin_dialog::init());

    // Registered for every copy, including the launchd child. The guard is what
    // keeps the proxy bound to the one fixed port the client CLIs are pointed
    // at, so exempting the launchd copy would leave the two copies unguarded
    // against each other. It is safe to apply to both: the plist's `KeepAlive`
    // only restarts the job after a *failed* exit, so a clean `exit(0)` from a
    // duplicate performs a single hand-off to the original instead of looping.
    builder = builder.plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
        focus_main_window(app);
        let ctx = app.state::<Arc<AppContext>>().inner().clone();
        // A second launch means "I could not find the window": if the
        // service was stopped, start it too, so the app always ends up in a
        // usable state.
        if !ctx.service_ctrl.is_running() {
            request_start(app.clone(), ctx.clone());
        }
        tauri::async_runtime::spawn(async move {
            ctx.logs
                .push(
                    "INFO",
                    format!("重复启动已合并到当前实例 (pid {})", std::process::id()),
                )
                .await;
        });
    }));

    builder
        .manage(ctx)
        .setup(move |app| {
            // Register the desktop's process-local capabilities with the RPC layer.
            //
            // The console page runs on `http://127.0.0.1:<port>/admin/` — an origin
            // the Tauri runtime does not inject IPC into — so the only way its
            // "启动/停止服务" button can reach this process is over HTTP, through the
            // RPC arms that call these hooks. A server build registers nothing and
            // answers with the systemd explanation instead.
            {
                // `setup` gets `&mut tauri::App`; the handle is what the tray and the
                // window helpers take.
                let app = app.handle().clone();
                let ctx_for_hooks = ctx_for_setup.clone();
                let app_start = app.clone();
                let ctx_start = ctx_for_hooks.clone();
                let app_stop = app.clone();
                let ctx_stop = ctx_for_hooks.clone();
                let ctx_status = ctx_for_hooks.clone();
                proxy_rs::desktop_hooks::register(proxy_rs::desktop_hooks::DesktopHooks {
                    start_service: Some(std::sync::Arc::new(move || {
                        request_start(app_start.clone(), ctx_start.clone());
                        Ok("代理服务已启动".to_string())
                    })),
                    stop_service: Some(std::sync::Arc::new(move || {
                        request_stop(app_stop.clone(), ctx_stop.clone());
                        Ok("代理服务已停止".to_string())
                    })),
                    open_logs_dir: Some(std::sync::Arc::new(|| match settings::log_dir() {
                        Some(dir) => {
                            reveal_in_file_manager(&dir);
                            Ok("已打开日志目录".to_string())
                        }
                        None => Err("未找到日志目录".to_string()),
                    })),
                    status: Some(std::sync::Arc::new(move || desktop_status(&ctx_status))),
                    reveal_path: Some(std::sync::Arc::new(|path: String| {
                        let p = std::path::PathBuf::from(&path);
                        if !p.exists() {
                            return Err(format!("路径不存在: {path}"));
                        }
                        reveal_in_file_manager(&p);
                        Ok(format!("已在文件管理器中显示: {path}"))
                    })),
                });
            }

            // Re-arm launch-at-login only once the single-instance guard has
            // accepted this process. Tauri initializes plugins during `build()`,
            // before this closure runs, so a duplicate has already exited by
            // now. Arming earlier would let the bootstrapped `RunAtLoad` copy
            // race this process for the singleton socket and win, leaving the
            // window the user just opened to exit instead.
            if launch_agent::should_rearm_at_startup(
                wants_launch_at_login,
                is_launchd_child,
                launch_agent::is_loaded(),
            ) {
                let _ = launch_agent::install();
            } else if wants_launch_at_login && is_launchd_child {
                // Must not touch the live job, but a plist written by an older
                // build can still carry the dangerous unconditional `KeepAlive`.
                // Refresh the file only; launchd reads it at the next load.
                let _ = launch_agent::rewrite_plist_if_stale();
            }

            let app_handle = app.handle().clone();
            start_proxy_server(app_handle.clone(), ctx_for_setup);

            // Daily check-in scheduler. Independent of the proxy listener: it
            // must run whether or not the local gateway is up, and it lives for
            // the whole process, so it gets its own cancellation token that is
            // never cancelled in practice (dropping it on exit stops the task).
            //
            // It must be spawned onto Tauri's async runtime rather than with
            // `tokio::spawn`: this `.setup()` closure runs on the main thread
            // with no Tokio reactor, and the scheduler's
            // `tokio::time::interval` needs one — spawning from there panicked
            // ("there is no reactor running") and, because a panic cannot
            // unwind through the Objective-C launch callback, aborted the app
            // at startup.
            spawn_supervised(
                ctx_for_scheduler.clone(),
                "每日打卡",
                proxy_rs::scheduler::run_daily_checkin(
                    ctx_for_scheduler.client.clone(),
                    ctx_for_scheduler.logs.clone(),
                    tokio_util::sync::CancellationToken::new(),
                ),
            );

            // Login-state scanner: renews access tokens before they age out, so
            // an account whose login state has no `expiresAt` (most imported
            // ones) is not left to fail a user request with a 401 first. Same
            // runtime requirement as the check-in scheduler above.
            spawn_supervised(
                ctx_for_scheduler.clone(),
                "登录态巡检",
                proxy_rs::scheduler::run_login_state_scanner(
                    ctx_for_scheduler.client.clone(),
                    ctx_for_scheduler.logs.clone(),
                    tokio_util::sync::CancellationToken::new(),
                ),
            );

            // Growth-task automation: opt-in, so it is spawned unconditionally
            // but does nothing until `growth_enabled` is set. Spawning it here
            // (rather than when the switch flips) keeps the runtime contract
            // identical to the other two loops.
            spawn_supervised(
                ctx_for_scheduler.clone(),
                "成长任务",
                proxy_rs::scheduler::run_growth_tasks(
                    ctx_for_scheduler.client.clone(),
                    ctx_for_scheduler.logs.clone(),
                    tokio_util::sync::CancellationToken::new(),
                ),
            );

            // Build Tray Menu
            let status_i =
                MenuItem::with_id(app, "status_label", "🟢 状态: 运行中", false, None::<&str>)?;
            let sep0 = PredefinedMenuItem::separator(app)?;
            let open_i = MenuItem::with_id(app, "open", "打开控制台", true, None::<&str>)?;
            let logs_i = MenuItem::with_id(app, "logs", "显示日志", true, None::<&str>)?;
            let sep1 = PredefinedMenuItem::separator(app)?;
            let start_i = MenuItem::with_id(app, "start", "启动代理服务", false, None::<&str>)?;
            let stop_i = MenuItem::with_id(app, "stop", "停止代理服务", true, None::<&str>)?;
            let sep2 = PredefinedMenuItem::separator(app)?;
            let quit_i = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

            let menu = Menu::with_items(
                app,
                &[
                    &status_i, &sep0, &open_i, &logs_i, &sep1, &start_i, &stop_i, &sep2, &quit_i,
                ],
            )?;

            app.manage(TrayState {
                status_item: status_i,
                start_item: start_i,
                stop_item: stop_i,
            });

            let tray_ctx = ctx_for_tray.clone();
            let active_bytes = include_bytes!("../icons/tray_active.png");
            let icon = tauri::image::Image::from_bytes(active_bytes).unwrap();
            let tray = TrayIconBuilder::with_id("main-tray")
                .icon(icon)
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(move |app, event| match event.id.as_ref() {
                    "open" => focus_main_window(app),
                    "logs" => {
                        focus_main_window(app);
                        if let Some(window) = app.get_webview_window("main") {
                            let _ =
                                window.eval("if (window.switchTab) { window.switchTab('logs'); }");
                        }
                    }
                    "start" => request_start(app.clone(), tray_ctx.clone()),
                    "stop" => request_stop(app.clone(), tray_ctx.clone()),
                    "quit" => quit_app(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if let TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } = event
                    {
                        focus_main_window(tray.app_handle());
                    }
                })
                .build(app)?;

            let _ = tray.set_icon_as_template(false);
            update_tray_state(&app_handle, &ctx_for_tray);

            Ok(())
        })
        .on_window_event(|window, event| {
            // Keep app alive in tray when window is closed
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            get_bootstrap_info,
            start_service,
            stop_service,
            open_logs_dir,
            reveal_path
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // On macOS, clicking the Dock icon after the window has been hidden
            // (closed into the tray) fires `Reopen`. Tauri does not auto-show the
            // window, so without this the only way back is the tray menu. Restore
            // it here so the Dock is a first-class way to reopen the console.
            //
            // macOS-only, and the `cfg` is load-bearing rather than cosmetic:
            // `RunEvent::Reopen` is declared `#[cfg(target_os = "macos")]` in
            // tauri, so referring to it on Windows is a hard compile error
            // (E0599: no variant named `Reopen`). The macOS Dock has no Windows
            // counterpart anyway — re-launching the exe goes through the
            // single-instance plugin, which focuses the window itself.
            #[cfg(target_os = "macos")]
            if let tauri::RunEvent::Reopen { .. } = event {
                focus_main_window(app);
            }
            // Other platforms: `event` is unused, so bind it away to keep the
            // closure warning-free under `-D warnings`.
            #[cfg(not(target_os = "macos"))]
            let _ = (app, event);
        });
}

#[cfg(test)]
mod tests {
    use super::resolve_api_key;

    /// Saving the settings form must never clear the stored key.
    ///
    /// Regression: the form has no `api_key` field since v1.8.5, so it posted an
    /// empty string on every save and the backend assigned it straight through —
    /// silently wiping a value `migrate_legacy_api_key` reads at startup. The
    /// damage was masked only because `gui-settings.json` kept the old key from
    /// before the field was removed.
    #[test]
    fn save_settings_keeps_the_stored_key_when_the_form_omits_it() {
        // Omitted / blank: keep what is stored.
        assert_eq!(resolve_api_key("", "ck-stored"), "ck-stored");
        assert_eq!(resolve_api_key("   ", "ck-stored"), "ck-stored");
    }

    /// A masked value is not a key: it is the UI echoing back a secret it was
    /// never given in the clear, so it must not overwrite the real one.
    #[test]
    fn save_settings_does_not_store_a_masked_placeholder() {
        assert_eq!(resolve_api_key("ck-••••wxyz", "ck-stored"), "ck-stored");
        assert_eq!(resolve_api_key("ck-****wxyz", "ck-stored"), "ck-stored");
    }

    /// A genuine new key still replaces the old one, trimmed.
    #[test]
    fn save_settings_accepts_a_real_replacement_key() {
        assert_eq!(resolve_api_key("ck-new", "ck-stored"), "ck-new");
        assert_eq!(resolve_api_key("  ck-new  ", "ck-stored"), "ck-new");
        // No stored key and nothing posted stays empty rather than erroring.
        assert_eq!(resolve_api_key("", ""), "");
    }
}
