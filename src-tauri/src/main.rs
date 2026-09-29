#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use proxy_rs::{
    claude_config, codex_config, dsh_config, launch_agent, metrics, providers, router, service,
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
    /// Upstream credential pool (WorkBuddy login states / multi-key). Rebuilt
    /// on every proxy (re)start so saved credential changes hot-apply.
    credential_pool: Arc<proxy_rs::session_pool::CredentialPool>,
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
        // The overview header's 配置厂商 / 当前账号 / 剩余积分 cards.
        "current_identity": current_identity_summary(),
    }))
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
        "overrides_total": today.overrides_total,
    }))
}

/// How many recent conversations the speed panel compares.
const RECENT_SESSIONS: usize = 3;

/// Speed metrics for the most recently active conversation.
///
/// Backs the overview's 输出速度 card and its drill-down. Scoped to one session
/// and **aggregated over its recent turns** (see `StatsDb::query_session_metrics`):
/// a per-turn figure swings too much to read, so the card shows the session's
/// time-weighted rate instead — summed tokens over summed generation time.
///
/// The payload is `session_metrics_json(latest)` plus a `recent` array holding
/// the same shape for the last [`RECENT_SESSIONS`] conversations. The drill-down
/// is a transposed table whose columns are those sessions, so the top-level
/// fields and the array entries must agree — building both from one mapper is
/// what guarantees that.
///
/// `speed_tps` is `null` unless at least one turn in the window was actually
/// measurable — an unmeasured turn (a plain non-streamed reply, or a request
/// served before this feature existed) must render as `—`, not as a fabricated
/// number.
#[tauri::command]
async fn get_session_metrics(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let stats = ctx.stats.clone();
    // Blocking SQLite work off the async runtime, same as `get_stats`.
    let (m, recent) = tauri::async_runtime::spawn_blocking(move || {
        let latest = stats.query_latest_session_metrics()?;
        let recent = stats.query_recent_session_metrics(RECENT_SESSIONS)?;
        Ok::<_, anyhow::Error>((latest, recent))
    })
    .await
    .map_err(|e| e.to_string())?
    .map_err(|e| e.to_string())?;

    // One shape for both: the current session is just the first entry of the
    // same mapping, so a field added here cannot go missing from `recent`
    // (which is how the two would silently drift apart).
    let mut payload = session_metrics_json(&m);
    let entries: Vec<Value> = recent.iter().map(session_metrics_json).collect();
    if let Some(obj) = payload.as_object_mut() {
        obj.insert("recent".to_string(), Value::Array(entries));
    }
    Ok(payload)
}

/// Shape one session aggregate for the GUI.
///
/// Shared with the `recent` list so the current row and the comparison rows are
/// built by the same code — a second hand-rolled mapping is how the two would
/// drift apart.
fn session_metrics_json(m: &proxy_rs::stats::SessionMetrics) -> Value {
    json!({
        "session_id": m.session_id,
        "model": m.model,
        "last_at": m.last_at,
        "turns": m.turns,
        "measured_turns": m.measured_turns,
        "output_tokens": m.output_tokens,
        "model_ms": m.model_ms,
        "speed_tps": m.tps(),
        "avg_ttft_ms": m.avg_ttft_ms(),
        "tool_wait_ms": m.tool_wait_ms(),
        "tool_waits": m.tool_waits,
    })
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

/// Resolve the stored `GuiSettings::api_key` from what the settings form posted.
///
/// `GuiSettings::api_key` is legacy (pre-1.8.5): keys now live in the key pool,
/// and the form has no field for it any more. So an **empty** value means "the
/// form did not carry it", not "clear it" — the form used to post `""` on every
/// save, and assigning that straight through silently wiped a key that
/// `migrate_legacy_api_key` still reads at startup. A masked value is the older
/// guard for the same intent: the UI echoed back a secret it never had in the
/// clear, so it must not be stored as if it were the key.
///
/// Returns the value to store: the existing key unless the form supplied a real,
/// unmasked replacement.
fn resolve_api_key(incoming: &str, existing: &str) -> String {
    let trimmed = incoming.trim();
    if trimmed.is_empty() || trimmed.contains('•') || trimmed.contains('*') {
        existing.to_string()
    } else {
        trimmed.to_string()
    }
}

#[tauri::command]
async fn save_settings(
    body: SaveSettingsBody,
    app: tauri::AppHandle,
    ctx: State<'_, Arc<AppContext>>,
) -> Result<Value, String> {
    let mut s = ctx.settings.write().await;
    let new_key = resolve_api_key(&body.api_key, &s.api_key);

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

// ── WorkBuddy credential pool management ───────────────────────────────────

/// Mask a credential for the GUI: the token never leaves the backend whole.
fn mask_credential(
    c: &proxy_rs::workbuddy_auth::WorkBuddyCredential,
    sticky_sessions: usize,
    is_default: bool,
) -> Value {
    let now = proxy_rs::util::unix_millis();
    let state = if !c.enabled {
        "disabled"
    } else if c
        .cooldown_until_ms
        .map(|until| now < until)
        .unwrap_or(false)
    {
        "cooldown"
    } else if !c.is_usable(now) {
        "expired"
    } else {
        "ok"
    };
    json!({
        "id": c.id,
        "label": c.label,
        "masked_token": c.masked_token(),
        "expires_at_ms": c.expires_at_ms,
        "uid": c.account.uid,
        "nickname": c.account.nickname,
        "enabled": c.enabled,
        "state": state,
        "last_error": c.last_error,
        "sticky_sessions": sticky_sessions,
        "is_default": is_default,
        // `null` means "not queried yet" — distinct from a real 0 balance.
        "points": c.points,
        "points_fetched_at_ms": c.points_fetched_at_ms,
        "checked_in_today": proxy_rs::workbuddy_auth::checked_in_today(
            c,
            &proxy_rs::workbuddy_auth::local_day(now),
        ),
    })
}

#[tauri::command]
async fn wb_credentials_list(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let items = proxy_rs::workbuddy_auth::load_credentials();
    let sticky = ctx.credential_pool.sticky_count().await;
    let prefs = proxy_rs::workbuddy_auth::load_preferences();
    // With no explicit default the first usable credential is the one in force,
    // so the badge follows the same rule the pool's `pick` uses.
    let effective_default = items
        .iter()
        .find(|c| c.is_usable(proxy_rs::util::unix_millis()))
        .map(|c| c.id.clone())
        .unwrap_or_default();
    let default_id = if prefs.default_credential_id.is_empty() {
        effective_default
    } else {
        prefs.default_credential_id.clone()
    };
    let list: Vec<Value> = items
        .iter()
        .map(|c| mask_credential(c, sticky, c.id == default_id))
        .collect();
    Ok(json!({
        "credentials": list,
        "default_credential_id": prefs.default_credential_id,
        "default_identity_id": prefs.default_identity_id,
    }))
}

#[derive(Deserialize)]
struct WbCredentialAddBody {
    /// Raw login-state JSON pasted from the desktop app or a session file.
    login_state: String,
    #[serde(default)]
    label: String,
}

#[tauri::command]
async fn wb_credentials_add(
    ctx: State<'_, Arc<AppContext>>,
    body: WbCredentialAddBody,
) -> Result<Value, String> {
    let mut credential = proxy_rs::workbuddy_auth::parse_login_state(&body.login_state)
        .map_err(|e| e.to_string())?;
    if !body.label.trim().is_empty() {
        credential.label = body.label.trim().to_string();
    }
    proxy_rs::workbuddy_auth::upsert_credential(credential.clone()).map_err(|e| e.to_string())?;
    reload_pool(&ctx).await;
    ctx.logs
        .push(
            "INFO",
            format!(
                "WorkBuddy 凭据已保存: {} ({})",
                credential.label, credential.id
            ),
        )
        .await;
    Ok(mask_credential(&credential, 0, false))
}

#[tauri::command]
async fn wb_credentials_delete(
    ctx: State<'_, Arc<AppContext>>,
    id: String,
) -> Result<Value, String> {
    let items = proxy_rs::workbuddy_auth::load_credentials();
    let remaining: Vec<_> = items.into_iter().filter(|c| c.id != id).collect();
    proxy_rs::workbuddy_auth::save_credentials(&remaining).map_err(|e| e.to_string())?;
    // Hot-apply: reload drops only the bindings pointing at the removed
    // credential, rather than resetting every session's stickiness.
    reload_pool(&ctx).await;
    ctx.logs
        .push("INFO", format!("WorkBuddy 凭据已删除: {}", id))
        .await;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
async fn wb_credentials_toggle(
    ctx: State<'_, Arc<AppContext>>,
    id: String,
    enabled: bool,
) -> Result<Value, String> {
    let mut items = proxy_rs::workbuddy_auth::load_credentials();
    let Some(c) = items.iter_mut().find(|c| c.id == id) else {
        return Err(format!("凭据不存在: {}", id));
    };
    c.enabled = enabled;
    if enabled {
        c.cooldown_until_ms = None;
        c.last_error.clear();
    }
    proxy_rs::workbuddy_auth::save_credentials(&items).map_err(|e| e.to_string())?;
    reload_pool(&ctx).await;
    if enabled {
        ctx.credential_pool.mark_ok(&id).await;
    }
    ctx.logs
        .push(
            "INFO",
            format!(
                "WorkBuddy 凭据 {} 已{}",
                id,
                if enabled { "启用" } else { "禁用" }
            ),
        )
        .await;
    Ok(json!({ "ok": true }))
}

/// Reload the in-memory pool after a default/credential change.
///
/// The proxy picks credentials from the pool on every request, so this is what
/// makes a newly selected default take effect without a service restart.
async fn reload_pool(ctx: &Arc<AppContext>) {
    ctx.credential_pool.reload_from_store().await;
}

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

#[tauri::command]
async fn wb_set_default(
    app: tauri::AppHandle,
    ctx: State<'_, Arc<AppContext>>,
    id: String,
) -> Result<Value, String> {
    // Route through the unified 身份池 so a single "default" always wins; the
    // legacy credential/key selectors both funnel into the same field now.
    let prefs = proxy_rs::workbuddy_auth::set_default_identity(&id).map_err(|e| e.to_string())?;
    reload_pool(&ctx).await;
    start_proxy_server(app, ctx.inner().clone());
    ctx.logs
        .push("INFO", format!("已设置默认 WorkBuddy 账号: {}", id))
        .await;
    Ok(json!({ "ok": true, "default_identity_id": prefs.default_identity_id }))
}

#[tauri::command]
async fn wb_preferences() -> Result<Value, String> {
    let prefs = proxy_rs::workbuddy_auth::load_preferences();
    Ok(json!({
        "default_credential_id": prefs.default_credential_id,
        "default_key_id": prefs.default_key_id,
        "default_identity_id": prefs.default_identity_id,
        "daily_checkin_enabled": prefs.daily_checkin_enabled,
        "daily_checkin_time": prefs.daily_checkin_time,
        "last_checkin_run_date": prefs.last_checkin_run_date,
    }))
}

#[tauri::command]
async fn wb_set_schedule(
    ctx: State<'_, Arc<AppContext>>,
    enabled: bool,
    time: String,
) -> Result<Value, String> {
    let prefs =
        proxy_rs::workbuddy_auth::save_schedule(enabled, &time).map_err(|e| e.to_string())?;
    ctx.logs
        .push(
            "INFO",
            format!(
                "每日定时打卡已{}（{}）",
                if enabled { "开启" } else { "关闭" },
                prefs.daily_checkin_time
            ),
        )
        .await;
    Ok(json!({
        "ok": true,
        "daily_checkin_enabled": prefs.daily_checkin_enabled,
        "daily_checkin_time": prefs.daily_checkin_time,
    }))
}

#[tauri::command]
async fn wb_refresh_points(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let report = proxy_rs::workbuddy_auth::refresh_all_points(&ctx.client).await;
    let known = report.results.iter().filter(|r| r.points.is_some()).count();
    ctx.logs
        .push(
            "INFO",
            format!(
                "已刷新身份池积分: 共 {} 个（账号+密钥），{} 个读取成功",
                report.results.len(),
                known
            ),
        )
        .await;
    // The refreshed balance belongs to the identity in force, so hand back the
    // same shape the overview header already consumes — a manual refresh and a
    // status poll update the card through one code path.
    Ok(json!({
        "results": report.results,
        "current_identity": current_identity_summary(),
    }))
}

#[tauri::command]
async fn wb_checkin_all(
    ctx: State<'_, Arc<AppContext>>,
    force: Option<bool>,
) -> Result<Value, String> {
    let forced = force.unwrap_or(false);
    let report = if forced {
        proxy_rs::workbuddy_auth::batch_claim_daily_checkin_forced(&ctx.client).await
    } else {
        proxy_rs::workbuddy_auth::batch_claim_daily_checkin(&ctx.client).await
    };
    ctx.logs
        .push(
            "INFO",
            format!(
                "WorkBuddy 账号池打卡完成: 共 {} 个，成功 {} 个，已打卡 {} 个，失败 {} 个",
                report.total, report.success, report.already_checked_in, report.failed
            ),
        )
        .await;
    Ok(json!(report))
}

#[tauri::command]
async fn wb_checkin_single(ctx: State<'_, Arc<AppContext>>, id: String) -> Result<Value, String> {
    let items = proxy_rs::workbuddy_auth::load_credentials();
    let Some(c) = items.into_iter().find(|item| item.id == id) else {
        return Err(format!("凭据不存在: {}", id));
    };
    // The manual button ignores the local "already claimed" record: the user is
    // asking for this one explicitly.
    let res = proxy_rs::workbuddy_auth::claim_daily_checkin_forced(&ctx.client, &c).await;
    ctx.logs
        .push(
            "INFO",
            format!("WorkBuddy 账号 {} 打卡结果: {}", id, res.message),
        )
        .await;
    Ok(json!(res))
}

// ── Upstream API key pool ──────────────────────────────────────────────────

/// Mask one key entry for the GUI; the raw key never leaves the backend.
fn mask_api_key(k: &proxy_rs::workbuddy_auth::ApiKeyEntry, is_default: bool) -> Value {
    json!({
        "id": k.id,
        "label": k.label,
        "masked": k.masked(),
        "enabled": k.enabled,
        "points": k.points,
        "is_default": is_default,
    })
}

#[tauri::command]
async fn api_keys_list() -> Result<Value, String> {
    let items = proxy_rs::workbuddy_auth::load_api_keys();
    let prefs = proxy_rs::workbuddy_auth::load_preferences();
    // With no explicit default the first enabled key is the one in force, so
    // the badge follows the same rule `active_api_key` uses.
    let effective = proxy_rs::workbuddy_auth::active_api_key();
    let list: Vec<Value> = items
        .iter()
        .map(|k| {
            let is_default = if !prefs.default_key_id.is_empty() {
                k.id == prefs.default_key_id
            } else {
                effective.as_deref() == Some(k.key.as_str())
            };
            mask_api_key(k, is_default)
        })
        .collect();
    Ok(json!({
        "keys": list,
        "default_key_id": prefs.default_key_id,
        "default_identity_id": prefs.default_identity_id,
    }))
}

#[derive(Deserialize)]
struct ApiKeyAddBody {
    key: String,
    #[serde(default)]
    label: String,
}

#[tauri::command]
async fn api_keys_add(
    ctx: State<'_, Arc<AppContext>>,
    body: ApiKeyAddBody,
) -> Result<Value, String> {
    let key = body.key.trim().to_string();
    if key.is_empty() {
        return Err("密钥不能为空".to_string());
    }
    let entry = proxy_rs::workbuddy_auth::ApiKeyEntry {
        id: proxy_rs::workbuddy_auth::api_key_id(&key),
        label: body.label.trim().to_string(),
        key,
        enabled: true,
        points: None,
        points_fetched_at_ms: None,
    };
    proxy_rs::workbuddy_auth::upsert_api_key(entry.clone()).map_err(|e| e.to_string())?;
    ctx.logs
        .push("INFO", format!("上游密钥已加入密钥池: {}", entry.id))
        .await;
    Ok(mask_api_key(&entry, false))
}

#[tauri::command]
async fn api_keys_delete(ctx: State<'_, Arc<AppContext>>, id: String) -> Result<Value, String> {
    let items = proxy_rs::workbuddy_auth::load_api_keys();
    let remaining: Vec<_> = items.into_iter().filter(|k| k.id != id).collect();
    proxy_rs::workbuddy_auth::save_api_keys(&remaining).map_err(|e| e.to_string())?;
    // A dangling default pointer would silently fall through to the first key
    // anyway, but clearing it keeps the GUI badge honest.
    let mut prefs = proxy_rs::workbuddy_auth::load_preferences();
    if prefs.default_key_id == id {
        prefs.default_key_id.clear();
        let _ = proxy_rs::workbuddy_auth::save_preferences(&prefs);
    }
    ctx.logs
        .push("INFO", format!("上游密钥已删除: {}", id))
        .await;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
async fn api_keys_toggle(
    ctx: State<'_, Arc<AppContext>>,
    id: String,
    enabled: bool,
) -> Result<Value, String> {
    let mut items = proxy_rs::workbuddy_auth::load_api_keys();
    let Some(k) = items.iter_mut().find(|k| k.id == id) else {
        return Err(format!("密钥不存在: {}", id));
    };
    k.enabled = enabled;
    proxy_rs::workbuddy_auth::save_api_keys(&items).map_err(|e| e.to_string())?;
    ctx.logs
        .push(
            "INFO",
            format!(
                "上游密钥 {} 已{}",
                id,
                if enabled { "启用" } else { "禁用" }
            ),
        )
        .await;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
async fn api_keys_set_default(
    app: tauri::AppHandle,
    ctx: State<'_, Arc<AppContext>>,
    id: String,
) -> Result<Value, String> {
    // Route through the unified 身份池; an empty id clears back to the key/
    // static fallback. Hot-applies by rebuilding the running proxy, which
    // resolves its effective key from the pool via `active_api_key()`.
    let prefs = proxy_rs::workbuddy_auth::set_default_identity(&id).map_err(|e| e.to_string())?;
    start_proxy_server(app, ctx.inner().clone());
    ctx.logs
        .push("INFO", format!("已设置默认上游密钥: {}", id))
        .await;
    Ok(json!({ "ok": true, "default_identity_id": prefs.default_identity_id }))
}

/// Unified 身份池 default selector: choose the default account, the default
/// key, or "use the key/static path" (`__none__`). Supersedes both the legacy
/// `wb_set_default` and `api_keys_set_default` controls.
#[tauri::command]
async fn wb_set_default_identity(
    app: tauri::AppHandle,
    ctx: State<'_, Arc<AppContext>>,
    id: String,
) -> Result<Value, String> {
    let prefs = proxy_rs::workbuddy_auth::set_default_identity(&id).map_err(|e| e.to_string())?;
    reload_pool(&ctx).await;
    start_proxy_server(app, ctx.inner().clone());
    ctx.logs
        .push(
            "INFO",
            format!(
                "已设置默认身份: {}",
                if id.is_empty() {
                    "默认（账号优先，否则密钥/单 Key）".to_string()
                } else {
                    id
                }
            ),
        )
        .await;
    Ok(json!({ "ok": true, "default_identity_id": prefs.default_identity_id }))
}

#[tauri::command]
async fn wb_sticky_reset(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    ctx.credential_pool.reset_sticky().await;
    ctx.logs
        .push("INFO", "已重置会话粘滞：全部会话回到默认凭据".to_string())
        .await;
    Ok(json!({ "ok": true }))
}

#[tauri::command]
async fn wb_oauth_start(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let res = proxy_rs::workbuddy_auth::start_oauth_flow(&client, None)
        .await
        .map_err(|e| e.to_string())?;
    ctx.logs
        .push("INFO", format!("已生成 WorkBuddy 扫码授权: {}", res.state))
        .await;
    Ok(json!({
        "state": res.state,
        "auth_url": res.auth_url,
        "qr_svg": res.qr_svg,
    }))
}

#[tauri::command]
async fn wb_oauth_poll(ctx: State<'_, Arc<AppContext>>, state: String) -> Result<Value, String> {
    let client = reqwest::Client::new();
    let res = proxy_rs::workbuddy_auth::poll_oauth_token(&client, None, &state)
        .await
        .map_err(|e| e.to_string())?;

    match res {
        proxy_rs::workbuddy_auth::OAuthPollResult::Pending => Ok(json!({ "status": "pending" })),
        proxy_rs::workbuddy_auth::OAuthPollResult::Failed { error } => {
            Ok(json!({ "status": "failed", "error": error }))
        }
        proxy_rs::workbuddy_auth::OAuthPollResult::Success { credential } => {
            ctx.logs
                .push(
                    "INFO",
                    format!(
                        "WorkBuddy 扫码授权成功并保存: {} ({})",
                        credential.label, credential.id
                    ),
                )
                .await;
            Ok(json!({
                "status": "success",
                "credential": mask_credential(&credential, 0, false),
            }))
        }
    }
}

#[tauri::command]
async fn fetch_models(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let settings = ctx.settings.read().await;
    let preset = settings.models_preset();
    let static_key = settings.api_key.clone();
    drop(settings);

    let api_key = proxy_rs::workbuddy_auth::active_api_key()
        .or(Some(static_key))
        .filter(|k| !k.trim().is_empty());
    let api_key = match api_key {
        Some(k) => k,
        None => return Err("未配置 API Key".to_string()),
    };

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

/// Report the DSH provider wiring so the UI can show the current state.
#[tauri::command]
async fn get_dsh_config() -> Result<Value, String> {
    let settings_path = match dsh_config::settings_path() {
        Some(p) => p,
        None => {
            return Ok(json!({
                "supported": false,
                "reason": "DSH_HOME / HOME is not set",
            }))
        }
    };

    let credentials_path = dsh_config::credentials_path();

    if !settings_path.exists() {
        return Ok(json!({
            "supported": true,
            "settings_path": settings_path.display().to_string(),
            "settings_exists": false,
            "provider_exists": false,
            "model_count": 0,
            "base_url": null,
            "credential_present": false,
        }));
    }

    let settings_text = std::fs::read_to_string(&settings_path).map_err(|e| e.to_string())?;
    let credentials_text = credentials_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok());
    let state = dsh_config::read_state(&settings_text, credentials_text.as_deref());

    Ok(json!({
        "supported": true,
        "settings_path": settings_path.display().to_string(),
        "settings_exists": true,
        "provider_exists": state.provider_exists,
        "model_count": state.model_ids.len(),
        "base_url": state.base_url,
        "credential_present": state.credential_present,
    }))
}

/// Write this proxy into DSH's `settings.yaml` as the `proxy-rs` provider.
///
/// Only that provider's `baseURL` and `models` are touched; the rest of the
/// document (other providers, the default model, UI preferences, plugin
/// namespaces) is left byte-for-byte intact. See `dsh_config` for why the write
/// is textual rather than a YAML round-trip.
#[tauri::command]
async fn apply_dsh_config(ctx: State<'_, Arc<AppContext>>) -> Result<Value, String> {
    let settings_path = dsh_config::settings_path()
        .ok_or_else(|| "DSH_HOME / HOME 未设置，无法定位 DSH 配置".to_string())?;
    let credentials_path = dsh_config::credentials_path()
        .ok_or_else(|| "DSH_HOME / HOME 未设置，无法定位 DSH 凭据文件".to_string())?;

    let (preset, api_key) = {
        let settings = ctx.settings.read().await;
        (settings.models_preset(), settings.api_key.clone())
    };

    if api_key.is_empty() {
        return Err("尚未配置 API Key，无法拉取模型列表".to_string());
    }

    let models = providers::fetch_models(&ctx.client, &preset, &api_key)
        .await
        .map_err(|e| format!("从上游 {} 拉取模型失败，未改动 DSH 配置: {}", preset.id, e))?;

    if models.is_empty() {
        return Err(format!(
            "上游 {} 未返回任何模型，未改动 DSH 配置",
            preset.id
        ));
    }

    // Prefer the port actually bound, but fall back to the configured one: a
    // stopped service reports port 0, and writing `http://127.0.0.1:0` would
    // point DSH at nothing. The configured port is where the service will bind
    // when it next starts.
    let bound = ctx.bound_port.load(std::sync::atomic::Ordering::SeqCst);
    let port = if bound != 0 {
        bound
    } else {
        ctx.settings.read().await.port
    };
    let base_url = format!("http://127.0.0.1:{}/v1", port);

    let dsh_models: Vec<dsh_config::DshModel> =
        models.iter().map(dsh_config::to_dsh_model).collect();

    let (settings_path, credentials_path, credential_added) =
        dsh_config::apply(&settings_path, &credentials_path, &base_url, &dsh_models)
            .map_err(|e| format!("写入 DSH 配置失败: {}", e))?;

    ctx.logs
        .push(
            "INFO",
            format!(
                "已写入 DSH 配置（{} 个模型，{}）: {}",
                dsh_models.len(),
                base_url,
                settings_path.display()
            ),
        )
        .await;

    Ok(json!({
        "ok": true,
        "models": dsh_models.len(),
        "base_url": base_url,
        "settings_path": settings_path.display().to_string(),
        "credentials_path": credentials_path.display().to_string(),
        "credential_added": credential_added,
        "note": "DSH 会在下次请求时读取新配置，无需重启",
    }))
}

#[tauri::command]
async fn test_upstream(
    body: TestUpstreamBody,
    ctx: State<'_, Arc<AppContext>>,
) -> Result<Value, String> {
    let settings = ctx.settings.read().await;
    let url = settings.chat_url(&providers::builtin_presets());
    // Capture the static key before dropping the guard; the key pool's active
    // key (which now also includes the migrated legacy single API Key) takes
    // priority, matching what the running proxy actually sends.
    let static_key = settings.api_key.clone();
    drop(settings);
    let api_key = proxy_rs::workbuddy_auth::active_api_key()
        .or(Some(static_key))
        .filter(|k| !k.trim().is_empty());
    let api_key = match api_key {
        Some(k) => k,
        None => return Err("API Key 尚未配置".to_string()),
    };
    let model = body
        .model
        .unwrap_or_else(|| "deepseek-v4-flash".to_string());

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
    // Migrate the legacy single API Key field into the key pool so the unified
    // 身份池 selector can manage it alongside the other keys. This runs before
    // the proxy builds its config, so the migrated key is already in force. A
    // key that already exists in the pool (same material) is deduped by id and
    // simply re-enables/re-labels the existing entry.
    migrate_legacy_api_key(&settings);
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
        is_launchd_child,
        // Credentials are re-read on every proxy (re)start, so a credential
        // saved in the GUI hot-applies with the rest of the settings. Ordered:
        // the GUI-selected default goes first, which is what makes it the
        // default (the pool picks the first usable entry).
        credential_pool: Arc::new(proxy_rs::session_pool::CredentialPool::new(
            proxy_rs::workbuddy_auth::ordered_credentials(),
        )),
    });

    let ctx_for_setup = ctx.clone();
    let ctx_for_tray = ctx.clone();
    let ctx_for_scheduler = ctx.clone();

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
            get_session_metrics,
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
            get_dsh_config,
            apply_dsh_config,
            test_upstream,
            open_logs_dir,
            wb_credentials_list,
            wb_credentials_add,
            wb_credentials_delete,
            wb_credentials_toggle,
            wb_sticky_reset,
            wb_oauth_start,
            wb_oauth_poll,
            wb_checkin_all,
            wb_checkin_single,
            wb_set_default,
            wb_preferences,
            wb_set_schedule,
            wb_refresh_points,
            wb_set_default_identity,
            api_keys_list,
            api_keys_add,
            api_keys_delete,
            api_keys_toggle,
            api_keys_set_default
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
