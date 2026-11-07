//! Rendering the provider's YAML fragments.
//!
//! The emit side: quoting a scalar only when it would change meaning, and
//! turning a [`DshModel`] list into either block-style lines or a flow-style
//! value. Pure text, no I/O.

use super::*;

// ── Rendering ───────────────────────────────────────────────────────────────

/// Quote a YAML scalar only when it would otherwise change meaning.
///
/// Model ids routinely contain `.` and `-`, which are safe unquoted; a colon, a
/// leading indicator character or surrounding space is not.
pub(crate) fn yaml_scalar(value: &str) -> String {
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

/// Render the `reasoningEfforts` flow mapping for one model, or `None` when the
/// model declares no reasoning at all.
///
/// `{ key: wire }` pairs key = selectable level, value = the `reasoning_effort`
/// string dispatch sends; both are the same spelling here because the upstream
/// gateway takes the level names verbatim. Declaring a level DSH's pi-ai does
/// not offer would fail its config validation, so unknown tiers are dropped and
/// an empty result falls back to the historical single `high` — that is what
/// every previously written profile already says.
pub(crate) fn render_reasoning_efforts(m: &DshModel) -> Option<String> {
    if m.supports_reasoning != Some(true) {
        return None;
    }
    let declared: Vec<&str> = m
        .reasoning_efforts
        .iter()
        .map(String::as_str)
        .filter(|tier| ENUMERATED_EFFORTS.contains(tier))
        .collect();
    if declared.is_empty() {
        return Some("{ high: high }".to_string());
    }
    let pairs: Vec<String> = declared.iter().map(|t| format!("{t}: {t}")).collect();
    Some(format!("{{ {} }}", pairs.join(", ")))
}

/// Tiers DSH's pi-ai can name, mirroring its `THINKING_LEVELS` gate (minus
/// `off`, which means "send nothing" and is not a tier a user picks here).
/// `max` is part of that gate, which is what lets the DeepSeek triplet through.
const ENUMERATED_EFFORTS: [&str; 6] = ["minimal", "low", "medium", "high", "xhigh", "max"];

/// Render a block-style `models:` list body, indented under its key.
///
/// Inline flow scalars are used for `input`/`reasoningEfforts`, which keeps each
/// model's fields on simple single lines while staying ordinary YAML.
pub(crate) fn render_models_block(models: &[DshModel], key_indent: usize) -> Vec<String> {
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
        if let Some(efforts) = render_reasoning_efforts(m) {
            out.push(format!("{field}reasoningEfforts: {efforts}"));
        }
    }
    out
}

/// Render the flow-style `models:` value, matching the document's own style.
pub(crate) fn render_models_flow(models: &[DshModel], indent: usize) -> String {
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
        if let Some(efforts) = render_reasoning_efforts(m) {
            out.push_str(&format!(
                "{}reasoningEfforts: {},\n",
                sp(indent + 8),
                efforts
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
