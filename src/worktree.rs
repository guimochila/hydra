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

/// Re-check a worktree's uncommitted-change count at most every `DIRTY_TTL_SECS`.
/// Running `git status` on every 250 ms refresh tick would be wasteful, and the count
/// changes slowly relative to that.
const DIRTY_TTL_SECS: u64 = 3;

/// cwd → (checked_at, count) throttled cache of uncommitted-change counts.
#[derive(Default)]
pub struct DirtyCache {
    map: HashMap<String, (u64, usize)>,
}

impl DirtyCache {
    /// Uncommitted-change count for `cwd`, recomputed only when older than the TTL.
    pub fn count(&mut self, cwd: &str, now: u64) -> usize {
        if let Some((checked_at, count)) = self.map.get(cwd) {
            if now.saturating_sub(*checked_at) < DIRTY_TTL_SECS {
                return *count;
            }
        }
        let count = git_dirty_count(cwd);
        self.map.insert(cwd.to_string(), (now, count));
        count
    }
}

/// The caches the agent pipeline threads through: stable worktree identity + volatile
/// dirty counts. Bundled so callers pass one thing.
#[derive(Default)]
pub struct Caches {
    pub worktree: WorktreeCache,
    pub dirty: DirtyCache,
}

/// The repo's default branch, resolved from `origin/HEAD`, falling back to a local
/// `main`/`master`, then to `"main"`. Used as the base for spawned worktrees.
pub fn default_branch(cwd: &str) -> String {
    if let Some(head) = git(
        cwd,
        &[
            "symbolic-ref",
            "--quiet",
            "--short",
            "refs/remotes/origin/HEAD",
        ],
    ) {
        if let Some(branch) = head.rsplit('/').next() {
            if !branch.is_empty() {
                return branch.to_string();
            }
        }
    }
    for candidate in ["main", "master"] {
        if git(cwd, &["rev-parse", "--verify", "--quiet", candidate]).is_some() {
            return candidate.to_string();
        }
    }
    "main".to_string()
}

/// Create a new worktree at `path` on a new branch `branch` based on `base_branch`,
/// run from `base_cwd` (any existing worktree of the repo). Errors carry git's stderr.
pub fn create_worktree(
    base_cwd: &str,
    path: &str,
    branch: &str,
    base_branch: &str,
) -> std::io::Result<()> {
    let out = Command::new("git")
        .arg("-C")
        .arg(base_cwd)
        .args(["worktree", "add", "-b", branch, path, base_branch])
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

fn git_dirty_count(cwd: &str) -> usize {
    match git(cwd, &["status", "--porcelain"]) {
        Some(out) => out.lines().filter(|l| !l.is_empty()).count(),
        None => 0,
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
