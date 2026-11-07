//! Account-pool export and import (preview + apply).

use super::*;

/// Export the pool as text, for the browser to save.
///
/// The desktop app opens a native "save as" panel; a server has nobody at the
/// console, so the same bytes go out as an HTTP download. That is also *less*
/// privileged: the server never learns a destination path, so it needs no
/// ability to write outside its own data directory.
pub fn export_pool(password: Option<&str>) -> Result<String> {
    let opts = pool_transfer::ExportOptions {
        passphrase: password
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .map(str::to_string),
    };
    let (text, _outcome) =
        pool_transfer::export_text(&opts).map_err(|e| anyhow!("导出失败: {e}"))?;
    Ok(text)
}

/// Preview an import without writing anything.
pub fn preview_pool(text: &str, password: Option<&str>, mode: &str) -> Result<Value> {
    let mode = parse_import_mode(mode)?;
    let preview = pool_transfer::preview_import(
        text,
        password.map(str::trim).filter(|p| !p.is_empty()),
        mode,
    )
    .map_err(|e| anyhow!("导入包解析失败: {e}"))?;
    Ok(serde_json::to_value(preview).unwrap_or_else(|_| json!({})))
}

/// Apply an import. Re-decodes rather than trusting a previous preview: the
/// bundle could have been edited between the two calls, and the write must
/// reflect what is on screen now.
pub fn apply_pool_import(text: &str, password: Option<&str>, mode: &str) -> Result<Value> {
    let mode = parse_import_mode(mode)?;
    let preview = pool_transfer::apply_import(
        text,
        password.map(str::trim).filter(|p| !p.is_empty()),
        mode,
    )
    .map_err(|e| anyhow!("导入失败: {e}"))?;
    Ok(serde_json::to_value(preview).unwrap_or_else(|_| json!({})))
}

fn parse_import_mode(mode: &str) -> Result<pool_transfer::ImportMode> {
    match mode.trim().to_ascii_lowercase().as_str() {
        "" | "merge" => Ok(pool_transfer::ImportMode::Merge),
        "replace" => Ok(pool_transfer::ImportMode::Replace),
        other => bail!("未知的导入模式: {other}（可选 merge / replace）"),
    }
}

// ── DSH / points ───────────────────────────────────────────────────────────
