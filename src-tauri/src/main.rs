#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use proxy_rs::{
    claude_config, codex_config, launch_agent, metrics, providers, router, service,
    settings::{self, GuiSettings, LogBuffer, DEFAULT_PORT},
    stats::RequestLogFilter,
    Config, StatsDb,
};
use serde::Deserialize;
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
    /// Whether launchd started this copy. Controls whether it may re-register
    /// its own job (it must not: `bootout` would kill the caller).
    is_launchd_child: bool,
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

#[tauri::command]
async fn get_status(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let settings = ctx.settings.read().await;
    let service_running = ctx.service_ctrl.is_running();
    let port = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst);
    let presets = providers::builtin_presets();
    let upstream = settings.chat_url(&presets);

    Ok(json!({
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
    }))
}

#[tauri::command]
async fn get_stats(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    // SQLite access is blocking; run it off the async runtime so a slow query
    // (or a writer holding the connection) cannot park a worker thread. The UI
    // polls this every couple of seconds.
    let stats = ctx.stats.clone();
    let today = tauri::async_runtime::spawn_blocking(move || stats.query_today())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(json!({
        "date": today.date,
        "requests_total": today.requests_total,
        "requests_success": today.requests_success,
        "requests_failed": today.requests_failed,
        "tokens_total": today.tokens_total(),
        "tokens_input": today.tokens_input,
        "tokens_cache_read": today.tokens_cache_read,
        "tokens_cache_write": today.tokens_cache_write,
        "tokens_output": today.tokens_output,
        "cache_hit_pct": today.cache_hit_pct(),
    }))
}

/// Start (or restart) the proxy service and note it in the log. The server task
/// logs the real outcome once the port is bound, so a failed bind is never
/// reported as success here.
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

#[tauri::command]
async fn get_logs(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let entries = ctx.logs.snapshot().await;
    Ok(json!({ "entries": entries }))
}

#[tauri::command]
async fn clear_logs(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    ctx.logs.clear().await;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
async fn get_request_logs(
    filter: Option<RequestLogFilter>,
    ctx: State<'_, Arc<AppContext>>,
) -> Result<Value, String> {
    let f = filter.unwrap_or_default();
    // Blocking SQLite off the async runtime: this runs several queries under a
    // std Mutex, on a command the UI polls every 2 seconds.
    let stats = ctx.stats.clone();
    let res = tauri::async_runtime::spawn_blocking(move || stats.query_request_logs(&f))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    serde_json::to_value(&res).map_err(|e| e.to_string())
}

#[tauri::command]
async fn clear_request_logs(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let stats = ctx.stats.clone();
    tauri::async_runtime::spawn_blocking(move || stats.clear_request_logs())
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
async fn get_settings(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let mut s = ctx.settings.read().await.clone();
    s.api_key = mask_key(&s.api_key);
    let mut payload = serde_json::to_value(&s).map_err(|e| e.to_string())?;
    if let Some(obj) = payload.as_object_mut() {
        let port = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst);
        obj.insert("actual_port".into(), json!(port));
        // What the provider itself declares, so the UI can label an unset
        // switch as "following the preset" rather than "off".
        obj.insert(
            "force_stream_preset".into(),
            json!(s.force_stream(&providers::builtin_presets())),
        );
    }
    Ok(payload)
}

#[derive(Deserialize)]
struct SaveSettingsBody {
    provider_id: String,
    #[serde(default)]
    custom_url: String,
    #[serde(default)]
    api_key: String,
    #[serde(default)]
    port: u16,
    #[serde(default)]
    bind: String,
    #[serde(default)]
    reasoning_model: String,
    #[serde(default)]
    completion_model: String,
    #[serde(default)]
    model_map: String,
    #[serde(default)]
    launch_at_login: bool,
    #[serde(default)]
    sanitize_terms: String,
    /// `null` keeps following the provider preset's own setting.
    #[serde(default)]
    force_stream: Option<bool>,
}

#[tauri::command]
async fn save_settings(
    body: SaveSettingsBody,
    app: tauri::AppHandle,
    ctx: State<'_, Arc<AppContext>>,
) -> Result<Value, String> {
    let mut s = ctx.settings.write().await;
    let new_key = if body.api_key.contains('•') || body.api_key.contains('*') {
        s.api_key.clone()
    } else {
        body.api_key.trim().to_string()
    };

    s.provider_id = body.provider_id.trim().to_string();
    s.custom_url = body.custom_url.trim().to_string();
    s.api_key = new_key;
    s.port = if body.port == 0 {
        DEFAULT_PORT
    } else {
        body.port
    };
    s.bind = if body.bind.trim().is_empty() {
        "127.0.0.1".to_string()
    } else {
        body.bind.trim().to_string()
    };
    s.reasoning_model = body.reasoning_model.trim().to_string();
    s.completion_model = body.completion_model.trim().to_string();
    s.model_map = body.model_map.trim().to_string();
    s.launch_at_login = body.launch_at_login;
    s.sanitize_terms = body.sanitize_terms.trim().to_string();
    s.force_stream = body.force_stream;

    s.save().map_err(|e| format!("保存配置失败: {}", e))?;

    let provider = s.provider_id.clone();
    let upstream = s.chat_url(&providers::builtin_presets());
    drop(s);

    ctx.logs
        .push("INFO", format!("设置已保存 (服务商: {})", provider))
        .await;

    // Toggle launch at login.
    //
    // The launchd copy skips `install()`: this process *is* the job, and
    // `install()` re-registers by booting the job out first — which kills the
    // caller. Toggling the switch on from within the launchd copy is therefore
    // a no-op in practice; the setting is persisted either way and the next
    // user-driven start re-arms it. `uninstall()` is safe from any copy.
    if body.launch_at_login {
        if !ctx.is_launchd_child {
            let _ = launch_agent::install();
        }
    } else {
        let _ = launch_agent::uninstall();
    }

    update_tray_state(&app, &ctx);

    // Hot-apply: rebuild the running proxy from the freshly saved settings so
    // model_map / sanitize_terms / api_key take effect immediately without a
    // full app quit+relaunch.
    start_proxy_server(app, ctx.inner().clone());

    Ok(json!({ "ok": true, "upstream_url": upstream }))
}

#[tauri::command]
async fn get_providers() -> Result<Value, String> {
    Ok(json!({ "providers": providers::builtin_presets() }))
}

#[tauri::command]
async fn fetch_models(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let settings = ctx.settings.read().await;
    let preset = settings.models_preset();
    let api_key = settings.api_key.clone();
    drop(settings);

    if api_key.is_empty() {
        return Err("未配置 API Key".to_string());
    }

    match providers::fetch_models(&ctx.client, &preset, &api_key).await {
        Ok(models) => {
            ctx.logs
                .push(
                    "INFO",
                    format!("从 {} 拉取到 {} 个模型", preset.id, models.len()),
                )
                .await;
            Ok(json!({ "provider": preset.id, "models": models }))
        }
        Err(e) => {
            ctx.logs.push("ERROR", format!("拉取模型失败: {}", e)).await;
            Err(e.to_string())
        }
    }
}

#[tauri::command]
async fn get_claude_config() -> Result<Value, String> {
    claude_config::read_current().map_err(|e| e.to_string())
}

#[derive(Deserialize)]
struct ClaudeConfigBody {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    sonnet: Option<String>,
    #[serde(default)]
    opus: Option<String>,
    #[serde(default)]
    haiku: Option<String>,
}

#[tauri::command]
async fn apply_claude_config(
    body: ClaudeConfigBody,
    ctx: State<'_, Arc<AppContext>>,
) -> Result<Value, String> {
    // Prefer the port actually bound, but fall back to the configured one: a
    // stopped service reports port 0, and writing `http://127.0.0.1:0` would
    // point Claude Code at nothing. The configured port is where the service
    // will bind when it next starts.
    let bound = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst);
    let port = if bound != 0 {
        bound
    } else {
        ctx.settings.read().await.port
    };
    let base_url = format!("http://127.0.0.1:{}", port);

    let mut updates = Vec::new();
    for (slot, val) in [
        ("model", body.model),
        ("sonnet", body.sonnet),
        ("opus", body.opus),
        ("haiku", body.haiku),
    ] {
        if let Some(m) = val.filter(|s| !s.trim().is_empty()) {
            updates.push(claude_config::SlotUpdate {
                slot: slot.to_string(),
                model: m,
                name: None,
            });
        }
    }

    if updates.is_empty() {
        return Err("未指定任何模型".to_string());
    }

    match claude_config::apply_slots(&updates, &base_url) {
        Ok(v) => {
            ctx.logs
                .push("INFO", "已成功写入 ~/.claude/settings.json".into())
                .await;
            Ok(v)
        }
        Err(e) => Err(e.to_string()),
    }
}

#[derive(Deserialize)]
struct TestUpstreamBody {
    #[serde(default)]
    model: Option<String>,
}

/// Report the Codex catalog wiring so the UI can show the current state.
#[tauri::command]
async fn get_codex_config() -> Result<Value, String> {
    let config_path = match codex_config::config_path() {
        Some(p) => p,
        None => {
            return Ok(json!({
                "supported": false,
                "reason": "CODEX_HOME / HOME is not set",
            }))
        }
    };

    let catalog_path = codex_config::current_catalog_path(&config_path);

    Ok(json!({
        "supported": true,
        "config_path": config_path.display().to_string(),
        "config_exists": config_path.exists(),
        "catalog_path": catalog_path.as_ref().map(|p| p.display().to_string()),
        "catalog_exists": catalog_path.as_ref().map(|p| p.exists()).unwrap_or(false),
    }))
}

/// Generate `model_catalog_json` from the provider's live model list.
///
/// Codex never consults a generic provider's `/v1/models` for its picker, so
/// the catalog has to come from this user-level key instead.
#[tauri::command]
async fn apply_codex_config(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let config_path = codex_config::config_path()
        .ok_or_else(|| "CODEX_HOME / HOME 未设置，无法定位 Codex 配置".to_string())?;

    let (preset, api_key) = {
        let settings = ctx.settings.read().await;
        (settings.models_preset(), settings.api_key.clone())
    };

    if api_key.is_empty() {
        return Err("尚未配置 API Key，无法拉取模型列表".to_string());
    }

    let models = providers::fetch_models(&ctx.client, &preset, &api_key)
        .await
        .map_err(|e| {
            format!(
                "从上游 {} 拉取模型失败，未改动 Codex 配置: {}",
                preset.id, e
            )
        })?;

    if models.is_empty() {
        return Err(format!(
            "上游 {} 未返回任何模型，未改动 Codex 配置",
            preset.id
        ));
    }

    let (catalog_path, config_path) = codex_config::write_catalog(&models, &config_path)
        .map_err(|e| format!("写入 Codex 配置失败: {}", e))?;

    ctx.logs
        .push(
            "INFO",
            format!(
                "已写入 Codex 模型目录（{} 个模型）: {}",
                models.len(),
                catalog_path.display()
            ),
        )
        .await;

    Ok(json!({
        "ok": true,
        "models": models.len(),
        "catalog_path": catalog_path.display().to_string(),
        "config_path": config_path.display().to_string(),
        "note": "重启 Codex 后模型选择器生效",
    }))
}
#[tauri::command]
async fn test_upstream(
    body: TestUpstreamBody,
    ctx: State<'_, Arc<AppContext>>,
) -> Result<Value, String> {
    let settings = ctx.settings.read().await;
    let url = settings.chat_url(&providers::builtin_presets());
    let api_key = settings.api_key.clone();
    let model = body
        .model
        .unwrap_or_else(|| "deepseek-v4-flash".to_string());
    drop(settings);

    if api_key.is_empty() {
        return Err("API Key 尚未配置".to_string());
    }

    let payload = json!({
        "model": model,
        "messages": [{"role": "user", "content": "请只回复:连接成功"}],
        "stream": true,
        "stream_options": {"include_usage": true},
        "max_tokens": 32,
    });

    let resp = ctx
        .client
        .post(&url)
        .header("Authorization", format!("Bearer {}", api_key))
        .header("X-API-Key", &api_key)
        .json(&payload)
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .map_err(|e| format!("网络请求异常: {}", e))?;

    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.is_success() {
        let got_content = text.contains("\"content\"");
        ctx.logs
            .push("INFO", format!("上游连通性测试通过 (模型: {})", model))
            .await;
        Ok(json!({
            "ok": got_content,
            "detail": if got_content { "上游正常返回了回复内容" } else { "上游响应成功但未包含 content" }
        }))
    } else {
        ctx.logs
            .push("ERROR", format!("上游测试响应错误: {} {}", status, text))
            .await;
        Err(format!("上游返回 HTTP {}: {}", status, text))
    }
}

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
    #[cfg(target_os = "macos")]
    {
        let _ = std::process::Command::new("open").arg(&dir).spawn();
    }
    #[cfg(target_os = "linux")]
    {
        let _ = std::process::Command::new("xdg-open").arg(&dir).spawn();
    }
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new("explorer").arg(&dir).spawn();
    }
}

fn mask_key(key: &str) -> String {
    if key.is_empty() {
        return String::new();
    }
    let chars: Vec<char> = key.chars().collect();
    if chars.len() <= 8 {
        return "•".repeat(chars.len());
    }
    let head: String = chars[..4].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{}••••{}", head, tail)
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
    tauri::async_runtime::spawn(async move {
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
        );

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

        let server = axum::serve(listener, app_router)
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
    if let Ok(guard) = ctx.server_shutdown.lock() {
        guard.cancel();
    }
    next_server_id(&ctx);
    ctx.bound_port.store(0, std::sync::atomic::Ordering::SeqCst);
    ctx.service_ctrl.mark_stopped();
}

fn main() {
    let settings = GuiSettings::load();
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

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
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
        is_launchd_child,
    });

    let ctx_for_setup = ctx.clone();
    let ctx_for_tray = ctx.clone();

    let mut builder = tauri::Builder::default();

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
            get_status,
            get_stats,
            start_service,
            stop_service,
            get_logs,
            clear_logs,
            get_request_logs,
            clear_request_logs,
            get_settings,
            save_settings,
            get_providers,
            fetch_models,
            get_claude_config,
            apply_claude_config,
            get_codex_config,
            apply_codex_config,
            test_upstream,
            open_logs_dir
        ])
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            // On macOS, clicking the Dock icon after the window has been hidden
            // (closed into the tray) fires `Reopen`. Tauri does not auto-show the
            // window, so without this the only way back is the tray menu. Restore
            // it here so the Dock is a first-class way to reopen the console.
            if let tauri::RunEvent::Reopen { .. } = event {
                focus_main_window(app);
            }
        });
}
