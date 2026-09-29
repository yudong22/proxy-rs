//! DSH (DeepSeek Harness) provider wiring.
//!
//! Writes the `proxy-rs` provider into `~/.dsh/settings.yaml` so the harness
//! talks to this proxy. Mirrors `codex_config.rs` in approach, and for the same
//! reason: the settings document belongs to the user.
//!
//! Why this is a hand-rolled text patch and not a YAML round-trip:
//!
//!   * `settings.yaml` is a *shared* document. It carries other providers, the
//!     agent's default model, UI/locale/permission preferences and whatever
//!     namespaces plugins added. A serialize-the-whole-document write would
//!     reformat all of it.
//!   * Measured on a real file, a `serde_yaml` parse→serialize pass took it from
//!     2831 to 2110 bytes: flow-style mappings were expanded to block style and
//!     the layout was rearranged wholesale. That is a violation, not a diff.
//!   * DSH's own writer patches at leaf level to preserve comments and
//!     formatting (see `settings-file`'s `patchNode`). Writing this file in any
//!     other way would fight it.
//!
//! So the write is confined to two leaf values inside the `proxy-rs` provider
//! (`baseURL` and `models`); every byte outside them is copied through
//! untouched. If the provider block cannot be located confidently the write
//! refuses rather than guessing — a corrupted shared config is far worse than a
//! reported error.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

/// The credential name DSH resolves for this provider.
///
/// proxy-rs does not authenticate its own clients (it forwards with the
/// credential it holds), so any non-empty value works. The value only has to be
/// something an HTTP header can carry, because DSH asserts that before use.
pub const CREDENTIAL_REF: &str = "PROXY_RS_API_KEY";

/// Placeholder credential value, matching the convention already present in
/// the user's credentials file.
const CREDENTIAL_PLACEHOLDER: &str = "nouse";

/// The namespace DSH's pi-ai adapter reads providers from.
const NAMESPACE: &str = "llm-pi-ai";
/// The provider id this proxy registers itself as.
const PROVIDER: &str = "proxy-rs";

/// Harness home, honoring `DSH_HOME` and falling back to `~/.dsh`.
pub fn dsh_home() -> Option<PathBuf> {
    std::env::var("DSH_HOME")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var("HOME")
                .ok()
                .filter(|h| !h.trim().is_empty())
                .map(|h| PathBuf::from(h).join(".dsh"))
        })
}

/// The settings document.
pub fn settings_path() -> Option<PathBuf> {
    dsh_home().map(|h| h.join("settings.yaml"))
}

/// The credential store.
pub fn credentials_path() -> Option<PathBuf> {
    dsh_home().map(|h| h.join(".credentials.yaml"))
}

/// One model entry to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DshModel {
    pub id: String,
    pub name: Option<String>,
    pub context_window: Option<u64>,
    pub max_tokens: Option<u64>,
    /// `true` writes `[ text, image ]`; `false` writes `[ text ]`; `None` omits
    /// the key so DSH falls back to the route's `defaultInput`.
    pub supports_images: Option<bool>,
    /// `true` declares reasoning levels; `None` omits the key, which leaves the
    /// model without an Effort menu rather than inventing capability.
    pub supports_reasoning: Option<bool>,
}

/// What the UI reports about the current wiring.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DshState {
    pub provider_exists: bool,
    pub base_url: Option<String>,
    pub model_ids: Vec<String>,
    /// True when the referenced credential is present in the credential store.
    pub credential_present: bool,
}

/// Map a provider model onto a DSH entry.
///
/// Field names differ across the boundary — the proxy speaks `snake_case`,
/// pi-ai's profile uses `camelCase` — so the mapping is explicit here rather
/// than delegated to a rename attribute on either side.
pub fn to_dsh_model(m: &crate::providers::GuiModel) -> DshModel {
    DshModel {
        id: m.id.clone(),
        name: m.name.clone(),
        context_window: m.context_window,
        max_tokens: m.max_output_tokens,
        supports_images: m.supports_images,
        supports_reasoning: m.supports_reasoning,
    }
}

// ── Reading ─────────────────────────────────────────────────────────────────

/// Locate the `proxy-rs` provider block inside `text`.
///
/// Returns `(start, end)` line indices of the block body, where `start` is the
/// line after the opening `{` and `end` is the index of the matching `}`.
/// `None` when the provider (or its opening brace) is not there.
fn find_provider_body(lines: &[String]) -> Option<(usize, usize)> {
    let provider_at = lines
        .iter()
        .position(|l| l.trim() == format!("{PROVIDER}:"))?;

    // The body opens with a `{` (the file uses a flow mapping) somewhere in the
    // few lines that follow the key.
    let open =
        (provider_at + 1..lines.len().min(provider_at + 3)).find(|&i| lines[i].trim() == "{")?;

    let open_indent = indent_of(&lines[open]);
    let mut depth = 0usize;
    for (i, line) in lines.iter().enumerate().skip(open) {
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        // The provider block closes at the brace that opens at
                        // `open_indent`; anything else means the shape is not
                        // what this writer understands.
                        if indent_of(line) == open_indent {
                            return Some((open + 1, i));
                        }
                    }
                }
                _ => {}
            }
        }
    }
    None
}

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

/// Read the `key: value` scalar on a body line, if present.
fn scalar_at(lines: &[String], key: &str) -> Option<String> {
    lines.iter().find_map(|l| {
        let t = l.trim();
        let rest = t
            .strip_prefix(key)?
            .strip_prefix(':')?
            .trim()
            .trim_end_matches(',')
            .trim();
        if rest.is_empty() {
            None
        } else {
            Some(rest.to_string())
        }
    })
}

/// Model ids inside the provider body, in order.
fn model_ids_at(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|l| {
            let t = l.trim();
            let rest = t.strip_prefix("id:")?.trim().trim_end_matches(',').trim();
            if rest.is_empty() {
                None
            } else {
                Some(unquote(rest))
            }
        })
        .collect()
}

fn unquote(v: &str) -> String {
    let v = v.trim();
    if v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'') {
        v[1..v.len() - 1].replace("''", "'")
    } else if v.len() >= 2 && v.starts_with('"') && v.ends_with('"') {
        v[1..v.len() - 1].to_string()
    } else {
        v.to_string()
    }
}

/// Inspect a settings document without modifying it.
pub fn read_state(settings_text: &str, credentials_text: Option<&str>) -> DshState {
    let lines: Vec<String> = settings_text.lines().map(|s| s.to_string()).collect();
    let Some((start, end)) = find_provider_body(&lines) else {
        return DshState {
            provider_exists: false,
            base_url: None,
            model_ids: Vec::new(),
            credential_present: credentials_text.map(credential_present).unwrap_or(false),
        };
    };
    let body = &lines[start..end];
    DshState {
        provider_exists: true,
        base_url: scalar_at(body, "baseURL"),
        model_ids: model_ids_at(body),
        credential_present: credentials_text.map(credential_present).unwrap_or(false),
    }
}

/// Whether the credential store already defines the reference.
pub fn credential_present(credentials_text: &str) -> bool {
    credentials_text.lines().any(|l| {
        let t = l.trim();
        t.strip_prefix(CREDENTIAL_REF)
            .map(|rest| rest.trim_start().starts_with(':'))
            .unwrap_or(false)
    })
}

// ── Rendering ───────────────────────────────────────────────────────────────

/// Quote a YAML scalar only when it would otherwise change meaning.
///
/// Model ids routinely contain `.` and `-`, which are safe unquoted; a colon, a
/// leading indicator character or surrounding space is not.
fn yaml_scalar(value: &str) -> String {
    let needs_quotes = value.is_empty()
        || value.starts_with([
            ':', '-', '?', ',', '[', ']', '{', '}', '#', '&', '*', '!', '|', '>', '@', '`', '"',
            '\'', '%',
        ])
        || value.contains(": ")
        || value.ends_with(':')
        || value.contains(" #")
        || value.trim() != value;
    if needs_quotes {
        format!("'{}'", value.replace('\'', "''"))
    } else {
        value.to_string()
    }
}

/// Render the `models:` value in the same flow style the document already uses.
///
/// `indent` is the indentation of the `models:` key itself; the bracket and
/// entry indents are derived from it so a rewritten block keeps the file's
/// existing shape instead of a serializer's idea of it.
fn render_models_value(models: &[DshModel], indent: usize) -> String {
    let sp = |n: usize| " ".repeat(n);
    let mut out = format!("{}[\n", sp(indent + 2));
    for (i, m) in models.iter().enumerate() {
        out.push_str(&format!("{}{{\n", sp(indent + 4)));
        out.push_str(&format!("{}id: {},\n", sp(indent + 8), yaml_scalar(&m.id)));
        if let Some(name) = &m.name {
            out.push_str(&format!("{}name: {},\n", sp(indent + 8), yaml_scalar(name)));
        }
        if let Some(cw) = m.context_window {
            out.push_str(&format!("{}contextWindow: {},\n", sp(indent + 8), cw));
        }
        if m.supports_reasoning == Some(true) {
            out.push_str(&format!(
                "{}reasoningEfforts: {{ high: high }},\n",
                sp(indent + 8)
            ));
        }
        if let Some(mt) = m.max_tokens {
            out.push_str(&format!("{}maxTokens: {},\n", sp(indent + 8), mt));
        }
        if let Some(images) = m.supports_images {
            let input = if images {
                "[ text, image ]"
            } else {
                "[ text ]"
            };
            out.push_str(&format!("{}input: {}\n", sp(indent + 8), input));
        }
        // Drop a trailing comma on the final field so the entry stays valid.
        while out.ends_with(",\n") {
            out.truncate(out.len() - 2);
            out.push('\n');
        }
        let closing = if i + 1 == models.len() { "\n" } else { ",\n" };
        out.push_str(&format!("{}}}{}", sp(indent + 6), closing));
    }
    out.push_str(&format!("{}]", sp(indent + 2)));
    out
}

// ── Writing ─────────────────────────────────────────────────────────────────

/// Replace `baseURL` and `models` inside the `proxy-rs` provider.
///
/// Every other line — including `apiKeyEnv`, `api`, `reasoningEffort`, sibling
/// providers and every other namespace — is copied through byte-for-byte.
pub fn upsert_provider(text: &str, base_url: &str, models: &[DshModel]) -> Result<String> {
    if models.is_empty() {
        anyhow::bail!("refusing to write an empty model list into DSH settings");
    }
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let had_trailing_newline = text.ends_with('\n');

    let (start, end) = find_provider_body(&lines).ok_or_else(|| {
        anyhow::anyhow!(
            "could not locate the `{PROVIDER}` provider under `{NAMESPACE}.providers` in the DSH \
             settings document; refusing to rewrite it. Add the provider once by hand (or via the \
             DSH Models page) and retry"
        )
    })?;

    // Replace the two leaf values inside the body, walking backwards so earlier
    // indices stay valid.
    let body: Vec<String> = lines[start..end].to_vec();

    if let Some(rel) = body
        .iter()
        .position(|l| l.trim_start().starts_with("baseURL:"))
    {
        let ind = indent_of(&body[rel]);
        lines[start + rel] = format!("{}baseURL: {},", " ".repeat(ind), base_url);
    } else {
        return Err(anyhow::anyhow!(
            "the `{PROVIDER}` provider has no `baseURL` key to update"
        ));
    }

    // `models:` opens a bracketed list; find its extent by bracket balance.
    let Some(models_rel) = body
        .iter()
        .position(|l| l.trim_start().starts_with("models:"))
    else {
        return Err(anyhow::anyhow!(
            "the `{PROVIDER}` provider has no `models` key to update"
        ));
    };
    let models_indent = indent_of(&body[models_rel]);

    let open_rel = (models_rel + 1..body.len())
        .find(|&i| body[i].trim() == "[")
        .ok_or_else(|| anyhow::anyhow!("`models` is not a bracketed list"))?;

    let mut depth = 0usize;
    let mut close_rel = None;
    // Only square brackets are counted: the entries' `{ … }` braces and any
    // `input: [ text, image ]` inside them are nested within the list, so the
    // bracket that returns the depth to zero is the list's own terminator.
    for (i, line) in body.iter().enumerate().skip(open_rel) {
        for ch in line.chars() {
            match ch {
                '[' => depth += 1,
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        close_rel = Some(i);
                    }
                }
                _ => {}
            }
        }
        if close_rel.is_some() {
            break;
        }
    }
    let close_rel = close_rel.ok_or_else(|| {
        anyhow::anyhow!("the `models` list is not terminated with `]` inside the provider block")
    })?;

    let rendered: Vec<String> = render_models_value(models, models_indent)
        .lines()
        .map(|s| s.to_string())
        .collect();

    // Rebuild the key line plus the list. The list keeps its own opening `[`
    // (it is the first rendered line); only the `models:` key is re-emitted
    // here, so a re-run finds the same shape it just wrote.
    let key_line = if body[models_rel].trim_end().ends_with(',') {
        format!("{}models:,", " ".repeat(models_indent))
    } else {
        format!("{}models:", " ".repeat(models_indent))
    };
    let mut replacement: Vec<String> = vec![key_line];
    replacement.extend(rendered);

    // A sibling key following the list means the list itself carried a comma.
    if close_rel + 1 < body.len() && body[close_rel].trim_end().ends_with(',') {
        if let Some(last) = replacement.last_mut() {
            if !last.ends_with(',') {
                last.push(',');
            }
        }
    }

    lines.splice(start + models_rel..=start + close_rel, replacement);

    let mut out = lines.join("\n");
    if had_trailing_newline || out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// Add the credential reference when it is missing, preserving everything else.
///
/// Inserted at the end of the `refs` mapping, which is where the other
/// references live; the `records` section and every existing entry are left
/// alone. Already-present is a no-op, so re-running is idempotent.
pub fn ensure_credential(text: &str) -> String {
    if credential_present(text) {
        return text.to_string();
    }
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let had_trailing_newline = text.ends_with('\n');

    // Prefer appending after the last entry of `refs`, before the next
    // top-level key.
    let refs_at = lines.iter().position(|l| l.trim_end() == "refs:");
    let insert_at = match refs_at {
        Some(r) => {
            let mut i = r + 1;
            let mut last_entry = None;
            while i < lines.len() {
                if !lines[i].trim().is_empty() && indent_of(&lines[i]) == 0 {
                    break; // next top-level key
                }
                if lines[i]
                    .trim_start()
                    .starts_with(|c: char| c.is_ascii_alphabetic())
                    && indent_of(&lines[i]) > 0
                {
                    last_entry = Some(i);
                }
                i += 1;
            }
            last_entry.map(|e| e + 1).unwrap_or(r + 1)
        }
        // No `refs` mapping at all: create one.
        None => {
            lines.push("refs:".to_string());
            lines.len()
        }
    };

    lines.insert(
        insert_at,
        format!("  {CREDENTIAL_REF}: {CREDENTIAL_PLACEHOLDER}"),
    );

    let mut out = lines.join("\n");
    if had_trailing_newline || out.is_empty() {
        out.push('\n');
    }
    out
}

/// Write the provider and credential, backing up what is replaced.
///
/// Returns `(settings_path, credentials_path, credential_added)`.
pub fn apply(
    settings_path: &Path,
    credentials_path: &Path,
    base_url: &str,
    models: &[DshModel],
) -> Result<(PathBuf, PathBuf, bool)> {
    let settings_text = std::fs::read_to_string(settings_path).with_context(|| {
        format!(
            "failed to read {} — create it once by launching DSH",
            settings_path.display()
        )
    })?;

    let updated = upsert_provider(&settings_text, base_url, models)?;

    if let Some(dir) = settings_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::copy(
        settings_path,
        settings_path.with_extension(format!("yaml.proxy-rs-backup-{}", backup_stamp())),
    );
    write_atomic(settings_path, &updated)?;

    // The credential is written only when missing, so an existing (possibly
    // meaningful) value is never overwritten.
    let mut credential_added = false;
    let credentials_text = std::fs::read_to_string(credentials_path).unwrap_or_default();
    if !credential_present(&credentials_text) {
        let patched = ensure_credential(&credentials_text);
        if let Some(dir) = credentials_path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        if credentials_path.exists() {
            let _ = std::fs::copy(
                credentials_path,
                credentials_path.with_extension(format!("yaml.proxy-rs-backup-{}", backup_stamp())),
            );
        }
        write_atomic(credentials_path, &patched)?;
        credential_added = true;
    }

    Ok((
        settings_path.to_path_buf(),
        credentials_path.to_path_buf(),
        credential_added,
    ))
}

/// Replace a file's contents without ever exposing a half-written document.
///
/// A plain `fs::write` truncates in place, so a reader — or DSH's own settings
/// watcher, which re-reads this document whenever it changes — can observe an
/// empty or partial file. Writing to a sibling then renaming makes the swap
/// atomic on the same filesystem.
///
/// The replaced file's permission bits are copied onto the replacement: the
/// credential store is `0600` because it holds secrets, and a fresh temp file
/// would otherwise be created world-readable under the usual umask.
fn write_atomic(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write as _;

    let dir = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    let tmp = dir.join(format!(
        ".{}.proxy-rs-tmp-{}",
        path.file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "dsh".to_string()),
        std::process::id()
    ));

    {
        let mut file = std::fs::File::create(&tmp)
            .with_context(|| format!("failed to create {}", tmp.display()))?;
        file.write_all(contents.as_bytes())
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        file.sync_all()
            .with_context(|| format!("failed to flush {}", tmp.display()))?;
    }

    if let Ok(meta) = std::fs::metadata(path) {
        let _ = std::fs::set_permissions(&tmp, meta.permissions());
    }

    std::fs::rename(&tmp, path).with_context(|| {
        let _ = std::fs::remove_file(&tmp);
        format!("failed to replace {}", path.display())
    })?;
    Ok(())
}

/// Filesystem-safe UTC-ish stamp for backup names.
fn backup_stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Mirrors the real document's shape: a flow mapping for `providers`, an
    /// unrelated namespace before it, a sibling provider, and other top-level
    /// keys after it. Comments are included because preserving them is the
    /// whole reason this writer is textual.
    const FIXTURE: &str = r#"# user notes that must survive
ui-onboarding:
  welcomeNoticeVersion: 2026-08-13.1
llm-pi-ai:
  providers:
    {
      other-gateway:
        { apiKeyEnv: OTHER_KEY, api: openai-completions, baseURL: https://other.test/v1 },
      proxy-rs:
        {
          apiKeyEnv: PROXY_RS_API_KEY,
          api: openai-completions,
          reasoningEffort: high,
          baseURL: http://127.0.0.1:3457/v1,
          models:
            [
              {
                  id: hy3,
                  name: Hy3,
                  contextWindow: 192000,
                  reasoningEfforts: { high: high },
                  maxTokens: 64000
                },
              {
                  id: glm-5.3-flash,
                  name: GLM-5.3-Flash,
                  contextWindow: 1000000,
                  maxTokens: 32000
                }
            ]
        }
    }
agent-default-model:
  provider: proxy-rs
  model: hy3
"#;

    fn model(id: &str) -> DshModel {
        DshModel {
            id: id.to_string(),
            name: None,
            context_window: None,
            max_tokens: None,
            supports_images: None,
            supports_reasoning: None,
        }
    }

    fn ids_in(text: &str) -> Vec<String> {
        let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
        let (s, e) = find_provider_body(&lines).expect("provider body");
        model_ids_at(&lines[s..e])
    }

    #[test]
    fn read_state_finds_provider_and_models() {
        let st = read_state(FIXTURE, None);
        assert!(st.provider_exists);
        assert_eq!(st.base_url.as_deref(), Some("http://127.0.0.1:3457/v1"));
        assert_eq!(st.model_ids, vec!["hy3", "glm-5.3-flash"]);
        assert!(!st.credential_present);
    }

    #[test]
    fn read_state_reports_missing_provider() {
        let st = read_state("llm-pi-ai:\n  providers:\n    {}\n", None);
        assert!(!st.provider_exists);
    }

    #[test]
    fn upsert_preserves_everything_outside_the_two_leaf_values() {
        let out = upsert_provider(FIXTURE, "http://127.0.0.1:9999/v1", &[model("hy4")]).unwrap();

        // Untouched context, in both directions.
        assert!(out.contains("# user notes that must survive"));
        assert!(out.contains("welcomeNoticeVersion: 2026-08-13.1"));
        assert!(out.contains("other-gateway:"));
        assert!(out.contains("apiKeyEnv: OTHER_KEY"));
        assert!(out.contains("baseURL: https://other.test/v1"));
        assert!(out.contains("agent-default-model:"));
        assert!(out.contains("  provider: proxy-rs"));

        // Untouched keys inside the provider itself.
        assert!(out.contains("apiKeyEnv: PROXY_RS_API_KEY"));
        assert!(out.contains("api: openai-completions"));
        assert!(out.contains("reasoningEffort: high"));

        // The two values that were meant to change.
        assert!(out.contains("baseURL: http://127.0.0.1:9999/v1"));
        assert!(!out.contains("3457"));
        assert_eq!(ids_in(&out), vec!["hy4"]);
    }

    #[test]
    fn upsert_is_idempotent() {
        let once = upsert_provider(FIXTURE, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let twice = upsert_provider(&once, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert_eq!(once, twice, "re-running must not keep changing the file");
    }

    #[test]
    fn upsert_renders_every_field_it_is_given() {
        let m = DshModel {
            id: "deepseek-v4.1-flash".to_string(),
            name: Some("Deepseek-V4.1-Flash".to_string()),
            context_window: Some(1_000_000),
            max_tokens: Some(128_000),
            supports_images: Some(true),
            supports_reasoning: Some(true),
        };
        let out = upsert_provider(FIXTURE, "http://127.0.0.1:3457/v1", &[m]).unwrap();
        assert!(out.contains("id: deepseek-v4.1-flash"));
        assert!(out.contains("name: Deepseek-V4.1-Flash"));
        assert!(out.contains("contextWindow: 1000000"));
        assert!(out.contains("maxTokens: 128000"));
        assert!(out.contains("reasoningEfforts: { high: high }"));
        assert!(out.contains("input: [ text, image ]"));
    }

    #[test]
    fn upsert_omits_unknown_capability_rather_than_inventing_it() {
        let out = upsert_provider(FIXTURE, "http://127.0.0.1:3457/v1", &[model("plain")]).unwrap();
        assert!(out.contains("id: plain"));
        assert!(!out.contains("reasoningEfforts"));
        assert!(!out.contains("input:"));
        assert!(!out.contains("contextWindow"));
    }

    #[test]
    fn rendered_list_has_no_trailing_comma_before_the_bracket() {
        let out = upsert_provider(
            FIXTURE,
            "http://127.0.0.1:3457/v1",
            &[model("a"), model("b")],
        )
        .unwrap();
        // The last field of the last entry must not carry a comma.
        let line = out
            .lines()
            .rev()
            .find(|l| l.trim().starts_with('}'))
            .expect("closing brace");
        assert!(line.trim().ends_with('}'), "got {line:?}");
    }

    #[test]
    fn upsert_keeps_the_documents_trailing_newline() {
        let out = upsert_provider(FIXTURE, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(out.ends_with('\n'));
        let no_newline = FIXTURE.trim_end_matches('\n').to_string();
        let out2 =
            upsert_provider(&no_newline, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(
            !out2.ends_with('\n'),
            "must not add one the file did not have"
        );
    }

    #[test]
    fn upsert_refuses_an_empty_model_list() {
        assert!(upsert_provider(FIXTURE, "http://127.0.0.1:3457/v1", &[]).is_err());
    }

    #[test]
    fn upsert_refuses_when_the_provider_is_absent() {
        let text = "llm-pi-ai:\n  providers:\n    {\n    }\n";
        let err = upsert_provider(text, "http://x/v1", &[model("a")]).unwrap_err();
        assert!(
            err.to_string().contains("could not locate"),
            "must explain itself, got {err}"
        );
        assert_eq!(
            text, "llm-pi-ai:\n  providers:\n    {\n    }\n",
            "input untouched on refusal"
        );
    }

    #[test]
    fn upsert_rejects_a_quoted_scalar_that_needs_quoting() {
        assert_eq!(yaml_scalar("plain-id.1"), "plain-id.1");
        assert_eq!(yaml_scalar("has: space"), "'has: space'");
        assert_eq!(yaml_scalar("trailing "), "'trailing '");
        assert_eq!(yaml_scalar("it's"), "it's");
        assert_eq!(yaml_scalar("a'b: c"), "'a''b: c'");
    }

    #[test]
    fn credential_is_added_once_and_is_idempotent() {
        let store =
            "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-x\nrecords:\n  a:\n    kind: grant\n";
        assert!(!credential_present(store));
        let once = ensure_credential(store);
        assert!(credential_present(&once));
        assert!(once.contains("PROXY_RS_API_KEY: nouse"));
        // Existing entries and the records section survive.
        assert!(once.contains("DEEPSEEK_API_KEY: sk-x"));
        assert!(once.contains("records:"));
        assert!(once.contains("    kind: grant"));
        // Inserted inside refs, not after records.
        let refs = once.find("refs:").unwrap();
        let records = once.find("records:").unwrap();
        let added = once.find("PROXY_RS_API_KEY").unwrap();
        assert!(
            refs < added && added < records,
            "credential belongs in refs"
        );

        let twice = ensure_credential(&once);
        assert_eq!(once, twice);
        assert_eq!(twice.matches("PROXY_RS_API_KEY").count(), 1);
    }

    #[test]
    fn credential_is_left_alone_when_already_present() {
        let store = "version: 1\nrefs:\n  PROXY_RS_API_KEY: nouse\nrecords:\n";
        assert_eq!(ensure_credential(store), store);
    }

    #[test]
    fn credential_appends_when_there_are_no_refs_yet() {
        let out = ensure_credential("version: 1\n");
        assert!(out.contains("refs:"));
        assert!(out.contains("PROXY_RS_API_KEY: nouse"));
    }

    #[test]
    fn probe_refuses_to_write_through_a_document_that_has_none() {
        // read_state must not invent a provider for a document without one.
        let st = read_state("locale:\n  preference: zh\n", None);
        assert!(!st.provider_exists);
        assert!(st.base_url.is_none());
        assert!(st.model_ids.is_empty());
    }

    #[test]
    fn document_without_notes_still_produces_valid_structure() {
        // No comment in the input: the output must not fabricate one.
        let plain = "llm-pi-ai:\n  providers:\n    {\n      proxy-rs:\n        {\n          baseURL: http://127.0.0.1:3457/v1,\n          models:\n            [\n              { id: old }\n            ]\n        }\n    }\n";
        let out = upsert_provider(plain, "http://127.0.0.1:3457/v1", &[model("new")]).unwrap();
        assert_eq!(ids_in(&out), vec!["new"]);
        assert!(!out.contains("old"));
        assert!(!out.contains('#'));
    }

    #[test]
    fn the_models_key_and_its_brackets_keep_their_indentation() {
        let out = upsert_provider(FIXTURE, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        let key = lines
            .iter()
            .position(|l| l.trim() == "models:")
            .expect("models key");
        // The key is at the provider body's field indent, and the bracket that
        // opens the list sits one level deeper and is still a bracket.
        assert_eq!(indent_of(lines[key]), 10, "models key indent");
        assert_eq!(lines[key + 1].trim(), "[");
        assert_eq!(indent_of(lines[key + 1]), 12, "opening bracket indent");
        assert!(lines.iter().any(|l| l.trim() == "]"));
        // Re-running must not drift the indentation either.
        let again = upsert_provider(&out, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert_eq!(out, again);
    }

    #[test]
    fn write_atomic_replaces_contents_and_keeps_permissions() {
        let dir = tempdir();
        let path = dir.join("settings.yaml");
        std::fs::write(&path, "old\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }

        write_atomic(&path, "new contents\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new contents\n");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "a secret-bearing file must not widen its mode");
        }

        // No temp file is left behind.
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.contains("proxy-rs-tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left {leftovers:?}");
    }

    #[test]
    fn apply_writes_both_files_and_is_idempotent() {
        let dir = tempdir();
        let settings = dir.join("settings.yaml");
        let creds = dir.join(".credentials.yaml");
        std::fs::write(&settings, FIXTURE).unwrap();
        std::fs::write(
            &creds,
            "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-x\nrecords:\n",
        )
        .unwrap();

        let (_, _, added) = apply(
            &settings,
            &creds,
            "http://127.0.0.1:3457/v1",
            &[model("hy3")],
        )
        .unwrap();
        assert!(added, "credential was missing, so it should be added");
        assert!(credential_present(
            &std::fs::read_to_string(&creds).unwrap()
        ));
        // The other credential survived.
        assert!(std::fs::read_to_string(&creds)
            .unwrap()
            .contains("DEEPSEEK_API_KEY: sk-x"));

        // Second run adds nothing and leaves the files alone.
        let before = std::fs::read_to_string(&settings).unwrap();
        let (_, _, added2) = apply(
            &settings,
            &creds,
            "http://127.0.0.1:3457/v1",
            &[model("hy3")],
        )
        .unwrap();
        assert!(!added2);
        assert_eq!(before, std::fs::read_to_string(&settings).unwrap());
    }

    #[test]
    fn apply_refuses_when_the_settings_document_is_missing() {
        let dir = tempdir();
        let err = apply(
            &dir.join("nope.yaml"),
            &dir.join(".credentials.yaml"),
            "http://127.0.0.1:3457/v1",
            &[model("hy3")],
        )
        .unwrap_err();
        assert!(err.to_string().contains("failed to read"), "got {err}");
    }

    /// Minimal temp-dir helper so the module stays free of extra dev-deps.
    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "proxy-rs-dsh-config-{}-{:?}",
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
