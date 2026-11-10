//! Claude Code, as a [`SessionSource`].
//!
//! A thin adapter: every method here delegates to `crate::claude_sessions`, which
//! keeps owning the transcript format. Nothing is moved or re-implemented, which
//! is what makes this a seam rather than a second implementation — the 17 tests
//! that pin real transcript shapes keep testing the code that runs.
//!
//! The one thing worth stating: `is_valid_id` is the *whole* path-safety story for
//! this source. [`crate::claude_sessions::is_valid_session_id`] rejects `..`,
//! separators, absolute paths and symlink tricks in a single strict lowercase-hex
//! UUID check, and callers must run it before building a path. Any future source
//! inherits that obligation, not this implementation.

use anyhow::Result;
use std::path::PathBuf;

use super::SessionSource;
use crate::claude_sessions::{
    self, Candidate, ClaudeSessionsSource, DetailArgs, SessionDetail, SessionSummary,
};

pub(crate) struct ClaudeSource;

impl ClaudeSource {
    fn open(&self) -> Result<ClaudeSessionsSource> {
        ClaudeSessionsSource::from_env().ok_or_else(|| {
            anyhow::anyhow!("未找到 ~/.claude/projects：当前用户没有 Claude Code 本地会话记录")
        })
    }
}

/// The same source pinned to an explicit root.
///
/// Test-only, and deliberately so. Production always goes through the registry
/// (which reads `$HOME`); the fixture-based format tests inside `claude_sessions`
/// need to point the *same* implementation at a temp directory, and duplicating
/// the adapter there would mean those tests stopped exercising the code the
/// console runs. It is the one behaviour [`ClaudeSource`] has that cannot be
/// injected any other way, because a trait that took a root would let a caller
/// name an arbitrary path.
#[cfg(test)]
pub(crate) struct ClaudeSourceAt {
    pub(crate) root: PathBuf,
}

#[cfg(test)]
impl SessionSource for ClaudeSourceAt {
    fn slug(&self) -> &'static str {
        super::CLAUDE_SLUG
    }
    fn kind(&self) -> &'static str {
        "claude_local"
    }
    fn label(&self) -> &'static str {
        "Claude Code"
    }
    fn gateway_prefix(&self) -> &'static str {
        claude_sessions::GATEWAY_PREFIX
    }
    fn is_valid_id(&self, id: &str) -> bool {
        claude_sessions::is_valid_session_id(id)
    }
    fn root(&self) -> Option<PathBuf> {
        Some(self.root.clone())
    }
    fn scan(&self) -> Result<Vec<Candidate>> {
        Ok(claude_sessions::candidates_for(&self.root, self.slug()))
    }
    fn summary(&self, c: &Candidate) -> Result<SessionSummary> {
        claude_sessions::summary_from_file(&c.path, &c.encoded_project)
    }
    fn read(&self, id: &str, args: &DetailArgs) -> Result<SessionDetail> {
        claude_sessions::read_session(&ClaudeSessionsSource::new(self.root.clone()), id, args)
    }
    fn read_subagent(&self, id: &str, agent: &str, args: &DetailArgs) -> Result<SessionDetail> {
        claude_sessions::read_subagent(
            &ClaudeSessionsSource::new(self.root.clone()),
            id,
            agent,
            args,
        )
    }
}

impl SessionSource for ClaudeSource {
    fn slug(&self) -> &'static str {
        super::CLAUDE_SLUG
    }

    fn kind(&self) -> &'static str {
        "claude_local"
    }

    fn label(&self) -> &'static str {
        "Claude Code"
    }

    fn gateway_prefix(&self) -> &'static str {
        claude_sessions::GATEWAY_PREFIX
    }

    fn is_valid_id(&self, id: &str) -> bool {
        claude_sessions::is_valid_session_id(id)
    }

    fn root(&self) -> Option<PathBuf> {
        ClaudeSessionsSource::from_env().map(|s| s.projects_root)
    }

    fn scan(&self) -> Result<Vec<Candidate>> {
        let src = self.open()?;
        Ok(claude_sessions::candidates_for(
            &src.projects_root,
            self.slug(),
        ))
    }

    fn summary(&self, c: &Candidate) -> Result<SessionSummary> {
        claude_sessions::summary_from_file(&c.path, &c.encoded_project)
    }

    fn read(&self, id: &str, args: &DetailArgs) -> Result<SessionDetail> {
        let src = self.open()?;
        claude_sessions::read_session(&src, id, args)
    }

    fn read_subagent(&self, id: &str, agent: &str, args: &DetailArgs) -> Result<SessionDetail> {
        let src = self.open()?;
        claude_sessions::read_subagent(&src, id, agent, args)
    }
}
