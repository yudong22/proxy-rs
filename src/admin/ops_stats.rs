//! The per-day statistics aggregate.

use super::*;

/// Today's statistics, off the async runtime.
pub async fn stats_today(stats: std::sync::Arc<StatsDb>) -> Result<Value> {
    let today = tokio::task::spawn_blocking(move || stats.query_today())
        .await
        .map_err(|e| anyhow!("统计查询异常终止: {e}"))?
        .map_err(|e| anyhow!("统计查询失败: {e}"))?;
    Ok(crate::api::stats_today_json(&today))
}

// ── Pool import / export ───────────────────────────────────────────────────
