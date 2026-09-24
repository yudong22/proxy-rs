//! Codex model-catalog generation.
//!
//! Codex CLI/Desktop ignores a custom provider's `GET /v1/models` response when
//! building its model picker: `OpenAiModelsManager::refresh_available_models`
//! only discovers remote models for providers that authenticate with an API key
//! *and* advertise `model_catalog_url` (or are first-party OpenAI without a
//! `base_url`). A generic OpenAI-compatible provider therefore keeps Codex's
//! bundled catalog, which is why the picker shows `gpt-5.6-luna` and friends
//! instead of the models the configured provider actually serves.
//!
//! The supported lever is the user-level `model_catalog_json` key, which
//! replaces the bundled catalog at startup. This module turns the provider's
//! model list into a catalog Codex accepts, and wires the path into
//! `~/.codex/config.toml` without disturbing unrelated keys.
//!
//! Catalog entries must satisfy `ModelInfo`'s required fields; Codex fails the
//! whole load (`missing field ...`) rather than skipping a malformed entry, so
//! every entry is emitted complete.

use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};

/// Codex's user config file, honoring `CODEX_HOME`.
pub fn config_path() -> Option<PathBuf> {
    let home = std::env::var("CODEX_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .map(|h| PathBuf::from(h).join(".codex"))
        })?;
    Some(home.join("config.toml"))
}

/// Where the generated catalog is written. Kept next to Codex's own config so
/// the path stays stable across proxy restarts.
pub fn catalog_path() -> Option<PathBuf> {
    config_path().map(|p| p.with_file_name("proxy-rs-model-catalog.json"))
}

/// One `ModelInfo` entry as Codex's catalog parser expects it.
#[derive(Debug, Clone, Serialize)]
struct CatalogModel {
    slug: String,
    display_name: String,
    description: String,
    base_instructions: String,
    shell_type: &'static str,
    visibility: &'static str,
    supported_in_api: bool,
    priority: i32,
    supported_reasoning_levels: Vec<ReasoningLevel>,
    default_reasoning_level: &'static str,
    default_reasoning_summary: &'static str,
    truncation_policy: TruncationPolicy,
    experimental_supported_tools: Vec<String>,
    context_window: u64,
    max_context_window: u64,
    support_verbosity: bool,
    apply_patch_tool_type: &'static str,
    web_search_tool_type: &'static str,
    input_modalities: Vec<&'static str>,
    supports_image_detail_original: bool,
    supports_parallel_tool_calls: bool,
    tool_mode: Option<&'static str>,
    multi_agent_version: Option<&'static str>,
    use_responses_lite: bool,
    include_skills_usage_instructions: bool,
    include_apps_usage_instructions: bool,
    include_plugin_usage_instructions: bool,
    node_repl_auto_review_required: bool,
    node_repl_disabled: bool,
    auto_review_model_override: Option<String>,
    model_specialty: Option<String>,
    upgrade: Option<serde_json::Value>,
    availability_nux: Option<serde_json::Value>,
    additional_speed_tiers: Vec<String>,
    service_tiers: Vec<serde_json::Value>,
    default_service_tier: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct ReasoningLevel {
    effort: &'static str,
    description: &'static str,
}

#[derive(Debug, Clone, Serialize)]
struct TruncationPolicy {
    mode: &'static str,
    limit: i64,
}

#[derive(Debug, Serialize)]
struct Catalog {
    models: Vec<CatalogModel>,
}

/// Baseline context window used when a provider does not report one.
const DEFAULT_CONTEXT_WINDOW: u64 = 272_000;

const REASONING_LEVELS: [(&str, &str); 4] = [
    ("low", "Fast responses with lighter reasoning"),
    (
        "medium",
        "Balances speed and reasoning depth for everyday tasks",
    ),
    ("high", "Greater reasoning depth for complex problems"),
    ("xhigh", "Extra high reasoning depth for complex problems"),
];

/// Build one catalog entry for a provider model id.
fn build_model(id: &str, name: Option<&str>, context_window: Option<u64>) -> CatalogModel {
    let ctx = context_window.unwrap_or(DEFAULT_CONTEXT_WINDOW).max(1);
    CatalogModel {
        slug: id.to_string(),
        display_name: name.unwrap_or(id).to_string(),
        description: format!("{} via proxy-rs", name.unwrap_or(id)),
        // Codex requires instructions per model; the generic Codex prompt is the
        // safe default for third-party models.
        base_instructions: "You are Codex, a coding agent. You and the user share the same \
                            workspace and collaborate to achieve the user's goals."
            .to_string(),
        shell_type: "unified_exec",
        visibility: "list",
        supported_in_api: true,
        priority: 1,
        supported_reasoning_levels: REASONING_LEVELS
            .iter()
            .map(|(effort, description)| ReasoningLevel {
                effort,
                description,
            })
            .collect(),
        default_reasoning_level: "medium",
        default_reasoning_summary: "none",
        truncation_policy: TruncationPolicy {
            mode: "tokens",
            limit: 10_000,
        },
        experimental_supported_tools: Vec::new(),
        context_window: ctx,
        max_context_window: ctx,
        support_verbosity: false,
        apply_patch_tool_type: "freeform",
        web_search_tool_type: "text",
        input_modalities: vec!["text", "image"],
        supports_image_detail_original: false,
        supports_parallel_tool_calls: true,
        tool_mode: None,
        multi_agent_version: None,
        use_responses_lite: false,
        include_skills_usage_instructions: false,
        include_apps_usage_instructions: true,
        include_plugin_usage_instructions: false,
        node_repl_auto_review_required: false,
        node_repl_disabled: false,
        auto_review_model_override: None,
        model_specialty: None,
        upgrade: None,
        availability_nux: None,
        additional_speed_tiers: Vec::new(),
        service_tiers: Vec::new(),
        default_service_tier: None,
    }
}

/// Render a Codex catalog JSON document for the given models.
pub fn render_catalog(models: &[crate::providers::GuiModel]) -> Result<String> {
    let entries = models
        .iter()
        .map(|m| build_model(&m.id, m.name.as_deref(), m.context_window))
        .collect();
    serde_json::to_string_pretty(&Catalog { models: entries })
        .context("failed to serialize Codex model catalog")
}

/// Write the catalog and point `model_catalog_json` at it.
///
/// Returns `(catalog_path, config_path)`.
pub fn write_catalog(
    models: &[crate::providers::GuiModel],
    config_path: &Path,
) -> Result<(PathBuf, PathBuf)> {
    if models.is_empty() {
        anyhow::bail!("refusing to write an empty Codex model catalog");
    }

    let catalog_path = config_path
        .parent()
        .map(|dir| dir.join("proxy-rs-model-catalog.json"))
        .ok_or_else(|| anyhow::anyhow!("Codex config path has no parent directory"))?;

    let json = render_catalog(models)?;
    if let Some(dir) = catalog_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&catalog_path, json)
        .with_context(|| format!("failed to write {}", catalog_path.display()))?;

    set_catalog_key(config_path, &catalog_path)?;

    Ok((catalog_path, config_path.to_path_buf()))
}

/// Point `model_catalog_json` at `catalog_path`, preserving every other key.
///
/// Codex reads `model_catalog_json` only from the user-level config, so this
/// edits that file in place rather than rewriting it: unrelated TOML (MCP
/// servers, plugins, projects) must survive untouched.
pub fn set_catalog_key(config_path: &Path, catalog_path: &Path) -> Result<()> {
    let value = catalog_path.display().to_string();
    let existing = if config_path.exists() {
        std::fs::read_to_string(config_path)?
    } else {
        String::new()
    };

    if config_path.exists() {
        let backup = config_path.with_extension("toml.bak");
        let _ = std::fs::copy(config_path, backup);
    }

    let rendered = upsert_toml_string(&existing, "model_catalog_json", &value);
    if let Some(dir) = config_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(config_path, rendered)
        .with_context(|| format!("failed to write {}", config_path.display()))?;
    Ok(())
}

/// Replace or insert a top-level `key = "value"` pair.
///
/// A hand-rolled upsert keeps the user's file byte-for-byte outside the target
/// line, which a TOML round-trip through a serializer would not.
fn upsert_toml_string(text: &str, key: &str, value: &str) -> String {
    let line = format!("{} = {}", key, toml_basic_string(value));

    let mut out: Vec<String> = Vec::new();
    let mut replaced = false;
    let mut in_table = false;

    for raw in text.lines() {
        let trimmed = raw.trim_start();
        if trimmed.starts_with('[') {
            // Top-level keys must precede the first `[table]` header.
            in_table = true;
        }
        let is_target = !in_table
            && !replaced
            && trimmed
                .split_once('=')
                .map(|(k, _)| k.trim() == key)
                .unwrap_or(false);
        if is_target {
            out.push(line.clone());
            replaced = true;
            continue;
        }
        out.push(raw.to_string());
    }

    if !replaced {
        // Insert above the first table header so the key stays top-level.
        let insert_at = out
            .iter()
            .position(|l| l.trim_start().starts_with('['))
            .unwrap_or(out.len());
        // Keep a blank separator line when injecting before a table.
        let mut block = Vec::new();
        if insert_at > 0 && !out[insert_at - 1].trim().is_empty() {
            block.push(String::new());
        }
        block.push(line);
        if insert_at < out.len() {
            block.push(String::new());
        }
        out.splice(insert_at..insert_at, block);
    }

    let mut rendered = out.join("\n");
    if text.ends_with('\n') || rendered.is_empty() {
        rendered.push('\n');
    }
    rendered
}

/// Quote a TOML basic string.
fn toml_basic_string(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len() + 2);
    escaped.push('"');
    for ch in value.chars() {
        match ch {
            '"' => escaped.push_str("\\\""),
            '\\' => escaped.push_str("\\\\"),
            '\n' => escaped.push_str("\\n"),
            '\r' => escaped.push_str("\\r"),
            '\t' => escaped.push_str("\\t"),
            c if (c as u32) < 0x20 => escaped.push_str(&format!("\\u{:04X}", c as u32)),
            c => escaped.push(c),
        }
    }
    escaped.push('"');
    escaped
}

/// Read back the catalog path currently configured, if any.
pub fn current_catalog_path(config_path: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(config_path).ok()?;
    for raw in text.lines() {
        let trimmed = raw.trim();
        if trimmed.starts_with('[') {
            break; // top-level keys only
        }
        let (k, v) = match trimmed.split_once('=') {
            Some(pair) => pair,
            None => continue,
        };
        if k.trim() != "model_catalog_json" {
            continue;
        }
        let v = v.trim();
        let unquoted = v
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .unwrap_or(v);
        return Some(PathBuf::from(unquoted));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::providers::GuiModel;

    fn model(id: &str) -> GuiModel {
        GuiModel {
            id: id.to_string(),
            name: None,
            context_window: None,
            max_output_tokens: None,
            supports_images: None,
            supports_reasoning: None,
        }
    }

    #[test]
    fn catalog_contains_one_entry_per_provider_model() {
        let json = render_catalog(&[model("hy3"), model("glm-5.3")]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let models = parsed["models"].as_array().unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0]["slug"], "hy3");
        assert_eq!(models[1]["slug"], "glm-5.3");
    }

    #[test]
    fn every_entry_carries_fields_codex_requires() {
        let json = render_catalog(&[model("hy3")]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        let entry = &parsed["models"][0];
        // Codex rejects the entire catalog when any required field is absent.
        for key in [
            "slug",
            "display_name",
            "description",
            "base_instructions",
            "shell_type",
            "visibility",
            "supported_in_api",
            "priority",
            "supported_reasoning_levels",
            "truncation_policy",
            "experimental_supported_tools",
            "context_window",
            "support_verbosity",
        ] {
            assert!(!entry[key].is_null(), "{} missing from catalog entry", key);
        }
        assert_eq!(entry["visibility"], "list");
        assert_eq!(entry["supported_in_api"], true);
    }

    #[test]
    fn provider_context_window_is_preserved() {
        let mut m = model("kimi-k2.7");
        m.context_window = Some(512_000);
        let json = render_catalog(&[m]).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["models"][0]["context_window"], 512_000);
        assert_eq!(parsed["models"][0]["max_context_window"], 512_000);
    }

    #[test]
    fn display_name_falls_back_to_id() {
        let json = render_catalog(&[model("hy3")]).unwrap();
        assert!(json.contains("\"display_name\": \"hy3\""));
    }

    #[test]
    fn empty_model_list_is_rejected() {
        assert!(render_catalog(&[]).is_ok());
        let dir = tempdir();
        let cfg = dir.join("config.toml");
        assert!(write_catalog(&[], &cfg).is_err());
    }

    #[test]
    fn upsert_inserts_key_before_first_table() {
        let text = "model = \"hy3\"\n\n[model_providers.custom]\nbase_url = \"x\"\n";
        let out = upsert_toml_string(text, "model_catalog_json", "/tmp/cat.json");
        let lines: Vec<&str> = out.lines().collect();
        let key_at = lines
            .iter()
            .position(|l| l.starts_with("model_catalog_json"))
            .unwrap();
        let table_at = lines
            .iter()
            .position(|l| l.starts_with("[model_providers.custom]"))
            .unwrap();
        assert!(key_at < table_at, "key must stay above the first table");
        assert!(out.contains("model = \"hy3\""));
        assert!(out.contains("base_url = \"x\""));
    }

    #[test]
    fn upsert_replaces_existing_key() {
        let text = "model_catalog_json = \"/old.json\"\nmodel = \"hy3\"\n";
        let out = upsert_toml_string(text, "model_catalog_json", "/new.json");
        assert!(out.contains("model_catalog_json = \"/new.json\""));
        assert!(!out.contains("/old.json"));
        assert_eq!(
            out.matches("model_catalog_json").count(),
            1,
            "must not duplicate the key"
        );
    }

    #[test]
    fn upsert_ignores_lookalike_key_inside_a_table() {
        let text = "[tui]\nmodel_catalog_json = \"keep-me\"\n";
        let out = upsert_toml_string(text, "model_catalog_json", "/new.json");
        // The table-scoped key is untouched and a top-level key is added.
        assert!(out.contains("[tui]\nmodel_catalog_json = \"keep-me\""));
        assert_eq!(out.matches("model_catalog_json").count(), 2);
        let top_level = out
            .lines()
            .take_while(|l| !l.trim_start().starts_with('['))
            .any(|l| l.contains("/new.json"));
        assert!(top_level);
    }

    #[test]
    fn upsert_preserves_unrelated_sections() {
        let text = "model = \"hy3\"\n\n[mcp_servers.node_repl]\ncommand = \"/bin/node\"\n\n[projects.\"/tmp/x\"]\ntrust_level = \"trusted\"\n";
        let out = upsert_toml_string(text, "model_catalog_json", "/tmp/cat.json");
        assert!(out.contains("[mcp_servers.node_repl]"));
        assert!(out.contains("command = \"/bin/node\""));
        assert!(out.contains("[projects.\"/tmp/x\"]"));
        assert!(out.contains("trust_level = \"trusted\""));
    }

    #[test]
    fn toml_string_escapes_quotes_and_backslashes() {
        assert_eq!(toml_basic_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(toml_basic_string(r"a\b"), r#""a\\b""#);
    }

    #[test]
    fn current_catalog_path_reads_back_written_value() {
        let dir = tempdir();
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, "model = \"hy3\"\n").unwrap();
        let models = [model("hy3")];
        let (catalog, _) = write_catalog(&models, &cfg).unwrap();
        assert!(catalog.exists());
        let read = current_catalog_path(&cfg).unwrap();
        assert_eq!(read, catalog);
        assert!(catalog
            .to_string_lossy()
            .contains("proxy-rs-model-catalog.json"));
    }

    #[test]
    fn write_catalog_backs_up_existing_config() {
        let dir = tempdir();
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, "model = \"hy3\"\n").unwrap();
        write_catalog(&[model("hy3")], &cfg).unwrap();
        assert!(cfg.with_extension("toml.bak").exists());
    }

    /// Minimal temp-dir helper so the module stays free of extra dev-deps.
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "proxy-rs-codex-config-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
