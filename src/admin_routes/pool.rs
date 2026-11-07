//! Account-pool import (preview + apply).

use super::*;

#[derive(Deserialize)]
pub(crate) struct ImportBody {
    text: String,
    #[serde(default)]
    password: String,
    #[serde(default)]
    mode: String,
}

pub(crate) async fn pool_import_preview(
    Json(body): Json<ImportBody>,
) -> Result<Json<Value>, ApiError> {
    let out = tokio::task::spawn_blocking(move || {
        admin::preview_pool(&body.text, Some(body.password.as_str()), &body.mode)
    })
    .await
    .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    Ok(Json(out))
}

pub(crate) async fn pool_import(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<ImportBody>,
) -> Result<Json<Value>, ApiError> {
    let mode = body.mode.clone();
    let out = tokio::task::spawn_blocking(move || {
        admin::apply_pool_import(&body.text, Some(body.password.as_str()), &mode)
    })
    .await
    .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    state.client_auth.reload_from_store().await;
    state.logs.push("INFO", "账号池已导入".to_string()).await;
    Ok(Json(out))
}

// ── Errors ─────────────────────────────────────────────────────────────────
