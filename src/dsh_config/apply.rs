//! Writing the provider and its credential.
//!
//! The I/O half: splicing the provider block in place, adding the credential
//! reference, and the atomic replacement that keeps a half-written document
//! from ever being observable.

use super::*;

/// Placeholder credential value, matching the convention already present in
/// the user's credentials file.
pub(crate) const CREDENTIAL_PLACEHOLDER: &str = "nouse";

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

    // A real gateway access key when one can be recovered, so the written config
    // works as-is. The placeholder remains for the case where no stored account
    // kept its plaintext: DSH would otherwise send no credential at all, and a
    // wrong one at least yields a 401 from the gateway instead of a mystery.
    let value = crate::client_accounts::writable_key()
        .map(|(_, key)| key)
        .unwrap_or_else(|| CREDENTIAL_PLACEHOLDER.to_string());
    lines.insert(insert_at, format!("  {CREDENTIAL_REF}: {value}"));

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
pub(crate) fn write_atomic(path: &Path, contents: &str) -> Result<()> {
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
