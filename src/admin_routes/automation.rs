//! Batch automation triggers: check-in all, growth all, growth preview.

use super::*;

pub(crate) async fn checkin_all(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    let client = reqwest::Client::new();
    let report = crate::workbuddy_auth::batch_claim_daily_checkin_forced(&client).await;
    let summary = format!(
        "手动打卡完成: 共 {} 个账号，成功 {} 个，已打卡 {} 个，失败 {} 个",
        report.total, report.success, report.already_checked_in, report.failed
    );
    state.logs.push("INFO", summary.clone()).await;
    Json(json!({ "ok": true, "summary": summary, "report": report }))
}

pub(crate) async fn growth_all(Extension(state): Extension<Arc<AdminState>>) -> Json<Value> {
    let client = reqwest::Client::new();
    // Enabled accounts only: a manual "run now" must not re-enable work the
    // operator switched off per account.
    let credentials: Vec<_> = crate::workbuddy_auth::load_credentials()
        .into_iter()
        .filter(|c| c.enabled)
        .collect();
    let report = crate::growth_runner::run_pool(&client, &credentials, None, false).await;
    let summary = crate::growth_runner::summarize(&report);
    state
        .logs
        .push("INFO", format!("手动成长任务: {summary}"))
        .await;
    Json(json!({ "ok": true, "summary": summary }))
}

#[derive(Deserialize)]
pub(crate) struct GrowthPreviewQuery {
    id: String,
}

/// What the growth automation would find for one account, without running it.
pub(crate) async fn growth_preview(
    Query(q): Query<GrowthPreviewQuery>,
) -> Result<Json<Value>, ApiError> {
    let credentials = crate::workbuddy_auth::load_credentials();
    let cred = credentials
        .iter()
        .find(|c| c.id == q.id)
        .ok_or_else(|| ApiError::not_found("账号不存在"))?;
    let client = reqwest::Client::new();
    let growth = crate::growth::GrowthClient::new(&client, None);
    let tasks = growth
        .tasks(cred)
        .await
        .map_err(|e| ApiError::internal(format!("拉取成长任务失败: {e}")))?;
    let energy = growth.energy(cred).await.ok();
    Ok(Json(json!({
        "tasks": tasks,
        "energy": energy,
        "earnable": tasks.iter().filter(|t| t.is_actionable()).map(|t| t.reward).sum::<u32>(),
    })))
}

// ── Inbound access accounts ────────────────────────────────────────────────
