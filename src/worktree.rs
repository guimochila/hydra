//! Resolve each agent's working directory to its git worktree/branch and the repo it
//! belongs to. With one worktree per agent, the branch is a per-agent label; the
//! common git dir groups agents of the same project under one header.
//!
//! Results are cached by cwd because a pane's directory rarely changes and git
//! subprocess calls would otherwise run on every refresh tick.

use std::collections::HashMap;
use std::process::Command;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeInfo {
    /// Toplevel of this worktree.
    pub root: String,
    /// The shared `.git` common dir — identity of the owning repo (grouping key).
    pub repo_key: String,
    /// Display name of the owning repo.
    pub repo_name: String,
    /// Current branch, or `None` when detached / not resolvable.
    pub branch: Option<String>,
}

/// cwd → resolved worktree info (or `None` when the path isn't in a git repo). The
/// `None` is cached too, so non-repo agents don't re-run git each tick.
#[derive(Default)]
pub struct WorktreeCache {
    map: HashMap<String, Option<WorktreeInfo>>,
}

impl WorktreeCache {
    pub fn resolve(&mut self, cwd: &str) -> Option<WorktreeInfo> {
        if let Some(cached) = self.map.get(cwd) {
            return cached.clone();
        }
        let info = resolve_uncached(cwd);
        self.map.insert(cwd.to_string(), info.clone());
        info
    }
}

fn resolve_uncached(cwd: &str) -> Option<WorktreeInfo> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let common_dir = git(cwd, &["rev-parse", "--git-common-dir"])?;
    let branch =
        git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD" && !b.is_empty());
    Some(WorktreeInfo {
        repo_name: repo_name_from_common_dir(&common_dir),
        repo_key: common_dir,
        root,
        branch,
    })
}

fn git(cwd: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Derive a human repo name from `git rev-parse --git-common-dir` output. That points
/// at the main repo's `.git` (e.g. `/home/me/proj/.git`), so the repo name is the
/// parent directory's basename.
fn repo_name_from_common_dir(common_dir: &str) -> String {
    let trimmed = common_dir.trim_end_matches('/');
    let without_git = trimmed.strip_suffix("/.git").unwrap_or(trimmed);
    // Bare repos or worktree admin paths: fall back to the last non-empty segment.
    let candidate = if without_git.ends_with(".git") {
        without_git.trim_end_matches(".git").trim_end_matches('/')
    } else {
        without_git
    };
    candidate
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(candidate)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repo_name_from_standard_git_dir() {
        assert_eq!(
            repo_name_from_common_dir("/Users/me/Personal/cet-services/.git"),
            "cet-services"
        );
    }

    #[test]
    fn repo_name_handles_trailing_slash() {
        assert_eq!(
            repo_name_from_common_dir("/Users/me/Personal/cet-services/.git/"),
            "cet-services"
        );
    }

    #[test]
    fn repo_name_from_bare_repo() {
        assert_eq!(repo_name_from_common_dir("/srv/git/myproj.git"), "myproj");
    }
}
