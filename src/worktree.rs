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

/// An existing worktree of the project that has no agent running in it — a candidate
/// for starting one. Shares `repo_key`/`repo_name` with agents so the two group together.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdleWorktree {
    pub path: String,
    pub branch: Option<String>,
    pub repo_key: String,
    pub repo_name: String,
}

/// All worktrees of one repo, as listed by `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectWorktrees {
    pub repo_key: String,
    pub repo_name: String,
    /// (absolute path, branch) per worktree; branch is `None` when detached.
    pub entries: Vec<(String, Option<String>)>,
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
pub struct DirtyCache {
    map: HashMap<String, (u64, usize)>,
    ttl: u64,
}

impl Default for DirtyCache {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            ttl: DIRTY_TTL_SECS,
        }
    }
}

impl DirtyCache {
    /// Construct with an explicit TTL (from config).
    pub fn with_ttl(ttl: u64) -> Self {
        Self {
            map: HashMap::new(),
            ttl,
        }
    }

    /// Uncommitted-change count for `cwd`, recomputed only when older than the TTL.
    pub fn count(&mut self, cwd: &str, now: u64) -> usize {
        if let Some((checked_at, count)) = self.map.get(cwd) {
            if now.saturating_sub(*checked_at) < self.ttl {
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
    pub wt_list: WorktreeListCache,
}

impl Caches {
    /// Build caches with config-derived TTLs.
    pub fn new(dirty_ttl: u64, wt_list_ttl: u64) -> Self {
        Self {
            worktree: WorktreeCache::default(),
            dirty: DirtyCache::with_ttl(dirty_ttl),
            wt_list: WorktreeListCache::with_ttl(wt_list_ttl),
        }
    }

    /// Drop all cached data so the next fetch re-reads git/tmux from scratch, while
    /// PRESERVING the configured TTLs (a bare `Default` would reset them to the built-in
    /// constants). Called after a mutation (spawn/remove) so the change shows immediately.
    pub fn invalidate(&mut self) {
        let (dirty_ttl, wt_list_ttl) = (self.dirty.ttl, self.wt_list.ttl);
        *self = Caches::new(dirty_ttl, wt_list_ttl);
    }
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

/// Whether the worktree at `cwd` has uncommitted changes.
pub fn is_dirty(cwd: &str) -> bool {
    git_dirty_count(cwd) > 0
}

/// Remove the worktree at `path`, run from `base_cwd` (another worktree of the repo —
/// never the one being removed). `force` maps to `--force`, required for a worktree
/// with uncommitted changes. Branch is left intact. Errors carry git's stderr.
pub fn remove_worktree(base_cwd: &str, path: &str, force: bool) -> std::io::Result<()> {
    let mut args = vec!["worktree", "remove"];
    if force {
        args.push("--force");
    }
    args.push(path);
    let out = Command::new("git")
        .arg("-C")
        .arg(base_cwd)
        .args(&args)
        .output()?;
    if out.status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(
            String::from_utf8_lossy(&out.stderr).trim().to_string(),
        ))
    }
}

fn resolve_uncached(cwd: &str) -> Option<WorktreeInfo> {
    let root = git(cwd, &["rev-parse", "--show-toplevel"])?;
    let common_dir = abs_common_dir(cwd)?;
    let branch =
        git(cwd, &["rev-parse", "--abbrev-ref", "HEAD"]).filter(|b| b != "HEAD" && !b.is_empty());
    Some(WorktreeInfo {
        repo_name: repo_name_from_common_dir(&common_dir),
        repo_key: common_dir,
        root,
        branch,
    })
}

/// The repo's common `.git` dir as a canonical absolute path. `git rev-parse
/// --git-common-dir` can return a relative path (notably `.git` in the main worktree),
/// so we join with cwd and canonicalize — giving one stable key across all worktrees.
fn abs_common_dir(cwd: &str) -> Option<String> {
    let raw = git(cwd, &["rev-parse", "--git-common-dir"])?;
    let p = std::path::Path::new(&raw);
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::path::Path::new(cwd).join(p)
    };
    let canon = std::fs::canonicalize(&joined).unwrap_or(joined);
    Some(canon.to_string_lossy().into_owned())
}

/// Canonicalize a path for stable comparison (resolves `..` and symlinks, e.g.
/// `/tmp` → `/private/tmp` on macOS). Falls back to the input when it can't.
fn canon(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// List all worktrees of the repo containing `cwd` via `git worktree list --porcelain`.
pub fn list_worktrees(cwd: &str) -> Option<ProjectWorktrees> {
    let common_dir = abs_common_dir(cwd)?;
    let out = git(cwd, &["worktree", "list", "--porcelain"])?;
    let entries = parse_worktree_porcelain(&out)
        .into_iter()
        .map(|(path, branch)| (canon(&path), branch))
        .collect();
    Some(ProjectWorktrees {
        repo_name: repo_name_from_common_dir(&common_dir),
        repo_key: common_dir,
        entries,
    })
}

/// Parse `git worktree list --porcelain` into (path, branch) pairs. Bare entries are
/// skipped; detached worktrees have `None` branch.
fn parse_worktree_porcelain(out: &str) -> Vec<(String, Option<String>)> {
    let mut result = Vec::new();
    let mut path: Option<String> = None;
    let mut branch: Option<String> = None;
    let mut bare = false;
    for line in out.lines() {
        if line.is_empty() {
            if let Some(p) = path.take() {
                if !bare {
                    result.push((p, branch.take()));
                }
            }
            branch = None;
            bare = false;
        } else if let Some(p) = line.strip_prefix("worktree ") {
            path = Some(p.to_string());
        } else if let Some(b) = line.strip_prefix("branch ") {
            branch = Some(b.strip_prefix("refs/heads/").unwrap_or(b).to_string());
        } else if line == "bare" {
            bare = true;
        }
    }
    if let Some(p) = path.take() {
        if !bare {
            result.push((p, branch));
        }
    }
    result
}

/// Re-list a repo's worktrees at most every `WORKTREE_LIST_TTL_SECS`.
const WORKTREE_LIST_TTL_SECS: u64 = 5;

/// cwd → (checked_at, worktrees) throttled cache of `git worktree list` output.
pub struct WorktreeListCache {
    map: HashMap<String, (u64, Option<ProjectWorktrees>)>,
    ttl: u64,
}

impl Default for WorktreeListCache {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            ttl: WORKTREE_LIST_TTL_SECS,
        }
    }
}

impl WorktreeListCache {
    /// Construct with an explicit TTL (from config).
    pub fn with_ttl(ttl: u64) -> Self {
        Self {
            map: HashMap::new(),
            ttl,
        }
    }

    pub fn get(&mut self, cwd: &str, now: u64) -> Option<ProjectWorktrees> {
        if let Some((checked_at, value)) = self.map.get(cwd) {
            if now.saturating_sub(*checked_at) < self.ttl {
                return value.clone();
            }
        }
        let value = list_worktrees(cwd);
        self.map.insert(cwd.to_string(), (now, value.clone()));
        value
    }
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

    #[test]
    fn parses_worktree_porcelain() {
        let out = "worktree /repo/main\nHEAD abc\nbranch refs/heads/main\n\n\
                   worktree /wt/feat\nHEAD def\nbranch refs/heads/feat/x\n\n\
                   worktree /wt/detached\nHEAD ghi\ndetached\n";
        let entries = parse_worktree_porcelain(out);
        assert_eq!(
            entries,
            vec![
                ("/repo/main".to_string(), Some("main".to_string())),
                ("/wt/feat".to_string(), Some("feat/x".to_string())),
                ("/wt/detached".to_string(), None),
            ]
        );
    }

    #[test]
    fn skips_bare_entries() {
        let out = "worktree /repo/bare\nbare\n\nworktree /wt/a\nHEAD x\nbranch refs/heads/a\n";
        let entries = parse_worktree_porcelain(out);
        assert_eq!(entries, vec![("/wt/a".to_string(), Some("a".to_string()))]);
    }

    #[test]
    fn invalidate_preserves_configured_ttls_and_clears_data() {
        let mut caches = Caches::new(11, 22);
        caches.dirty.map.insert("x".into(), (0, 5));
        caches.wt_list.map.insert("y".into(), (0, None));
        caches.invalidate();
        assert_eq!(caches.dirty.ttl, 11, "dirty TTL must survive invalidate");
        assert_eq!(
            caches.wt_list.ttl, 22,
            "wt_list TTL must survive invalidate"
        );
        assert!(caches.dirty.map.is_empty(), "cached data must be cleared");
        assert!(caches.wt_list.map.is_empty(), "cached data must be cleared");
    }
}
