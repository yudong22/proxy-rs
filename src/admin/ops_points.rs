//! Balance refresh for one identity or all of them.

use super::*;

/// Refresh every identity's point balance.
pub async fn refresh_all_points(
    state: &std::sync::Arc<crate::admin_state::AdminState>,
) -> Result<Value> {
    let report = workbuddy_auth::refresh_all_points(&state.client).await;
    let known = report.results.iter().filter(|r| r.points.is_some()).count();
    state
        .logs
        .push(
            "INFO",
            format!(
                "已刷新身份池积分: 共 {} 个（账号+密钥），{} 个读取成功",
                report.results.len(),
                known
            ),
        )
        .await;
    Ok(json!({
        "results": report.results,
        "current_identity": current_identity_summary(),
    }))
}

/// Refresh one identity's point balance.
pub async fn refresh_one_points(
    state: &std::sync::Arc<crate::admin_state::AdminState>,
    id: &str,
) -> Result<Value> {
    let Some(result) = workbuddy_auth::refresh_one_points(&state.client, id).await else {
        bail!("身份不存在: {id}");
    };
    state
        .logs
        .push(
            "INFO",
            format!(
                "已刷新积分: {} ({}) → {}",
                result.label,
                result.id,
                match result.points {
                    Some(p) => format!("{p:.2}"),
                    None => "读取失败".to_string(),
                }
            ),
        )
        .await;
    Ok(json!({
        "result": result,
        "current_identity": current_identity_summary(),
    }))
}

// ── Local client sessions (Claude Code) ───────────────────────────────────
