//! Inbound client access-key accounts.

use super::*;

pub(crate) async fn client_accounts_get(
    Extension(state): Extension<Arc<AdminState>>,
) -> Json<Value> {
    Json(admin::client_accounts_json(&state.client_auth).await)
}

#[derive(Deserialize)]
pub(crate) struct CreateClientAccountBody {
    label: String,
    #[serde(default)]
    note: String,
}

pub(crate) async fn client_accounts_create(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<CreateClientAccountBody>,
) -> Result<Json<Value>, ApiError> {
    let out =
        tokio::task::spawn_blocking(move || admin::create_client_account(&body.label, &body.note))
            .await
            .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.client_auth.reload_from_store().await;
    Ok(Json(out))
}

pub(crate) async fn client_accounts_patch(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Json(body): Json<EnabledBody>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::set_client_account_enabled(&id, body.enabled)?;
    state.client_auth.reload_from_store().await;
    Ok(Json(out))
}

pub(crate) async fn client_accounts_reissue(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = tokio::task::spawn_blocking(move || admin::reissue_client_account_key(&id))
        .await
        .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.client_auth.reload_from_store().await;
    Ok(Json(out))
}

pub(crate) async fn client_accounts_delete(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::delete_client_account(&id)?;
    state.client_auth.reload_from_store().await;
    Ok(Json(out))
}

// ── Pool import ────────────────────────────────────────────────────────────
//
// Export is not here: it is a browser download served by `crate::console`. The
// desktop app used a native "save as" panel and a server path; the console needs
// neither, and giving the server a write-anywhere capability for it would be a
// privilege it never uses.
