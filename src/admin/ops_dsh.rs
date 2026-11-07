//! Reading and writing the DeepSeek Harness config.

use super::*;

/// Report the DSH provider wiring so the console can show the current state.
pub fn read_dsh_config() -> Result<Value> {
    let Some(patch_path) = crate::dsh_config::patch_path() else {
        return Ok(json!({
            "supported": false,
            "reason": "DSH_HOME / HOME is not set",
        }));
    };
    let credentials_path = crate::dsh_config::credentials_path();
    if !patch_path.exists() {
        return Ok(json!({
            "supported": true,
            "profile": crate::dsh_config::active_profile(),
            "settings_path": patch_path.display().to_string(),
            "settings_exists": false,
            "provider_exists": false,
            "model_count": 0,
            "base_url": null,
            "credential_present": false,
            "session_header": false,
        }));
    }
    let settings_text =
        std::fs::read_to_string(&patch_path).map_err(|e| anyhow!("读取 DSH 配置失败: {e}"))?;
    let credentials_text = credentials_path
        .as_ref()
        .and_then(|p| std::fs::read_to_string(p).ok());
    let state = crate::dsh_config::read_state(&settings_text, credentials_text.as_deref());
    Ok(json!({
        "supported": true,
        "profile": crate::dsh_config::active_profile(),
        "settings_path": patch_path.display().to_string(),
        "settings_exists": true,
        "provider_exists": state.provider_exists,
        "model_count": state.model_ids.len(),
        "base_url": state.base_url,
        "credential_present": state.credential_present,
        "session_header": state.session_header,
    }))
}

/// Write this proxy into the active DSH profile's patch file.
pub async fn apply_dsh_config(
    state: &std::sync::Arc<crate::admin_state::AdminState>,
) -> Result<Value> {
    let Some(patch_path) = crate::dsh_config::patch_path() else {
        bail!("DSH_HOME / HOME 未设置，无法定位 DSH 配置");
    };
    let Some(credentials_path) = crate::dsh_config::credentials_path() else {
        bail!("DSH_HOME / HOME 未设置，无法定位 DSH 凭据文件");
    };
    let settings = GuiSettings::load();
    let preset = settings.models_preset();
    if settings.api_key.is_empty() {
        bail!("尚未配置 API Key，无法拉取模型列表");
    }
    let client = reqwest::Client::new();
    let models = crate::providers::fetch_models(&client, &preset, &settings.api_key)
        .await
        .map_err(|e| anyhow!("从上游 {} 拉取模型失败，未改动 DSH 配置: {e}", preset.id))?;
    if models.is_empty() {
        bail!("上游 {} 未返回任何模型，未改动 DSH 配置", preset.id);
    }
    let base_url = format!("http://127.0.0.1:{}/v1", state.process.port());
    let dsh_models: Vec<crate::dsh_config::DshModel> =
        models.iter().map(crate::dsh_config::to_dsh_model).collect();
    let (patch, credentials, credential_added) =
        crate::dsh_config::apply(&patch_path, &credentials_path, &base_url, &dsh_models)
            .map_err(|e| anyhow!("写入 DSH 配置失败: {e}"))?;
    Ok(json!({
        "ok": true,
        "models": dsh_models.len(),
        "profile": crate::dsh_config::active_profile(),
        "settings_path": patch.display().to_string(),
        "credentials_path": credentials.display().to_string(),
        "credential_added": credential_added,
    }))
}
