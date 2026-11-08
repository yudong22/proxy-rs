//! Settings, providers/models lists, and schedule preferences.

use super::*;

pub(crate) async fn settings_get(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    let mut payload = admin::settings_json();
    if let Some(obj) = payload.as_object_mut() {
        // What is actually bound, which can differ from the configured port.
        obj.insert("actual_port".into(), json!(state.process.port()));
    }
    Json(payload)
}

pub(crate) async fn settings_save(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<admin::SettingsUpdate>,
) -> Result<Json<Value>, ApiError> {
    let saved = admin::save_settings(body)?;
    // Same shared hot-apply as the RPC arm and the desktop app: the switch must
    // affect the live gate, not just the settings file.
    admin::hot_apply_client_auth(&state.client_auth).await;
    // And the same rebuild, for the same reason: `Config` is a snapshot, so a
    // save that only writes the file leaves the data plane on the old values
    // (`model_map`, `sanitize_terms`, `intercept_safety_classifier`, upstream
    // URL). The desktop registers this hook; a server does not, and is told that
    // systemd owns the restart.
    let restart_note = match crate::desktop_hooks::call("restart", |h| h.restart_service.clone()) {
        Ok(outcome) => format!("；{outcome}"),
        Err(systemd_note) => format!("（{systemd_note}）"),
    };
    state
        .logs
        .push("INFO", format!("设置已保存{restart_note}"))
        .await;
    Ok(Json(saved))
}

pub(crate) async fn providers() -> Json<Value> {
    Json(json!({ "providers": crate::providers::builtin_presets() }))
}

pub(crate) async fn models() -> Result<Json<Value>, ApiError> {
    let settings = crate::settings::GuiSettings::load();
    let preset = settings.models_preset();
    let url = preset
        .models_url
        .clone()
        .or_else(|| preset.models_config_url.clone())
        .ok_or_else(|| ApiError::invalid("当前厂商没有可用的模型列表端点"))?;
    let client = reqwest::Client::new();
    let list = crate::providers::fetch_models(&client, &preset, &settings.api_key)
        .await
        .map_err(|e| ApiError::internal(format!("拉取模型失败: {e}")))?;
    Ok(Json(json!({ "models": list, "endpoint": url })))
}

// ── Identity pool ──────────────────────────────────────────────────────────

pub(crate) async fn preferences() -> Json<Value> {
    Json(admin::preferences_json())
}

#[derive(Deserialize)]
pub(crate) struct ScheduleBody {
    enabled: bool,
    #[serde(default)]
    time: String,
    growth: Option<bool>,
}

pub(crate) async fn preferences_checkin(
    Json(body): Json<ScheduleBody>,
) -> Result<Json<Value>, ApiError> {
    let time = if body.time.trim().is_empty() {
        "09:00".to_string()
    } else {
        body.time
    };
    Ok(Json(admin::save_schedule(
        body.enabled,
        &time,
        body.growth,
    )?))
}
