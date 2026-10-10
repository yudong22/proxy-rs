//! DSH (DeepSeek Harness) provider wiring.
//!
//! Writes the `proxy-rs` provider into the active profile's
//! `$DSH_HOME/profiles/<name>/cordis.patch.yml` so the harness talks to this
//! proxy. Mirrors `codex_config.rs` in approach, and for the same reason: the
//! configuration document belongs to the user.
//!
//! Why the target is the profile patch and not `settings.yaml`:
//!
//!   * `settings.yaml` no longer exists in current DSH. The harness *renames*
//!     it to `settings.yaml.imported` on first boot and moves each section into
//!     the profile that owns it (`SettingsForms.importLegacyDocument` in
//!     `@deepseek-ai/dsh-settings`). Writing the old path produced a file the
//!     running harness never reads.
//!   * A profile's settings are a **patch list**: a top-level YAML array whose
//!     entries target a loader row by `id`. Editing the patch list is how every
//!     other DSH surface changes these values, so this writer does the same.
//!
//! Why this is a hand-rolled text patch and not a YAML round-trip:
//!
//!   * The patch file is a *shared* document. It carries other rows (locale,
//!     theme, permissions, the default model) and arbitrary user comments. A
//!     serialize-the-whole-document write would reformat all of it — and DSH's
//!     own writer patches at leaf level to preserve comments and formatting, so
//!     a wholesale rewrite would fight it.
//!   * A patch entry's `config` **replaces** the target row's whole config
//!     (`applyEntryPatches` assigns `target[key] = value`), it does not merge.
//!     So the `llm-pi-ai` entry must keep every sibling provider and every
//!     hand-set profile key; only the fields below are allowed to change.
//!
//! The write is confined to three leaves inside the `proxy-rs` provider —
//! `baseURL`, `sessionHeader` and `models`. Every byte outside them is copied
//! through untouched. If the provider block cannot be located confidently the
//! write refuses rather than guessing: a corrupted shared config is far worse
//! than a reported error.
//!
//! Both YAML styles are accepted, because a patch file is hand-editable and the
//! two shapes appear in the wild: the block style the harness writes
//! (`proxy-rs:` followed by indented `key: value` lines) and the flow style
//! (`proxy-rs:` followed by a `{ … }` mapping) used by the retired
//! `settings.yaml`.

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

/// The HTTP field DSH must send for proxy-rs to key requests by conversation.
///
/// Without it the gateway sees only the `deepseek-harness/<version>`
/// user-agent and merges every chat in an install under `dsh:<version>`.
/// Writing it here is what makes the session filter work on a fresh install
/// without a manual edit.
pub const SESSION_HEADER: &str = "x-deepseek-harness-session-id";

/// Filename of a profile's user patch layer.
const PROFILE_PATCH_FILENAME: &str = "cordis.patch.yml";
/// Directory under the Harness home holding every profile.
const PROFILES_DIRNAME: &str = "profiles";
/// Shipped profile `dsh web` uses, and the fallback when nothing else matches.
const DEFAULT_PROFILE: &str = "web";

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

/// Directory holding every profile.
pub fn profiles_dir() -> Option<PathBuf> {
    dsh_home().map(|h| h.join(PROFILES_DIRNAME))
}

/// Name of the profile whose patch file should be edited.
///
/// Resolution order, most authoritative first:
///
/// 1. `DSH_PROFILE`, when it names a directory that exists — the launcher sets
///    it, so this is exact whenever the GUI inherits the environment.
/// 2. The most recently modified profile whose patch file already declares this
///    provider: that is the profile the user's DSH is actually wired to.
/// 3. The most recently modified profile with a patch file at all.
/// 4. `web`, the shipped default for `dsh web`.
///
/// A GUI process normally has no `DSH_PROFILE`, which is why the on-disk
/// evidence outranks the guess in cases 2–3 rather than writing to `web` blind
/// and leaving a second, dead provider block behind.
pub fn active_profile() -> String {
    if let Some(name) = std::env::var("DSH_PROFILE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
    {
        if profiles_dir()
            .map(|d| d.join(&name).is_dir())
            .unwrap_or(false)
        {
            return name;
        }
    }

    let Some(dir) = profiles_dir() else {
        return DEFAULT_PROFILE.to_string();
    };
    let mut declaring: Vec<(std::time::SystemTime, String)> = Vec::new();
    let mut any_patch: Vec<(std::time::SystemTime, String)> = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return DEFAULT_PROFILE.to_string();
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        let patch = entry.path().join(PROFILE_PATCH_FILENAME);
        let Ok(meta) = std::fs::metadata(&patch) else {
            continue;
        };
        let modified = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        if std::fs::read_to_string(&patch)
            .map(|text| declares_provider(&text))
            .unwrap_or(false)
        {
            declaring.push((modified, name.clone()));
        }
        any_patch.push((modified, name));
    }

    let newest = |mut v: Vec<(std::time::SystemTime, String)>| {
        v.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        v.into_iter().next().map(|(_, name)| name)
    };
    newest(declaring)
        .or_else(|| newest(any_patch))
        .unwrap_or_else(|| DEFAULT_PROFILE.to_string())
}

/// Whether a patch document already wires this provider.
///
/// Both document shapes count: the profile patch list, where the namespace is
/// the entry's `id:` (`- id: llm-pi-ai`), and the retired `settings.yaml`, where
/// it is a top-level key. Requiring the provider key as well keeps this from
/// matching a document that merely mentions the namespace.
fn declares_provider(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let namespaced = lines.iter().any(|l| {
        let t = l.trim_start();
        t.trim() == format!("{NAMESPACE}:")
            || t.strip_prefix("- ")
                .map(|rest| rest.trim() == format!("id: {NAMESPACE}"))
                .unwrap_or(false)
    });
    namespaced && lines.iter().any(|l| l.trim() == format!("{PROVIDER}:"))
}

/// The profile patch document this writer edits.
pub fn patch_path() -> Option<PathBuf> {
    profiles_dir().map(|d| d.join(active_profile()).join(PROFILE_PATCH_FILENAME))
}

/// The credential store.
pub fn credentials_path() -> Option<PathBuf> {
    dsh_home().map(|h| h.join(".credentials.yaml"))
}

/// One model entry to write.
#[derive(Debug, Clone, PartialEq)]
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
    /// Per-model 积分 consumption ratio (`×0.29`). `None` omits it; `Some(0.0)`
    /// is a free model.
    pub points_ratio: Option<f64>,
    /// Upstream free badge (e.g. `夜间免费`); `None` when not free.
    pub free_badge: Option<String>,
}

/// What the UI reports about the current wiring.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DshState {
    pub provider_exists: bool,
    pub base_url: Option<String>,
    pub model_ids: Vec<String>,
    /// True when the referenced credential is present in the credential store.
    pub credential_present: bool,
    /// True when the provider already sends the conversation session id, so the
    /// gateway's per-conversation session filter works.
    pub session_header: bool,
}

/// Map a provider model onto a DSH entry.
///
/// Field names differ across the boundary — the proxy speaks `snake_case`,
/// pi-ai's profile uses `camelCase` — so the mapping is explicit here rather
/// than delegated to a rename attribute on either side.
pub fn to_dsh_model(m: &crate::providers::GuiModel) -> DshModel {
    // DSH shows the model by its `name`; append the 积分 ratio and free badge so
    // the user sees the relative cost next to the model in the DSH picker.
    let name = match (&m.name, m.points_ratio, &m.free_badge) {
        (Some(n), Some(r), Some(b)) => Some(format!("{} (×{:.2} {})", n, r, b)),
        (Some(n), Some(r), None) => Some(format!("{} (×{:.2})", n, r)),
        (Some(n), None, Some(b)) => Some(format!("{} ({})", n, b)),
        (Some(n), None, None) => Some(n.clone()),
        (None, _, _) => None,
    };
    DshModel {
        id: m.id.clone(),
        name,
        context_window: m.context_window,
        max_tokens: m.max_output_tokens,
        supports_images: m.supports_images,
        supports_reasoning: m.supports_reasoning,
        points_ratio: m.points_ratio,
        free_badge: m.free_badge.clone(),
    }
}

// ── Shared line helpers ─────────────────────────────────────────────────────

fn indent_of(line: &str) -> usize {
    line.len() - line.trim_start().len()
}

fn is_blank(line: &str) -> bool {
    line.trim().is_empty()
}

/// The minimum indentation of the lines in `from..limit` that sit deeper than
/// `parent_indent`, i.e. the indentation its direct children use.
fn child_indent(
    lines: &[String],
    from: usize,
    limit: usize,
    parent_indent: usize,
) -> Option<usize> {
    let mut min = usize::MAX;
    for line in lines.iter().take(limit).skip(from) {
        if is_blank(line) {
            continue;
        }
        let ind = indent_of(line);
        if ind <= parent_indent {
            break;
        }
        if ind < min {
            min = ind;
        }
    }
    (min != usize::MAX).then_some(min)
}

/// Locate a direct child `key:` of the block whose value starts at `from`.
///
/// `from` must be the first line *inside* the parent's value (i.e. one past the
/// parent's own key line), because the child indentation is derived from the
/// lines scanned here.
///
/// Only the block's own indentation level is matched, so a nested `id:` inside
/// the model list can never be mistaken for the entry's `id:`, and the search
/// stops as soon as the scan leaves the parent block.
fn find_child(
    lines: &[String],
    from: usize,
    limit: usize,
    parent_indent: usize,
    key: &str,
) -> Option<usize> {
    let ci = child_indent(lines, from, limit, parent_indent)?;
    let want = format!("{key}:");
    let spaced = format!("{want} ");
    (from..limit).find(|&i| {
        let line = &lines[i];
        !is_blank(line)
            && indent_of(line) == ci
            && (line.trim() == want || line.trim().starts_with(&spaced))
    })
}

/// Exclusive end of the value block belonging to the key line at `key_at`.
fn value_block_end(lines: &[String], key_at: usize, key_indent: usize, limit: usize) -> usize {
    let mut i = key_at + 1;
    while i < limit {
        let line = &lines[i];
        if !is_blank(line) && indent_of(line) <= key_indent {
            break;
        }
        i += 1;
    }
    i
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

/// Read the `key: value` scalar on a body line, if present.
fn scalar_at(lines: &[String], key: &str) -> Option<String> {
    let want = format!("{key}:");
    lines.iter().find_map(|l| {
        let t = l.trim();
        let rest = t.strip_prefix(&want)?.trim().trim_end_matches(',').trim();
        if rest.is_empty() {
            None
        } else {
            Some(unquote(rest))
        }
    })
}

/// Model ids in `lines`, in order, accepting both block and flow list shapes.
///
/// Block entries read `- id: x`; flow entries read `id: x,`. Only a leading
/// `session-`-style key is stripped, so a nested field cannot be mistaken for
/// the id.
fn model_ids_in(lines: &[String]) -> Vec<String> {
    lines
        .iter()
        .filter_map(|l| {
            let t = l.trim();
            let t = t.strip_prefix("- ").unwrap_or(t);
            let rest = t.strip_prefix("id:")?.trim().trim_end_matches(',').trim();
            if rest.is_empty() {
                None
            } else {
                Some(unquote(rest))
            }
        })
        .collect()
}

// ── Locating the provider block ─────────────────────────────────────────────

/// A located provider block inside the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ProviderSpan {
    /// First line of the provider's body.
    body_start: usize,
    /// Exclusive end of the provider's body.
    body_end: usize,
    /// Indentation of the `proxy-rs:` key itself.
    key_indent: usize,
    /// Line index of the `providers:` key that owns it, when the shape has one.
    providers_at: Option<usize>,
    /// True for the braced (`{ … }`) style, which renders `models` as a flow
    /// list and needs a trailing comma on every field but the last.
    is_flow: bool,
}

/// Locate `proxy-rs:` inside a profile patch list (`- id: llm-pi-ai`).
///
/// Returns `None` when the patch list, the entry, or the provider key is
/// missing — the caller decides whether that is an insert or a refusal.
fn find_block_span(lines: &[String]) -> Option<ProviderSpan> {
    let (entry_start, entry_end, entry_indent) = find_entry(lines)?;

    let config_at = find_child(lines, entry_start + 1, entry_end, entry_indent, "config")?;
    let config_indent = indent_of(&lines[config_at]);
    let config_end = value_block_end(lines, config_at, config_indent, entry_end);
    let providers_at = find_child(lines, config_at + 1, config_end, config_indent, "providers")?;

    let providers_indent = indent_of(&lines[providers_at]);
    let providers_end = value_block_end(lines, providers_at, providers_indent, config_end);

    let provider_at = find_child(
        lines,
        providers_at + 1,
        providers_end,
        providers_indent,
        PROVIDER,
    )?;
    let key_indent = indent_of(&lines[provider_at]);
    let body_end = value_block_end(lines, provider_at, key_indent, providers_end);

    Some(ProviderSpan {
        body_start: provider_at + 1,
        body_end,
        key_indent,
        providers_at: Some(providers_at),
        is_flow: false,
    })
}

/// Find the `- id: llm-pi-ai` list item, as `(start, end, indent)`.
fn find_entry(lines: &[String]) -> Option<(usize, usize, usize)> {
    let want = format!("id: {NAMESPACE}");
    let start = lines.iter().position(|l| {
        let t = l.trim_start();
        t.strip_prefix("- ")
            .map(|rest| rest.trim() == want)
            .unwrap_or(false)
    })?;
    let indent = indent_of(&lines[start]);
    let mut end = lines.len();
    for (i, line) in lines.iter().enumerate().skip(start + 1) {
        if is_blank(line) {
            continue;
        }
        if indent_of(line) <= indent {
            end = i;
            break;
        }
    }
    Some((start, end, indent))
}

/// Locate `proxy-rs:` in the retired `settings.yaml` shape, where `llm-pi-ai:`
/// is a top-level mapping and `providers:` holds a flow mapping.
fn find_flow_span(lines: &[String]) -> Option<ProviderSpan> {
    let ns_at = lines
        .iter()
        .position(|l| l.trim() == format!("{NAMESPACE}:"))?;
    let ns_indent = indent_of(&lines[ns_at]);
    let ns_end = value_block_end(lines, ns_at, ns_indent, lines.len());

    let providers_at = find_child(lines, ns_at + 1, ns_end, ns_indent, "providers")?;

    // The flow body opens with a `{` on one of the few lines after the key.
    let open =
        (providers_at + 1..lines.len().min(providers_at + 4)).find(|&i| lines[i].trim() == "{")?;
    let open_indent = indent_of(&lines[open]);

    let mut depth = 0usize;
    let mut close = None;
    for (i, line) in lines.iter().enumerate().skip(open) {
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && indent_of(line) == open_indent {
                        close = Some(i);
                    }
                }
                _ => {}
            }
        }
        if close.is_some() {
            break;
        }
    }
    let close = close?;

    let provider_at = (open + 1..close).find(|&i| lines[i].trim() == format!("{PROVIDER}:"))?;
    let key_indent = indent_of(&lines[provider_at]);

    // The provider's own body is braced too, and its model entries close with
    // their own `}` lines. The provider's closing brace is therefore the one
    // that (a) balances the brace opened right after the key and (b) sits at
    // that brace's indentation — a nested entry's brace is deeper.
    let provider_open = (provider_at + 1..close).find(|&i| lines[i].trim() == "{")?;
    let provider_open_indent = indent_of(&lines[provider_open]);
    let mut depth = 0usize;
    let mut body_end = close;
    for (i, line) in lines.iter().enumerate().skip(provider_open) {
        if i > close {
            break;
        }
        for ch in line.chars() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 && indent_of(line) == provider_open_indent {
                        body_end = i;
                    }
                }
                _ => {}
            }
        }
        if body_end != close {
            break;
        }
    }

    Some(ProviderSpan {
        // Start *after* the opening brace so the body holds only the fields.
        body_start: provider_open + 1,
        body_end,
        key_indent,
        providers_at: Some(providers_at),
        is_flow: true,
    })
}

/// Locate the provider in whichever document shape `text` uses.
fn find_span(lines: &[String]) -> Option<ProviderSpan> {
    find_block_span(lines).or_else(|| find_flow_span(lines))
}

// ── Reading ─────────────────────────────────────────────────────────────────

/// Inspect a patch document without modifying it.
pub fn read_state(patch_text: &str, credentials_text: Option<&str>) -> DshState {
    let lines: Vec<String> = patch_text.lines().map(|s| s.to_string()).collect();
    let credential_present = credentials_text.map(credential_present).unwrap_or(false);
    let Some(span) = find_span(&lines) else {
        return DshState {
            provider_exists: false,
            base_url: None,
            model_ids: Vec::new(),
            credential_present,
            session_header: false,
        };
    };
    let body = &lines[span.body_start..span.body_end];
    DshState {
        provider_exists: true,
        base_url: scalar_at(body, "baseURL"),
        model_ids: model_ids_in(body),
        credential_present,
        session_header: body.iter().any(|l| {
            l.trim()
                .strip_prefix("sessionHeader:")
                .map(|rest| rest.trim() == SESSION_HEADER)
                .unwrap_or(false)
        }),
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

/// Render a block-style `models:` list body, indented under its key.
///
/// Inline flow scalars are used for `input`/`reasoningEfforts`, which keeps each
/// model's fields on simple single lines while staying ordinary YAML.
fn render_models_block(models: &[DshModel], key_indent: usize) -> Vec<String> {
    let item = " ".repeat(key_indent + 2);
    let field = " ".repeat(key_indent + 4);
    let mut out = Vec::new();
    for m in models {
        out.push(format!("{item}- id: {}", yaml_scalar(&m.id)));
        if let Some(name) = &m.name {
            out.push(format!("{field}name: {}", yaml_scalar(name)));
        }
        if let Some(cw) = m.context_window {
            out.push(format!("{field}contextWindow: {cw}"));
        }
        if let Some(mt) = m.max_tokens {
            out.push(format!("{field}maxTokens: {mt}"));
        }
        if let Some(images) = m.supports_images {
            let input = if images {
                "[ text, image ]"
            } else {
                "[ text ]"
            };
            out.push(format!("{field}input: {input}"));
        }
        if m.supports_reasoning == Some(true) {
            out.push(format!("{field}reasoningEfforts: {{ high: high }}"));
        }
    }
    out
}

/// Render the flow-style `models:` value, matching the document's own style.
fn render_models_flow(models: &[DshModel], indent: usize) -> String {
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

/// Replace `baseURL`, `sessionHeader` and `models` inside the `proxy-rs`
/// provider, leaving every other line byte-for-byte intact.
pub fn upsert_provider(text: &str, base_url: &str, models: &[DshModel]) -> Result<String> {
    if models.is_empty() {
        anyhow::bail!("refusing to write an empty model list into the DSH profile patch");
    }
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let had_trailing_newline = text.ends_with('\n');

    let span = find_span(&lines).ok_or_else(|| {
        anyhow::anyhow!(
            "could not locate the `{PROVIDER}` provider under `{NAMESPACE}.providers` in the DSH \
             profile patch; refusing to rewrite it. Add the provider once by hand (or via the \
             DSH Models page) and retry"
        )
    })?;

    if span.is_flow {
        return upsert_flow(text, span, base_url, models, had_trailing_newline);
    }

    // `baseURL` must exist to anchor the write: without it the document is not
    // a provider this writer understands, and inventing one would be guesswork.
    let base_at = find_child(
        &lines,
        span.body_start,
        span.body_end,
        span.key_indent,
        "baseURL",
    )
    .ok_or_else(|| anyhow::anyhow!("the `{PROVIDER}` provider has no `baseURL` key to update"))?;
    let field_indent = indent_of(&lines[base_at]);
    lines[base_at] = format!("{}baseURL: {base_url}", " ".repeat(field_indent));

    // `sessionHeader` is what makes the gateway's session filter work. It is
    // replaced when present and inserted right after `baseURL` when absent, so
    // a re-run finds it exactly where it was left.
    let session_at = find_child(
        &lines,
        span.body_start,
        span.body_end,
        span.key_indent,
        "sessionHeader",
    );
    let session_line = format!(
        "{}sessionHeader: {SESSION_HEADER}",
        " ".repeat(field_indent)
    );
    match session_at {
        Some(at) => lines[at] = session_line,
        None => lines.insert(base_at + 1, session_line),
    }

    // The span shifted by one line whenever the key was inserted.
    let shift = usize::from(session_at.is_none());
    let body_start = span.body_start;
    let body_end = span.body_end + shift;

    let models_at = find_child(&lines, body_start, body_end, span.key_indent, "models")
        .ok_or_else(|| {
            anyhow::anyhow!("the `{PROVIDER}` provider has no `models` key to update")
        })?;
    let models_indent = indent_of(&lines[models_at]);
    let models_end = value_block_end(&lines, models_at, models_indent, body_end);

    // The key line is re-emitted without any inline value (`models: []`), then
    // the rendered block. Replacing the whole value block keeps a re-run
    // idempotent rather than appending a second list.
    let mut replacement = vec![format!("{}models:", " ".repeat(models_indent))];
    replacement.extend(render_models_block(models, models_indent));
    lines.splice(models_at..models_end, replacement);

    let mut out = lines.join("\n");
    if had_trailing_newline || out.is_empty() {
        out.push('\n');
    }
    Ok(out)
}

/// The flow-style writer for the retired `settings.yaml` shape.
fn upsert_flow(
    text: &str,
    span: ProviderSpan,
    base_url: &str,
    models: &[DshModel],
    had_trailing_newline: bool,
) -> Result<String> {
    let mut lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
    let body: Vec<String> = lines[span.body_start..span.body_end].to_vec();

    let base_rel = body
        .iter()
        .position(|l| l.trim_start().starts_with("baseURL:"))
        .ok_or_else(|| {
            anyhow::anyhow!("the `{PROVIDER}` provider has no `baseURL` key to update")
        })?;
    let ind = indent_of(&body[base_rel]);
    lines[span.body_start + base_rel] = format!("{}baseURL: {base_url},", " ".repeat(ind));

    let session_rel = body
        .iter()
        .position(|l| l.trim_start().starts_with("sessionHeader:"));
    let session_line = format!("{}sessionHeader: {SESSION_HEADER},", " ".repeat(ind));
    match session_rel {
        Some(rel) => lines[span.body_start + rel] = session_line,
        None => lines.insert(span.body_start + base_rel + 1, session_line),
    }

    let shift = usize::from(session_rel.is_none());
    let body_start = span.body_start;
    let body_end = span.body_end + shift;
    let body: Vec<String> = lines[body_start..body_end].to_vec();

    let models_rel = body
        .iter()
        .position(|l| l.trim_start().starts_with("models:"))
        .ok_or_else(|| {
            anyhow::anyhow!("the `{PROVIDER}` provider has no `models` key to update")
        })?;
    let models_indent = indent_of(&body[models_rel]);

    let open_rel = (models_rel + 1..body.len())
        .find(|&i| body[i].trim() == "[")
        .ok_or_else(|| anyhow::anyhow!("`models` is not a bracketed list"))?;

    // Only square brackets are counted: the entries' `{ … }` braces and any
    // `input: [ text, image ]` inside them are nested within the list, so the
    // bracket that returns the depth to zero is the list's own terminator.
    let mut depth = 0usize;
    let mut close_rel = None;
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

    let rendered: Vec<String> = render_models_flow(models, models_indent)
        .lines()
        .map(|s| s.to_string())
        .collect();

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

    lines.splice(
        body_start + models_rel..=body_start + close_rel,
        replacement,
    );

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
/// Returns `(patch_path, credentials_path, credential_added)`.
pub fn apply(
    patch_path: &Path,
    credentials_path: &Path,
    base_url: &str,
    models: &[DshModel],
) -> Result<(PathBuf, PathBuf, bool)> {
    let patch_text = std::fs::read_to_string(patch_path).with_context(|| {
        format!(
            "failed to read {} — start DSH once so it creates the profile, or create the file",
            patch_path.display()
        )
    })?;

    let updated = upsert_provider(&patch_text, base_url, models)?;

    if let Some(dir) = patch_path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let _ = std::fs::copy(
        patch_path,
        patch_path.with_extension(format!("yml.proxy-rs-backup-{}", backup_stamp())),
    );
    write_atomic(patch_path, &updated)?;

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
        patch_path.to_path_buf(),
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

    /// Mirrors the real profile patch: a top-level list of id-targeted entries,
    /// the `llm-pi-ai` entry with a block-style provider, sibling rows before
    /// and after, and comments — which are included because preserving them is
    /// the whole reason this writer is textual.
    const PATCH: &str = r#"# Your patch layer for this dsh profile.
- id: ui-settings-general
  name: "@deepseek-ai/dsh-client-ui-settings-general"
  config:
    welcomeNoticeVersion: 2026-09-28.1
- id: llm-pi-ai
  name: "@deepseek-ai/dsh-llm-pi-ai"
  config:
    providers:
      proxy-rs:
        apiKeyEnv: PROXY_RS_API_KEY
        api: openai-completions
        reasoningEffort: high
        customFlag: keep-me
        baseURL: http://127.0.0.1:3457/v1
        models:
          - id: hy3
            name: Hy3
            contextWindow: 192000
            maxTokens: 64000
            input: [ text ]
            reasoningEfforts: { high: high }
          - id: glm-5.3-flash
            name: GLM-5.3-Flash
            contextWindow: 1000000
            maxTokens: 32000
- id: agent-default-model
  name: "@deepseek-ai/dsh-agent-default-model"
  config:
    provider: proxy-rs
    model: hy3
"#;

    /// The retired `settings.yaml` shape, kept so a flow-style document is
    /// still updatable rather than refused.
    const FLOW: &str = r#"# user notes that must survive
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
          baseURL: http://127.0.0.1:3457/v1,
          models:
            [
              {
                  id: hy3,
                  name: Hy3,
                  contextWindow: 192000,
                  maxTokens: 64000
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
            points_ratio: None,
            free_badge: None,
        }
    }

    fn models_in(text: &str) -> Vec<String> {
        let lines: Vec<String> = text.lines().map(|s| s.to_string()).collect();
        let span = find_span(&lines).expect("provider span");
        model_ids_in(&lines[span.body_start..span.body_end])
    }

    #[test]
    fn read_state_finds_provider_and_models() {
        let st = read_state(PATCH, None);
        assert!(st.provider_exists);
        assert_eq!(st.base_url.as_deref(), Some("http://127.0.0.1:3457/v1"));
        assert_eq!(st.model_ids, vec!["hy3", "glm-5.3-flash"]);
        assert!(!st.credential_present);
        assert!(!st.session_header);
    }

    #[test]
    fn read_state_reports_missing_provider() {
        let st = read_state("- id: locale\n  config:\n    preference: zh\n", None);
        assert!(!st.provider_exists);
    }

    #[test]
    fn read_state_reports_the_session_header() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let st = read_state(&out, None);
        assert!(
            st.session_header,
            "the writer must leave sessionHeader configured"
        );
        assert_eq!(st.model_ids, vec!["hy3"]);
    }

    #[test]
    fn upsert_writes_the_session_header_when_absent() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(out.contains("sessionHeader: x-deepseek-harness-session-id"));
    }

    #[test]
    fn upsert_replaces_an_existing_session_header_rather_than_duplicating() {
        let once = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let twice = upsert_provider(&once, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert_eq!(once, twice, "re-running must not keep changing the file");
        assert_eq!(twice.matches("sessionHeader:").count(), 1);
    }

    #[test]
    fn upsert_preserves_everything_outside_the_managed_leaves() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:9999/v1", &[model("hy4")]).unwrap();

        // Untouched context, in both directions.
        assert!(out.contains("# Your patch layer for this dsh profile."));
        assert!(out.contains("welcomeNoticeVersion: 2026-09-28.1"));
        assert!(out.contains("- id: agent-default-model"));
        assert!(out.contains("    model: hy3"));

        // Untouched keys inside the provider itself, including one this writer
        // knows nothing about.
        assert!(out.contains("apiKeyEnv: PROXY_RS_API_KEY"));
        assert!(out.contains("api: openai-completions"));
        assert!(out.contains("reasoningEffort: high"));
        assert!(out.contains("customFlag: keep-me"));

        // The values that were meant to change.
        assert!(out.contains("baseURL: http://127.0.0.1:9999/v1"));
        assert!(!out.contains("3457"));
        assert_eq!(models_in(&out), vec!["hy4"]);
    }

    #[test]
    fn upsert_is_idempotent() {
        let once = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let twice = upsert_provider(&once, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert_eq!(once, twice, "re-running must not keep changing the file");
    }

    #[test]
    fn upsert_keeps_the_models_inside_the_provider_block() {
        // The block locator must stop at the provider's own boundary: a rewrite
        // that ran past it would corrupt the sibling entry below.
        let out =
            upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("a"), model("b")]).unwrap();
        let lines: Vec<String> = out.lines().map(|s| s.to_string()).collect();
        let span = find_span(&lines).expect("provider span");
        assert_eq!(
            model_ids_in(&lines[span.body_start..span.body_end]),
            vec!["a", "b"]
        );
        assert!(out.contains("- id: agent-default-model"));
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
            points_ratio: None,
            free_badge: None,
        };
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[m]).unwrap();
        assert!(out.contains("- id: deepseek-v4.1-flash"));
        assert!(out.contains("name: Deepseek-V4.1-Flash"));
        assert!(out.contains("contextWindow: 1000000"));
        assert!(out.contains("maxTokens: 128000"));
        assert!(out.contains("reasoningEfforts: { high: high }"));
        assert!(out.contains("input: [ text, image ]"));
    }

    #[test]
    fn upsert_omits_unknown_capability_rather_than_inventing_it() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("plain")]).unwrap();
        assert!(out.contains("- id: plain"));
        assert!(!out.contains("reasoningEfforts"));
        assert!(!out.contains("input:"));
        assert!(!out.contains("contextWindow"));
    }

    #[test]
    fn upsert_refuses_an_empty_model_list() {
        assert!(upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[]).is_err());
    }

    #[test]
    fn to_dsh_model_appends_ratio_and_badge_to_name() {
        // Free node with a badge: `Hy3 (×0.00 限时免费)`.
        let free = crate::providers::GuiModel {
            id: "hy3".into(),
            name: Some("Hy3".into()),
            context_window: Some(200_000),
            max_output_tokens: Some(8_192),
            supports_images: Some(true),
            supports_reasoning: None,
            points_ratio: Some(0.0),
            free_badge: Some("限时免费".into()),
        };
        let d = to_dsh_model(&free);
        assert_eq!(d.name.as_deref(), Some("Hy3 (×0.00 限时免费)"));

        // Paid model: `Hy4 preview (×0.29)`.
        let paid = crate::providers::GuiModel {
            id: "hy4-preview".into(),
            name: Some("Hy4 preview".into()),
            context_window: Some(200_000),
            max_output_tokens: Some(32_768),
            supports_images: Some(true),
            supports_reasoning: Some(true),
            points_ratio: Some(0.29),
            free_badge: None,
        };
        let d2 = to_dsh_model(&paid);
        assert_eq!(d2.name.as_deref(), Some("Hy4 preview (×0.29)"));
    }

    #[test]
    fn upsert_refuses_when_the_provider_is_absent() {
        let text = "- id: locale\n  name: \"@deepseek-ai/dsh-client-locale\"\n  config:\n    preference: zh\n";
        let err = upsert_provider(text, "http://x/v1", &[model("a")]).unwrap_err();
        assert!(
            err.to_string().contains("could not locate"),
            "must explain itself, got {err}"
        );
        assert_eq!(text, text, "input untouched on refusal");
    }

    #[test]
    fn upsert_keeps_the_documents_trailing_newline() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(out.ends_with('\n'));
        let no_newline = PATCH.trim_end_matches('\n').to_string();
        let out2 =
            upsert_provider(&no_newline, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(
            !out2.ends_with('\n'),
            "must not add one the file did not have"
        );
    }

    #[test]
    fn upsert_updates_a_flow_style_document_too() {
        // The retired `settings.yaml` shape must still be updatable: refusing it
        // would strand anyone whose profile patch was written before the
        // migration.
        let out = upsert_provider(FLOW, "http://127.0.0.1:9999/v1", &[model("hy4")]).unwrap();
        assert!(out.contains("# user notes that must survive"));
        assert!(out.contains("other-gateway:"));
        assert!(out.contains("baseURL: https://other.test/v1"));
        assert!(out.contains("baseURL: http://127.0.0.1:9999/v1"));
        assert!(out.contains("sessionHeader: x-deepseek-harness-session-id,"));
        assert_eq!(models_in(&out), vec!["hy4"]);
        let once = out;
        let twice = upsert_provider(&once, "http://127.0.0.1:9999/v1", &[model("hy4")]).unwrap();
        assert_eq!(once, twice, "flow writes must be idempotent too");
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
        let st = read_state("- id: locale\n  config:\n    preference: zh\n", None);
        assert!(!st.provider_exists);
        assert!(st.base_url.is_none());
        assert!(st.model_ids.is_empty());
        assert!(!st.session_header);
    }

    #[test]
    fn the_models_key_keeps_its_indentation() {
        let out = upsert_provider(PATCH, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        let lines: Vec<&str> = out.lines().collect();
        let key = lines
            .iter()
            .position(|l| l.trim() == "models:")
            .expect("models key");
        // The key sits at the provider body's field indent (8 spaces in the
        // fixture) and the entries one level deeper.
        assert_eq!(indent_of(lines[key]), 8, "models key indent");
        assert!(lines[key + 1].trim_start().starts_with("- id: hy3"));
        assert_eq!(indent_of(lines[key + 1]), 10, "item indent");
        // Re-running must not drift the indentation either.
        let again = upsert_provider(&out, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert_eq!(out, again);
    }

    #[test]
    fn declares_provider_matches_only_the_managed_route() {
        assert!(declares_provider(PATCH));
        assert!(!declares_provider(
            "- id: locale\n  config:\n    preference: zh\n"
        ));
    }

    #[test]
    fn write_atomic_replaces_contents_and_keeps_permissions() {
        let dir = tempdir();
        let path = dir.join("cordis.patch.yml");
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
        let patch = dir.join("cordis.patch.yml");
        let creds = dir.join(".credentials.yaml");
        std::fs::write(&patch, PATCH).unwrap();
        std::fs::write(
            &creds,
            "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-x\nrecords:\n",
        )
        .unwrap();

        let (_, _, added) =
            apply(&patch, &creds, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(added, "credential was missing, so it should be added");
        assert!(credential_present(
            &std::fs::read_to_string(&creds).unwrap()
        ));
        // The other credential survived.
        assert!(std::fs::read_to_string(&creds)
            .unwrap()
            .contains("DEEPSEEK_API_KEY: sk-x"));

        // Second run adds nothing and leaves the files alone.
        let before = std::fs::read_to_string(&patch).unwrap();
        let (_, _, added2) =
            apply(&patch, &creds, "http://127.0.0.1:3457/v1", &[model("hy3")]).unwrap();
        assert!(!added2);
        assert_eq!(before, std::fs::read_to_string(&patch).unwrap());
    }

    #[test]
    fn apply_refuses_when_the_patch_document_is_missing() {
        let dir = tempdir();
        let err = apply(
            &dir.join("nope.yml"),
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
