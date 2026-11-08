use anyhow::{bail, Result};
use serde_json::{json, Value};
use std::path::PathBuf;

/// Claude Code's user settings file.
pub fn settings_path() -> Option<PathBuf> {
    let home = crate::settings::home_dir()?;
    Some(home.join(".claude/settings.json"))
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

/// `permissions.defaultMode` values Claude Code accepts, in the order the GUI
/// offers them, each with the one-line behaviour the UI shows.
///
/// This list is the whole surface of "how often does Claude ask before it
/// acts". It is deliberately explicit rather than a free-text field: the CLI
/// silently ignores an unrecognised mode and falls back to its built-in
/// default, so a typo here would read as "the setting did nothing".
///
/// `auto` and `bypassPermissions` only take effect from *user* settings
/// (`~/.claude/settings.json`, which is the file this module writes) — since
/// v2.1.257 the CLI ignores both in project/local settings.
///
/// `manual` is a real CLI alias for `default`, but it is deliberately not
/// offered and not accepted here: two spellings of one behaviour would make the
/// form show a mode it then could not round-trip, and `default` is the spelling
/// the settings reference documents as the canonical one.
pub const PERMISSION_MODES: &[(&str, &str)] = &[
    ("", "跟随 Claude Code 默认（不写入）"),
    ("default", "每次都询问（Manual）"),
    ("acceptEdits", "自动接受文件编辑，其余仍询问"),
    ("plan", "只读规划，不改文件（Plan）"),
    ("auto", "分类器逐条判定（Auto，会产生额外请求）"),
    ("dontAsk", "不会询问：会弹窗的一律直接拒绝"),
    (
        "bypassPermissions",
        "全部自动通过（Bypass，不再有任何询问）",
    ),
];

/// True for a value [`PERMISSION_MODES`] lists, so the writer and the reader
/// cannot disagree about what is legal.
fn is_known_mode(mode: &str) -> bool {
    mode.is_empty() || PERMISSION_MODES.iter().any(|(m, _)| *m == mode)
}

/// Read the current env map (and top-level `model`) for display.
pub fn read_current() -> Result<Value> {
    let path = settings_path().ok_or_else(|| anyhow::anyhow!("HOME is not set"))?;
    if !path.exists() {
        return Ok(json!({
            "exists": false,
            "env": {},
            "model": null,
            "permission_mode": Value::Null,
            "skip_dangerous_prompt": false,
        }));
    }
    let text = std::fs::read_to_string(&path)?;
    let doc: Value =
        serde_json::from_str(&text).map_err(|e| anyhow::anyhow!("invalid JSON: {}", e))?;

    let env = doc.get("env").cloned().unwrap_or_else(|| json!({}));
    let model = doc.get("model").cloned().unwrap_or(Value::Null);

    // `permissions.defaultMode` and `skipDangerousModePermissionPrompt` are read
    // straight out of the same document the writer edits, so the form shows what
    // is really on disk rather than what the last click asked for.
    let permission_mode = doc
        .pointer("/permissions/defaultMode")
        .cloned()
        .unwrap_or(Value::Null);
    let skip_dangerous_prompt = doc
        .get("skipDangerousModePermissionPrompt")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    Ok(json!({
        "exists": true,
        "path": path.display().to_string(),
        "env": env,
        "model": model,
        "permission_mode": permission_mode,
        "skip_dangerous_prompt": skip_dangerous_prompt,
    }))
}

/// Apply model slots, preserving every unrelated key.
pub fn apply_slots(updates: &[SlotUpdate], base_url: &str) -> Result<Value> {
    apply_slots_with_permission(updates, base_url, None)
}

/// As [`apply_slots`], plus an optional `permissions.defaultMode` to write.
///
/// `permission_mode` is `None` when the form means "leave the CLI's own default
/// alone" — which is not the same as writing `"default"`, because an operator
/// may have chosen `auto` in the CLI itself and a bare apply of the model slots
/// must not silently reset that.
pub fn apply_slots_with_permission(
    updates: &[SlotUpdate],
    base_url: &str,
    permission_mode: Option<&str>,
) -> Result<Value> {
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

    // The permission keys live outside `env`, so they are applied to the whole
    // document; borrowing it once here keeps the two writers from disagreeing
    // about the shape of the file.
    if let Some(mode) = permission_mode {
        apply_permission_update(&mut doc, mode)?;
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

    Ok(json!({
        "ok": true,
        "path": path.display().to_string(),
        "permission_mode": doc
            .pointer("/permissions/defaultMode")
            .cloned()
            .unwrap_or(Value::Null),
        "skip_dangerous_prompt": doc
            .get("skipDangerousModePermissionPrompt")
            .and_then(Value::as_bool)
            .unwrap_or(false),
    }))
}

/// Write `permissions.defaultMode` (and the consent key it depends on).
///
/// Two things here are not obvious and both were verified against Claude Code
/// 2.1.311 rather than inferred from the docs' prose:
///
///   * `bypassPermissions` is gated on a *consent record*, not just on the mode.
///     Without `skipDangerousModePermissionPrompt: true` in user settings the CLI
///     shows a warning dialog before entering the mode, and a background/`--bg`
///     session is silently downgraded to `default` — which is exactly the
///     "nothing happens" failure a one-click write must not produce. Writing the
///     key is what makes the mode survive an unattended start. Accepting the
///     dialog by hand writes the same key, so this is not a bypass of the
///     consent record; it *is* the consent record.
///   * the mode is written to the *user* file, which is the only scope where
///     `auto` and `bypassPermissions` take effect at all (the CLI ignores both
///     from project/local settings since v2.1.257).
///
/// Every other `permissions.*` key the user set — `allow`, `deny`, `ask`,
/// `additionalDirectories`, and any `defaultMode` we are not asked to change —
/// is left exactly as it was.
fn apply_permission_update(doc: &mut Value, mode: &str) -> Result<()> {
    if !is_known_mode(mode) {
        bail!("unknown permission mode: {}", mode);
    }
    if mode.is_empty() {
        // "Follow the CLI's own default" means write nothing: the caller
        // explicitly did not choose a mode.
        return Ok(());
    }

    let root = doc
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("settings.json must be a JSON object"))?;

    // `permissions` is normally absent on a fresh install. If it is present but
    // not an object, refuse rather than replace someone's hand-written value
    // with an object: silently discarding it is not recoverable from the .bak
    // once the user has closed the window.
    let permissions = match root.get_mut("permissions") {
        None => {
            root.insert("permissions".into(), json!({}));
            root.get_mut("permissions").expect("just inserted")
        }
        Some(value) if value.is_object() => value,
        Some(_) => bail!("settings.json 的 `permissions` 不是对象，未做改动"),
    };
    permissions
        .as_object_mut()
        .expect("checked object above")
        .insert("defaultMode".into(), json!(mode));

    if mode == "bypassPermissions" {
        root.insert("skipDangerousModePermissionPrompt".into(), json!(true));
    }

    Ok(())
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
    // A real gateway access key when one is available, so the config works as
    // written. The placeholder is only for the case where no stored account kept
    // its plaintext: the CLI refuses to start without *some* token, and a wrong
    // one at least fails at the gateway with a 401 that names the problem,
    // rather than at the CLI with "no auth configured".
    let token = crate::client_accounts::writable_key()
        .map(|(_, key)| key)
        .unwrap_or_else(|| PLACEHOLDER_AUTH_TOKEN.to_string());
    env.insert("ANTHROPIC_AUTH_TOKEN".into(), json!(token));

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

    // ── permission mode ────────────────────────────────────────────────────

    fn doc_with_permissions(permissions: Value) -> Value {
        json!({
            "env": { "ANTHROPIC_MODEL": "keep-me", "ANTHROPIC_BASE_URL": "http://old:1" },
            "permissions": permissions,
            "model": "sonnet",
        })
    }

    #[test]
    fn bypass_writes_the_mode_and_the_consent_key() {
        // Both keys are required: the mode alone leaves the CLI showing the
        // dialog in a terminal and silently downgrading in a `--bg` session,
        // which reads to the user as "the setting did nothing".
        let mut doc = json!({ "env": {} });
        apply_permission_update(&mut doc, "bypassPermissions").unwrap();

        assert_eq!(doc["permissions"]["defaultMode"], "bypassPermissions");
        assert_eq!(doc["skipDangerousModePermissionPrompt"], true);
    }

    #[test]
    fn every_mode_but_bypass_leaves_the_consent_key_alone() {
        // Writing it for `auto` would record a consent the user never gave, and
        // would then let a later `--dangerously-skip-permissions` start without
        // its dialog.
        for mode in ["default", "acceptEdits", "plan", "auto", "dontAsk"] {
            let mut doc = json!({ "env": {} });
            apply_permission_update(&mut doc, mode).unwrap();
            assert_eq!(doc["permissions"]["defaultMode"], mode);
        }
    }

    #[test]
    fn an_existing_consent_key_is_never_removed() {
        // The user accepted the dialog once; changing the mode away from bypass
        // must not erase that record (the CLI would show the dialog again).
        let mut doc = json!({ "skipDangerousModePermissionPrompt": true, "env": {} });
        apply_permission_update(&mut doc, "acceptEdits").unwrap();
        assert_eq!(doc["skipDangerousModePermissionPrompt"], true);
    }

    #[test]
    fn unrelated_permission_keys_are_preserved() {
        // `allow`/`deny`/`ask` are hand-tuned by the user; a mode change must
        // not reset them.
        let mut doc = doc_with_permissions(json!({
            "allow": ["Bash(git *)"],
            "deny": ["Bash(rm *)"],
            "additionalDirectories": ["/tmp"],
        }));
        apply_permission_update(&mut doc, "auto").unwrap();

        assert_eq!(doc["permissions"]["defaultMode"], "auto");
        assert_eq!(doc["permissions"]["allow"][0], "Bash(git *)");
        assert_eq!(doc["permissions"]["deny"][0], "Bash(rm *)");
        assert_eq!(doc["permissions"]["additionalDirectories"][0], "/tmp");
    }

    #[test]
    fn a_missing_permissions_object_is_created() {
        // A fresh `~/.claude/settings.json` usually has no `permissions` at all.
        let mut doc = json!({ "env": { "ANTHROPIC_MODEL": "m" } });
        apply_permission_update(&mut doc, "plan").unwrap();
        assert_eq!(doc["permissions"]["defaultMode"], "plan");
        assert_eq!(doc["env"]["ANTHROPIC_MODEL"], "m");
    }

    #[test]
    fn a_non_object_permissions_value_is_refused_not_overwritten() {
        // Replacing someone's hand-written value would be unrecoverable once
        // they closed the window, so refuse and let them fix it.
        let mut doc = doc_with_permissions(json!("oops"));
        let err = apply_permission_update(&mut doc, "auto");
        assert!(
            err.is_err(),
            "a non-object `permissions` must not be replaced"
        );
        assert_eq!(
            doc["permissions"], "oops",
            "the original value must survive"
        );
    }

    #[test]
    fn an_unknown_mode_is_rejected() {
        // The CLI silently ignores an unknown mode and falls back to its
        // default, so accepting one here would look like a successful write
        // that changed nothing.
        let mut doc = json!({ "env": {} });
        let err = apply_permission_update(&mut doc, "bypass");
        assert!(err.is_err(), "a misspelled mode must not be written");
    }

    #[test]
    fn an_empty_mode_is_the_no_write_choice() {
        // The form's first option means "follow the CLI's own default", which
        // must leave no `defaultMode` behind — not write `""`.
        let mut doc = json!({ "env": {}, "permissions": { "allow": ["Bash(ls)"] } });
        apply_permission_update(&mut doc, "").unwrap();
        assert!(doc.pointer("/permissions/defaultMode").is_none());
        assert_eq!(doc["permissions"]["allow"][0], "Bash(ls)");
    }

    #[test]
    fn the_offered_modes_are_exactly_the_ones_claude_code_accepts() {
        // Guards the list the UI renders against the writer's validation: a
        // value only one of them knows about is either a dead option or a
        // rejected write.
        let modes: Vec<&str> = PERMISSION_MODES.iter().map(|(m, _)| *m).collect();
        assert_eq!(
            modes,
            vec![
                "",
                "default",
                "acceptEdits",
                "plan",
                "auto",
                "dontAsk",
                "bypassPermissions"
            ],
            "offered modes must stay in sync with Claude Code's accepted values"
        );
        // Every non-empty offered mode must pass validation…
        for mode in modes.iter().filter(|m| !m.is_empty()) {
            let mut doc = json!({ "env": {} });
            apply_permission_update(&mut doc, mode).expect("every offered mode must be writable");
        }
        // …and a plausible-not-real value must not.
        let mut doc = json!({ "env": {} });
        assert!(apply_permission_update(&mut doc, "manual").is_err());
    }
}
