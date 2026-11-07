//! Tray icon/menu wiring and the start/stop requests it triggers.

use crate::app::context::*;
use crate::*;
use serde_json::json;

pub(crate) fn update_tray_state(app: &tauri::AppHandle, ctx: &AppContext) {
    let running = ctx.service_ctrl.is_running();
    let port = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst);

    if let Some(tray) = app.tray_by_id("main-tray") {
        let active_bytes = include_bytes!("../../icons/tray_active.png");
        let stopped_bytes = include_bytes!("../../icons/tray_stopped.png");
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
pub(crate) fn emit_service_state(app: &tauri::AppHandle) {
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
pub(crate) fn current_identity_summary() -> Value {
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
pub(crate) fn spawn_supervised<F>(ctx: Arc<AppContext>, label: &'static str, fut: F)
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
pub(crate) fn desktop_status(ctx: &Arc<AppContext>) -> Value {
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

pub(crate) fn request_start(app: tauri::AppHandle, ctx: Arc<AppContext>) {
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
pub(crate) fn request_stop(app: tauri::AppHandle, ctx: Arc<AppContext>) {
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
