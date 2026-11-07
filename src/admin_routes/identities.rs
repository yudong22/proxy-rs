//! The credential / api-key pool, plus the per-identity automation triggers.

use super::*;

pub(crate) async fn identities(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    Json(admin::identities_json(&state.pool).await)
}

#[derive(Deserialize)]
pub(crate) struct DefaultBody {
    id: String,
}

pub(crate) async fn identity_set_default(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<DefaultBody>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::set_default_identity(&body.id)?;
    // The pool is rebuilt so the change takes effect on the next request without
    // a restart. Without this the console would report success while traffic
    // kept using the old default until the unit was restarted.
    state.pool.reload_from_store().await;
    state
        .logs
        .push("INFO", format!("已设置默认身份: {}", body.id))
        .await;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub(crate) struct OrderBody {
    order: Vec<String>,
}

pub(crate) async fn identity_reorder(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<OrderBody>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::reorder_identities(&body.order)?;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

pub(crate) async fn sticky_reset(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    // Clears every session's relocated-credential binding, so the next request
    // for each session goes back to the default identity.
    let before = state.pool.sticky_count().await;
    state.pool.reset_sticky().await;
    Json(json!({ "ok": true, "cleared": before }))
}

#[derive(Deserialize)]
pub(crate) struct CredentialAddBody {
    login_state: String,
    #[serde(default)]
    label: String,
}

pub(crate) async fn credential_add(
    Extension(state): Extension<Arc<AdminState>>,
    Json(body): Json<CredentialAddBody>,
) -> Result<Json<Value>, ApiError> {
    let label = body.label.clone();
    let out = tokio::task::spawn_blocking(move || admin::add_credential(&body.login_state, &label))
        .await
        .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

pub(crate) async fn credential_delete(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = tokio::task::spawn_blocking(move || admin::delete_credential(&id))
        .await
        .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub(crate) struct EnabledBody {
    pub(crate) enabled: bool,
}

pub(crate) async fn credential_patch(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Json(body): Json<EnabledBody>,
) -> Result<Json<Value>, ApiError> {
    let out = tokio::task::spawn_blocking(move || admin::set_credential_enabled(&id, body.enabled))
        .await
        .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

#[derive(Deserialize)]
pub(crate) struct ReloginBody {
    login_state: String,
}

pub(crate) async fn credential_relogin(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Json(body): Json<ReloginBody>,
) -> Result<Json<Value>, ApiError> {
    let out =
        tokio::task::spawn_blocking(move || admin::relogin_credential(&id, &body.login_state))
            .await
            .map_err(|e| ApiError::internal(format!("任务异常终止: {e}")))??;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

pub(crate) async fn credential_checkin(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let client = reqwest::Client::new();
    let credentials = crate::workbuddy_auth::load_credentials();
    let cred = credentials
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    // Forced: a manual click means "do it now", not "skip because today's slot
    // is recorded". The scheduled loop is the caller that must respect the slot.
    let result = crate::workbuddy_auth::claim_daily_checkin_forced(&client, cred).await;
    state
        .logs
        .push("INFO", format!("手动打卡 {}: {}", id, result.message))
        .await;
    Ok(Json(json!({ "ok": true, "result": result })))
}

pub(crate) async fn credential_growth(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let client = reqwest::Client::new();
    let credentials = crate::workbuddy_auth::load_credentials();
    let cred = credentials
        .iter()
        .find(|c| c.id == id)
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    let run = crate::growth_runner::run_account(&client, cred, None, false).await;
    // Built here rather than reusing `summarize` (which takes the whole pool's
    // report): one account's outcome is a different sentence, and a per-account
    // route that answered with a pool-wide summary would be actively confusing.
    let summary = format!(
        "{}: 接取 {} 个，领奖 {} 个，获得 {} 积分，{} 个任务未推进",
        run.label,
        run.accepted,
        run.claimed.len(),
        run.credits_earned,
        run.stalled.len()
    );
    state
        .logs
        .push("INFO", format!("手动成长任务 {}: {}", id, summary))
        .await;
    Ok(Json(json!({ "ok": true, "summary": summary })))
}

pub(crate) async fn key_add(Json(body): Json<KeyAddBody>) -> Result<Json<Value>, ApiError> {
    Ok(Json(admin::add_key(&body.key, &body.label)?))
}

#[derive(Deserialize)]
pub(crate) struct KeyAddBody {
    key: String,
    #[serde(default)]
    label: String,
}

pub(crate) async fn key_delete(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::delete_key(&id)?;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}

pub(crate) async fn key_patch(
    Extension(state): Extension<Arc<AdminState>>,
    Path(id): Path<String>,
    Json(body): Json<EnabledBody>,
) -> Result<Json<Value>, ApiError> {
    let out = admin::set_key_enabled(&id, body.enabled)?;
    state.pool.reload_from_store().await;
    Ok(Json(out))
}
