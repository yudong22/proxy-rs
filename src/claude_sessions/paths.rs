//! Where transcripts live, and which ids/labels are allowed to name one.

use super::*;

/// Where transcripts are read from.
///
/// Built from a path so tests never touch the real home; production builds one
/// with [`Self::from_env`].
#[derive(Debug, Clone)]
pub struct ClaudeSessionsSource {
    pub projects_root: PathBuf,
}

impl ClaudeSessionsSource {
    pub fn new(projects_root: impl Into<PathBuf>) -> Self {
        Self {
            projects_root: projects_root.into(),
        }
    }

    /// `$CLAUDE_PROJECTS_DIR` (test/override hook) → `~/.claude/projects`.
    ///
    /// `None` when the directory does not exist: a machine that never ran
    /// Claude Code has no sessions, and the caller reports that as an
    /// explanatory payload rather than an error.
    pub fn from_env() -> Option<Self> {
        if let Some(dir) = std::env::var_os("CLAUDE_PROJECTS_DIR") {
            let path = PathBuf::from(dir);
            if path.is_dir() {
                return Some(Self::new(path));
            }
            return None;
        }
        let home = home_dir()?;
        let path = home.join(".claude").join("projects");
        path.is_dir().then(|| Self::new(path))
    }
}

// ── Id validation and paths ─────────────────────────────────────────────

/// Strict lowercase-hex UUID (`8-4-4-4-12`). This one check rejects `..`,
/// separators, absolute paths and symlink tricks before any path is built.
pub fn is_valid_session_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (i, b) in bytes.iter().enumerate() {
        match i {
            8 | 13 | 18 | 23 => {
                if *b != b'-' {
                    return false;
                }
            }
            _ => {
                if !b.is_ascii_hexdigit() || b.is_ascii_uppercase() {
                    return false;
                }
            }
        }
    }
    true
}

/// Agent file suffix: the bare id after `agent-`, without the extension.
pub fn is_valid_agent_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(crate) fn agent_id_from_file_name(name: &str) -> Option<String> {
    let stem = name.strip_suffix(".jsonl")?;
    let id = stem.strip_prefix("agent-")?;
    is_valid_agent_id(id).then(|| id.to_string())
}

/// Locate `<root>/<any-project>/<id>.jsonl`, confined to the root.
pub fn resolve_session_path(src: &ClaudeSessionsSource, id: &str) -> Result<PathBuf> {
    if !is_valid_session_id(id) {
        bail!("无效的会话 ID");
    }
    let root = src
        .projects_root
        .canonicalize()
        .with_context(|| format!("会话根目录不可读: {}", src.projects_root.display()))?;
    let wanted = format!("{id}.jsonl");
    let entries = fs::read_dir(&root).with_context(|| format!("读取失败: {}", root.display()))?;
    for entry in entries.flatten() {
        // Never follow symlinks: a project directory is a real directory or it
        // is not one this module reads.
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let candidate = entry.path().join(&wanted);
        let Ok(canon) = candidate.canonicalize() else {
            continue;
        };
        if canon.starts_with(&root) && canon.is_file() {
            return Ok(canon);
        }
    }
    bail!("会话不存在: {id}")
}

// ── Pure helpers ────────────────────────────────────────────────────────

/// The directory walk, labelled with the source that owns `root`.
///
/// The label is passed in rather than read from a global so the grouping code can
/// be driven by a synthetic source (see `sessions`' tests) without depending on
/// this module at all.
pub(crate) fn candidates_for(root: &Path, source: &str) -> Vec<Candidate> {
    let mut out = Vec::new();
    let walk = walkdir::WalkDir::new(root)
        .min_depth(1)
        .max_depth(1)
        .follow_links(false)
        .sort_by_file_name();
    for entry in walk.into_iter().flatten() {
        if !entry.file_type().is_dir() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let Ok(children) = fs::read_dir(entry.path()) else {
            continue;
        };
        for child in children.flatten() {
            if !child.file_type().map(|t| t.is_file()).unwrap_or(false) {
                continue;
            }
            let fname = child.file_name().to_string_lossy().to_string();
            let Some(stem) = fname.strip_suffix(".jsonl") else {
                continue;
            };
            if !is_valid_session_id(stem) {
                continue;
            }
            let Ok(meta) = child.metadata() else {
                continue;
            };
            out.push(Candidate {
                path: child.path(),
                encoded_project: name.clone(),
                mtime: file_mtime_ms(&meta),
                // The scan already stat'ed the file; group totals reuse it
                // instead of paying a second pass over every session.
                size_bytes: meta.len(),
                id: stem.to_string(),
                source: source.to_string(),
            });
        }
    }
    out
}

/// The absolute working directory recorded in the head of one transcript.
///
/// Read only until the first line that carries a `cwd` (`GROUP_CWD_PROBE_BYTES`
/// at most). Used solely to label a project group — the directory name cannot be
/// decoded back into a path (`-` is a legal path segment too), so the group's
/// display name has to come from the transcript itself.
pub(crate) fn probe_cwd(path: &Path) -> Option<String> {
    let file = fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file.take(GROUP_CWD_PROBE_BYTES));
    let mut found = None;
    read_lines(reader.by_ref(), |line| {
        if found.is_some() {
            return;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        if let Some(cwd) = str_of(&v, "cwd").filter(|s| !s.is_empty()) {
            found = Some(cwd.to_string());
        }
    })
    .ok()?;
    found
}

// ── Public API ──────────────────────────────────────────────────────────
