use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Claude Code's user settings file.
pub fn settings_path() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".claude/settings.json"))
}

/// Token written to `ANTHROPIC_AUTH_TOKEN`.
///
/// A placeholder, not a secret: the gateway resolves the real upstream
/// credential itself, and only needs the client to send *something*. The CLI
/// will not start a request without a token set, so the value exists to satisfy
/// that check and is deliberately not asked for in the GUI.
pub const PLACEHOLDER_AUTH_TOKEN: &str = "nouse";

/// Model slots Claude Code reads from `env`.
#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub struct SlotUpdate {
    /// Slot name: sonnet | opus | haiku | model
    pub slot: String,
    /// Upstream model id, e.g. `deepseek-v4.1-flash`
    pub model: String,
    /// Optional human-friendly display name
    #[serde(default)]
    pub name: Option<String>,
}

/// Read the current env map (and top-level `model`) for display.
pub fn read_current() -> Result<Value> {
    let path = settings_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    if !path.exists() {
        return Ok(json!({ "exists": false, "env": {}, "model": null }));
    }
    let text = std::fs::read_to_string(&path)?;
    let doc: Value =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("invalid JSON: {}", e))?;

    let env = doc.get("env").cloned().unwrap_or_else(|| json!({}));
    let model = doc.get("model").cloned().unwrap_or(Value::Null);

    Ok(json!({
        "exists": true,
        "path": path.display().to_string(),
        "env": env,
        "model": model,
    }))
}

/// Apply model slots, preserving every unrelated key.
pub fn apply_slots(updates: &[SlotUpdate], base_url: &str) -> Result<Value> {
    let path = settings_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;

    let mut doc: Value = if path.exists() {
        let text = std::fs::read_to_string(&path)?;
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("invalid JSON: {}", e))?
    } else {
        json!({ "env": {} })
    };

    if !doc.is_object() {
        bail!("settings.json must be a JSON object");
    }

    let env = doc
        .as_object_mut()
        .and_then(|o| o.get_mut("env"))
        .and_then(Value::as_object_mut)
        .ok_or_else(|| anyhow::anyhow!("settings.json has no `env` object"))?;

    apply_env_updates(env, updates, base_url)?;

    // Create a backup so a bad edit is recoverable.
    if path.exists() {
        let backup = path.with_extension("json.bak");
        let _ = std::fs::copy(&path, backup);
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;

    Ok(json!({ "ok": true, "path": path.display().to_string() }))
}

/// Write the model slots and gateway wiring into Claude Code's `env` map.
///
/// Pure, so the exact keys written can be asserted directly. Split out from
/// [`apply_slots`] because that function's only other job is file I/O against a
/// fixed `$HOME` path.
fn apply_env_updates(
    env: &mut serde_json::Map<String, Value>,
    updates: &[SlotUpdate],
    base_url: &str,
) -> Result<()> {
    for u in updates {
        match u.slot.as_str() {
            "model" => {
                env.insert("ANTHROPIC_MODEL".into(), json!(u.model));
            }
            "sonnet" | "opus" | "haiku" => {
                let key = format!("ANTHROPIC_DEFAULT_{}_MODEL", u.slot.to_uppercase());
                let name_key = format!("{}_NAME", key);
                env.insert(key, json!(u.model));
                env.insert(
                    name_key,
                    json!(u.name.clone().unwrap_or_else(|| u.model.clone())),
                );
            }
            other => bail!("unknown model slot: {}", other),
        }
    }

    // Both of these are gateway wiring rather than a user choice, so they are
    // written on every apply and deliberately not shown in the GUI: the URL must
    // track whatever port the proxy is actually serving on, and the token is
    // only there because the CLI refuses to send a request without one (the
    // gateway resolves the real upstream credential itself).
    env.insert("ANTHROPIC_BASE_URL".into(), json!(base_url));
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(PLACEHOLDER_AUTH_TOKEN));

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env_with(pairs: &[(&str, &str)]) -> serde_json::Map<String, Value> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), json!(v)))
            .collect()
    }

    fn slot(name: &str, model: &str) -> SlotUpdate {
        SlotUpdate {
            slot: name.to_string(),
            model: model.to_string(),
            name: None,
        }
    }

    #[test]
    fn gateway_wiring_is_written_alongside_the_models() {
        let mut env = env_with(&[("ANTHROPIC_MODEL", "old")]);
        apply_env_updates(
            &mut env,
            &[slot("model", "deepseek-v4.1-flash")],
            "http://127.0.0.1:3457",
        )
        .unwrap();

        assert_eq!(env["ANTHROPIC_MODEL"], "deepseek-v4.1-flash");
        // Both are required for the CLI to reach the gateway at all, and are
        // written without being asked for in the GUI.
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:3457");
        assert_eq!(env["ANTHROPIC_AUTH_TOKEN"], PLACEHOLDER_AUTH_TOKEN);
    }

    #[test]
    fn base_url_tracks_the_port_it_is_given() {
        // The URL must follow the gateway's port, not a fixed default.
        for port in [3456u16, 3457, 8080] {
            let mut env = env_with(&[]);
            apply_env_updates(
                &mut env,
                &[slot("model", "m")],
                &format!("http://127.0.0.1:{port}"),
            )
            .unwrap();
            assert_eq!(
                env["ANTHROPIC_BASE_URL"],
                format!("http://127.0.0.1:{port}")
            );
        }
    }

    #[test]
    fn wiring_is_rewritten_on_every_apply() {
        // A stale URL from an earlier port must not survive.
        let mut env = env_with(&[("ANTHROPIC_BASE_URL", "http://127.0.0.1:9999")]);
        apply_env_updates(&mut env, &[slot("model", "m")], "http://127.0.0.1:3457").unwrap();
        assert_eq!(env["ANTHROPIC_BASE_URL"], "http://127.0.0.1:3457");
    }

    #[test]
    fn unrelated_env_keys_are_preserved() {
        let mut env = env_with(&[("SOME_OTHER_TOOL", "keep-me")]);
        apply_env_updates(&mut env, &[slot("model", "m")], "http://127.0.0.1:3457").unwrap();
        assert_eq!(env["SOME_OTHER_TOOL"], "keep-me");
    }

    #[test]
    fn model_slots_write_their_default_and_name_keys() {
        let mut env = env_with(&[]);
        apply_env_updates(
            &mut env,
            &[slot("sonnet", "glm-5.3"), slot("opus", "kimi-k2.7")],
            "http://127.0.0.1:3457",
        )
        .unwrap();

        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "glm-5.3");
        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"], "glm-5.3");
        assert_eq!(env["ANTHROPIC_DEFAULT_OPUS_MODEL"], "kimi-k2.7");
        // No top-level model was requested, so none is invented.
        assert!(env.get("ANTHROPIC_MODEL").is_none());
    }

    #[test]
    fn an_explicit_display_name_wins_over_the_model_id() {
        let mut env = env_with(&[]);
        apply_env_updates(
            &mut env,
            &[SlotUpdate {
                slot: "sonnet".to_string(),
                model: "glm-5.3".to_string(),
                name: Some("GLM 5.3".to_string()),
            }],
            "http://127.0.0.1:3457",
        )
        .unwrap();

        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL"], "glm-5.3");
        assert_eq!(env["ANTHROPIC_DEFAULT_SONNET_MODEL_NAME"], "GLM 5.3");
    }

    #[test]
    fn an_unknown_slot_is_rejected() {
        let mut env = env_with(&[]);
        let err = apply_env_updates(&mut env, &[slot("bogus", "m")], "http://127.0.0.1:3457");
        assert!(err.is_err(), "an unknown slot must not be silently ignored");
    }
}
