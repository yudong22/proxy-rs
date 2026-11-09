#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! The Tauri desktop shell.
//!
//! ## Split note (3.4.1)
//!
//! `main.rs` was 1,080 lines: the app context, tray wiring, five IPC commands,
//! the two listener lifecycles, and `main()`'s setup/build closures. They are now
//! one file per concern under `app/`. The IPC command surface itself is
//! **unchanged** — `tauri::generate_handler!` still lists exactly the five
//! host-process commands, which `tests/admin_api.rs` pins. Everything except
//! `main()` is re-exported here so that guard and the `#[tauri::command]`
//! registration keep resolving without qualification.

mod app {
    pub mod context;
    pub mod lifecycle;
    pub mod server;
    pub mod tray;
}

// Re-exported flat: `generate_handler![get_bootstrap_info, …]` and the tray code
// refer to these by bare name, exactly as they did when everything was in one
// file.
use app::context::*;
use app::lifecycle::*;
use app::server::*;
use app::tray::*;

// Shared imports, re-exported to the `app` submodules through `use super::*`.
// `pub(crate) use` rather than plain `use`: a private import is not visible to a
// child module, so the submodules could not see `Arc`, `json!`, `State`… without
// re-importing every one of them individually.
pub(crate) use proxy_rs::{
    launch_agent, metrics, providers, router, service,
    settings::{self, GuiSettings, LogBuffer, DEFAULT_PORT},
    Config, StatsDb,
};
pub(crate) use serde_json::{json, Value};
pub(crate) use std::sync::{atomic::AtomicU16, Arc};
pub(crate) use tauri::{
    menu::{Menu, MenuItem, PredefinedMenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State,
};
pub(crate) use tokio::sync::RwLock;
pub(crate) use tokio_util::sync::CancellationToken;

// ── IPC commands ────────────────────────────────────────────────────────────
//
// These five live in `main.rs` rather than `app/commands.rs` on purpose: the
// `#[tauri::command]` macro emits helper macros (`__cmd__<name>`) in the module
// that *defines* the function, and `generate_handler!` resolves them in the
// module that *registers* it. Splitting them across files fails to compile with
// "cannot find macro `__cmd__get_bootstrap_info`" — so the definition site is
// pinned here by the framework, not by preference. Everything these commands
// call lives in `app/`.
//
// Exactly five: start/stop the process, read the bootstrap info, and two OS file
// manager actions. Anything else belongs in `admin_rpc` (see the guard test
// `the_desktop_ipc_surface_is_only_the_five_host_process_commands`).
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
    let bound = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst);
    let console = ctx.console_port.load(std::sync::atomic::Ordering::SeqCst);
    // `port` is the **console's** port: that is where the bootstrap page goes.
    // `proxy_port` is reported separately so the page can say what the service
    // will listen on, and so the two never get confused again.
    let configured = ctx.settings.try_read().map(|s| s.port).unwrap_or(0);
    let proxy_port = if bound != 0 {
        bound
    } else if configured != 0 {
        configured
    } else {
        DEFAULT_PORT
    };
    Ok(json!({
        // The console is up as soon as this answers, even when the proxy is not.
        "running": console != 0,
        "port": console,
        "console_ready": console != 0,
        "proxy_running": ctx.service_ctrl.is_running(),
        "proxy_port": proxy_port,
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

/// Open the gateway's own console URL in the system default browser.
///
/// Same one-`cfg`-ladder shape as [`reveal_in_file_manager`]: the three
/// platforms differ only in the launcher program, and keeping them side by side
/// is what stops one of them from silently missing a fix.
///
/// ## The guard is the point
///
/// This function is the last hop before an OS program acts on a string. It
/// therefore re-checks the **prefix** itself instead of trusting its caller:
/// only `http://127.0.0.1:<port>/admin/…` may be spawned. Without this, any
/// future caller — a new RPC arm, a refactor that forwards a front-end field —
/// would turn the desktop into "open anything the console page asks for",
/// including `file://` and remote sites, in the user's real browser.
///
/// Returning `Result` rather than spawning and forgetting: the RPC layer reports
/// what happened, and a silently dropped `Err` here would look exactly like a
/// successful open to the person clicking the button.
pub(crate) fn open_url_in_browser(url: &str) -> Result<(), String> {
    // 双保险：调用方本应只传 `desktop_hooks::session_console_url` 拼出的地址，
    // 这里再独立校验一次。校验的是「方案 + 本机回环 + 控制台路径」，不是简单的
    // 包含关系——`file://`、别的站点、`http://127.0.0.1.evil.example/` 都过不来。
    if !proxy_rs::desktop_hooks::is_console_url(url) {
        return Err(format!(
            "拒绝打开非本机控制台地址（只允许 http://127.0.0.1:<端口>/admin/…）: {url}"
        ));
    }

    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("调用 open 失败: {e}"))?;
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("xdg-open")
            .arg(url)
            .spawn()
            .map_err(|e| format!("调用 xdg-open 失败: {e}"))?;
    }
    #[cfg(target_os = "windows")]
    {
        // `start` is a `cmd` builtin, so it must be run through `cmd /C`.
        //
        // The first argument of `start` is the **window title**, not the target.
        // Passing the URL there is the classic bug: with a title set, `start`
        // treats the next token as the thing to open, and a URL containing `&`
        // (or quotes) gets split by the command interpreter. The empty `""` is
        // that title, and it must be there even when we believe the URL is
        // clean — our URLs carry no query string precisely so this stays safe,
        // but the shell does not know that.
        std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn()
            .map_err(|e| format!("调用 cmd start 失败: {e}"))?;
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
    {
        return Err(format!("当前平台不支持在浏览器中打开: {url}"));
    }

    Ok(())
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
        // 0 = "not listening". It must not start at the default port: the
        // console and the bootstrap page use this as "is there a listener, and
        // where", and a default here reported a port that was never bound —
        // the bootstrap page then polled it and failed with "服务未在预期端口响应".
        bound_port: Arc::new(AtomicU16::new(0)),
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
        console_port: Arc::new(std::sync::atomic::AtomicU16::new(0)),
        client_auth: Arc::new(proxy_rs::client_accounts::ClientAuthState::new(
            client_auth_enabled,
        )),
    });

    // The console gets its own listener, started now and never stopped: it
    // carries the start/stop buttons, so it must outlive the proxy it controls.
    start_console_listener(ctx.clone());

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
                let app_restart = app.clone();
                let ctx_restart = ctx_for_hooks.clone();
                proxy_rs::desktop_hooks::register(proxy_rs::desktop_hooks::DesktopHooks {
                    start_service: Some(std::sync::Arc::new(move || {
                        request_start(app_start.clone(), ctx_start.clone());
                        Ok("代理服务已启动".to_string())
                    })),
                    stop_service: Some(std::sync::Arc::new(move || {
                        request_stop(app_stop.clone(), ctx_stop.clone());
                        Ok("代理服务已停止".to_string())
                    })),
                    // Rebuild the listener so a settings save reaches the data
                    // plane. Without this the save only wrote the file and the
                    // running router kept its old `Config` snapshot — the bug
                    // that made the classifier switch look broken in 3.5.2.
                    restart_service: Some(std::sync::Arc::new(move || {
                        restart_proxy_server(app_restart.clone(), ctx_restart.clone());
                        Ok("代理服务已按新配置重启".to_string())
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
                    // The console's own port, which is what any URL for its pages
                    // must carry. Deliberately **not** `bound_port`: that is the
                    // data plane's port and the two differ by as little as 1, so
                    // the wrong one produces "opened, but blank/404" rather than
                    // an error anyone can act on.
                    console_port: Some(std::sync::Arc::new({
                        let ctx_port = ctx_for_hooks.clone();
                        move || {
                            ctx_port
                                .console_port
                                .load(std::sync::atomic::Ordering::SeqCst)
                        }
                    })),
                    // 「在浏览器打开此会话」的最后一跳。RPC 层只交出 id，由
                    // `desktop_hooks::session_console_url` 拼出唯一允许的地址；
                    // 这里再校验一次前缀，然后交给平台启动器。
                    open_url: Some(std::sync::Arc::new(|url: String| {
                        open_url_in_browser(&url)?;
                        Ok("已在浏览器中打开".to_string())
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
