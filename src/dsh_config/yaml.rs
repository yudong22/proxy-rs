//! Locating and reading structure inside the patch document.
//!
//! Pure text, no I/O: indentation arithmetic, child lookup, value-block bounds,
//! scalar/list reading, and the two locators — profile patch list and the
//! retired flow style — behind [`ProviderSpan`].

use super::*;

// ── Shared line helpers ─────────────────────────────────────────────────────

pub(crate) fn indent_of(line: &str) -> usize {
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
pub(crate) fn find_child(
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
pub(crate) fn value_block_end(
    lines: &[String],
    key_at: usize,
    key_indent: usize,
    limit: usize,
) -> usize {
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
pub(crate) fn scalar_at(lines: &[String], key: &str) -> Option<String> {
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
pub(crate) fn model_ids_in(lines: &[String]) -> Vec<String> {
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
pub(crate) struct ProviderSpan {
    /// First line of the provider's body.
    pub(crate) body_start: usize,
    /// Exclusive end of the provider's body.
    pub(crate) body_end: usize,
    /// Indentation of the `proxy-rs:` key itself.
    pub(crate) key_indent: usize,
    /// Line index of the `providers:` key that owns it, when the shape has one.
    pub(crate) providers_at: Option<usize>,
    /// True for the braced (`{ … }`) style, which renders `models` as a flow
    /// list and needs a trailing comma on every field but the last.
    pub(crate) is_flow: bool,
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
pub(crate) fn find_span(lines: &[String]) -> Option<ProviderSpan> {
    find_block_span(lines).or_else(|| find_flow_span(lines))
}
