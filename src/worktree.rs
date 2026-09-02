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
    /// How this worktree's HEAD relates to the repo's default branch, for the cleanup
    /// badge. `None` when unknown or when the worktree IS the default branch.
    pub state: Option<BranchState>,
}

/// How an idle worktree's HEAD relates to the repo's default branch — what the cleanup
/// badge reports.
///
/// The distinction that matters: `ahead == 0` (no commits the default branch lacks) is
/// true both for a branch whose work *landed* and for one that never had a commit, and
/// those mean opposite things for cleanup. Reporting `merged` for both is worse than
/// useless — `create_worktree` branches off the default branch, so EVERY worktree hydra
/// spawns starts at `ahead == 0` and would be badged "safe to delete" the moment it's
/// created. Worse, under a squash-merge workflow a genuinely merged branch keeps its
/// original commits off the default branch forever and so stays `ahead > 0`, making
/// `ahead == 0` a signal that can *only* fire on an empty branch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BranchState {
    /// No commits of its own, and HEAD sits on the default branch's first-parent chain:
    /// the branch never diverged, it's just an older point of the default branch itself.
    /// A freshly spawned (or abandoned-before-any-commit) worktree.
    NoCommits,
    /// No commits of its own, and HEAD is off the first-parent chain — it got into the
    /// default branch as the second parent of a merge commit. Genuinely merged.
    Merged,
    /// Has `ahead` commits the default branch lacks; `behind` is the reverse count.
    Diverged { ahead: usize, behind: usize },
}

/// All worktrees of one repo, as listed by `git worktree list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectWorktrees {
    pub repo_key: String,
    pub repo_name: String,
    /// The repo's default branch (base for ahead/behind badges and spawns).
    pub default_branch: String,
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

/// Re-check a worktree's branch state at most every `AHEAD_BEHIND_TTL_SECS`.
/// It only changes on commits, so this can be much lazier than the dirty count.
const AHEAD_BEHIND_TTL_SECS: u64 = 30;

/// path → (checked_at, state) throttled cache of branch state vs the default branch.
pub struct BranchStateCache {
    map: HashMap<String, (u64, Option<BranchState>)>,
    ttl: u64,
}

impl Default for BranchStateCache {
    fn default() -> Self {
        Self {
            map: HashMap::new(),
            ttl: AHEAD_BEHIND_TTL_SECS,
        }
    }
}

impl BranchStateCache {
    /// Construct with an explicit TTL (from config).
    pub fn with_ttl(ttl: u64) -> Self {
        Self {
            map: HashMap::new(),
            ttl,
        }
    }

    /// Branch state of `cwd` vs `base`, recomputed only when older than the TTL.
    pub fn get(&mut self, cwd: &str, base: &str, now: u64) -> Option<BranchState> {
        if let Some((checked_at, state)) = self.map.get(cwd) {
            if now.saturating_sub(*checked_at) < self.ttl {
                return *state;
            }
        }
        let state = branch_state(cwd, base);
        self.map.insert(cwd.to_string(), (now, state));
        state
    }
}

/// The caches the agent pipeline threads through: stable worktree identity + volatile
/// dirty counts. Bundled so callers pass one thing.
#[derive(Default)]
pub struct Caches {
    pub worktree: WorktreeCache,
    pub dirty: DirtyCache,
    pub wt_list: WorktreeListCache,
    pub branch: BranchStateCache,
}

impl Caches {
    /// Build caches with config-derived TTLs.
    pub fn new(dirty_ttl: u64, wt_list_ttl: u64, branch_ttl: u64) -> Self {
        Self {
            worktree: WorktreeCache::default(),
            dirty: DirtyCache::with_ttl(dirty_ttl),
            wt_list: WorktreeListCache::with_ttl(wt_list_ttl),
            branch: BranchStateCache::with_ttl(branch_ttl),
        }
    }

    /// Drop all cached data so the next fetch re-reads git/tmux from scratch, while
    /// PRESERVING the configured TTLs (a bare `Default` would reset them to the built-in
    /// constants). Called after a mutation (spawn/remove) so the change shows immediately.
    pub fn invalidate(&mut self) {
        let (dirty_ttl, wt_list_ttl, branch_ttl) =
            (self.dirty.ttl, self.wt_list.ttl, self.branch.ttl);
        *self = Caches::new(dirty_ttl, wt_list_ttl, branch_ttl);
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

/// Create a worktree at `path` on branch `branch`, run from `base_cwd` (any existing
/// worktree of the repo). A branch that doesn't exist yet is created from
/// `base_branch`; an existing one is checked out as-is — so re-spawning a name whose
/// worktree was removed resumes that branch instead of failing on `-b`. (git itself
/// still errors if the branch is checked out in another worktree.) Errors carry
/// git's stderr.
pub fn create_worktree(
    base_cwd: &str,
    path: &str,
    branch: &str,
    base_branch: &str,
) -> std::io::Result<()> {
    let exists = git(
        base_cwd,
        &[
            "rev-parse",
            "--verify",
            "--quiet",
            &format!("refs/heads/{branch}"),
        ],
    )
    .is_some();
    let args: Vec<&str> = if exists {
        vec!["worktree", "add", path, branch]
    } else {
        vec!["worktree", "add", "-b", branch, path, base_branch]
    };
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

/// One-shot, uncached cwd → worktree resolution, for user-initiated actions that
/// happen outside the cached fetch pipeline (e.g. anchoring a spawn on the popup's
/// own cwd). Prefer `WorktreeCache::resolve` on any recurring path.
pub fn resolve(cwd: &str) -> Option<WorktreeInfo> {
    resolve_uncached(cwd)
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
        default_branch: default_branch(cwd),
        entries,
    })
}

/// Preview text for an idle worktree: its recent commits, plus uncommitted files
/// when there are any. Shown in the popup's preview pane (fetched by the worker).
pub fn preview(path: &str) -> String {
    let log = git(path, &["log", "--oneline", "-8"]).unwrap_or_default();
    match git(path, &["status", "--short"]) {
        Some(status) => format!("{log}\n\n— uncommitted —\n{status}"),
        None => log,
    }
}

/// (ahead, behind) of `cwd`'s HEAD relative to `base` — commits only on HEAD vs only
/// on `base`. `None` when git fails (e.g. `base` doesn't exist).
pub fn ahead_behind(cwd: &str, base: &str) -> Option<(usize, usize)> {
    let out = git(
        cwd,
        &[
            "rev-list",
            "--left-right",
            "--count",
            &format!("HEAD...{base}"),
        ],
    )?;
    parse_ahead_behind(&out)
}

/// Classify `cwd`'s HEAD against `base` (the repo's default branch) for the cleanup
/// badge. See `BranchState` for why `ahead == 0` alone isn't enough. `None` when git
/// fails (e.g. `base` doesn't exist).
pub fn branch_state(cwd: &str, base: &str) -> Option<BranchState> {
    let (ahead, behind) = ahead_behind(cwd, base)?;
    if ahead > 0 {
        return Some(BranchState::Diverged { ahead, behind });
    }
    if head_on_first_parent_chain(cwd, base, behind) {
        Some(BranchState::NoCommits)
    } else {
        Some(BranchState::Merged)
    }
}

/// Whether `cwd`'s HEAD sits on `base`'s first-parent chain — the branch never diverged,
/// it's just an older point of `base` itself.
///
/// Only meaningful when HEAD is already an ancestor of `base` (i.e. `ahead == 0`), which
/// is also what bounds the walk: every first-parent commit between `base` and HEAD is
/// unreachable from HEAD and so counted in `behind`, so HEAD is at most `behind` steps
/// down the chain. That cap keeps this cheap on repos with long histories.
fn head_on_first_parent_chain(cwd: &str, base: &str, behind: usize) -> bool {
    let Some(head) = git(cwd, &["rev-parse", "HEAD"]) else {
        return false;
    };
    let limit = format!("--max-count={}", behind.saturating_add(1));
    match git(cwd, &["rev-list", "--first-parent", &limit, base]) {
        Some(chain) => chain.lines().any(|sha| sha == head),
        None => false,
    }
}

/// Parse `git rev-list --left-right --count` output (`"<ahead>\t<behind>"`).
fn parse_ahead_behind(out: &str) -> Option<(usize, usize)> {
    let mut f = out.split_whitespace();
    let ahead = f.next()?.parse().ok()?;
    let behind = f.next()?.parse().ok()?;
    Some((ahead, behind))
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

    /// Build a throwaway git repo with one commit on `main`, returning its path.
    /// Callers must remove it (and any worktrees) when done.
    fn temp_repo(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hydra-wt-test-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let run = |args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(&dir)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        run(&["init", "-b", "main"]);
        run(&[
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ]);
        dir
    }

    #[test]
    fn create_worktree_reuses_an_existing_branch() {
        let repo = temp_repo("reuse");
        let repo_s = repo.display().to_string();
        // The branch already exists (e.g. a previous spawn created it, worktree since
        // removed): create_worktree must check it out instead of failing on `-b`.
        let out = Command::new("git")
            .args(["-C", &repo_s, "branch", "feat"])
            .output()
            .unwrap();
        assert!(out.status.success());

        let wt = repo.join("wt-feat");
        let wt_s = wt.display().to_string();
        create_worktree(&repo_s, &wt_s, "feat", "main").expect("reuses the existing branch");
        assert_eq!(
            git(&wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).as_deref(),
            Some("feat")
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn create_worktree_still_creates_a_new_branch() {
        let repo = temp_repo("new");
        let repo_s = repo.display().to_string();
        let wt = repo.join("wt-fresh");
        let wt_s = wt.display().to_string();
        create_worktree(&repo_s, &wt_s, "fresh", "main").expect("creates a new branch");
        assert_eq!(
            git(&wt_s, &["rev-parse", "--abbrev-ref", "HEAD"]).as_deref(),
            Some("fresh")
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn parses_ahead_behind_counts() {
        assert_eq!(parse_ahead_behind("2\t5"), Some((2, 5)));
        assert_eq!(parse_ahead_behind("0\t0"), Some((0, 0)));
        assert_eq!(parse_ahead_behind("garbage"), None);
        assert_eq!(parse_ahead_behind(""), None);
    }

    #[test]
    fn ahead_behind_counts_commits_against_the_base_branch() {
        let repo = temp_repo("ab");
        let repo_s = repo.display().to_string();
        let wt = repo.join("wt-feat");
        let wt_s = wt.display().to_string();
        create_worktree(&repo_s, &wt_s, "feat", "main").unwrap();
        commit(&wt_s, "on-feat"); // one commit only on feat…
        commit(&repo_s, "on-main"); // …and one only on main
        assert_eq!(ahead_behind(&wt_s, "main"), Some((1, 1)));
        let _ = std::fs::remove_dir_all(&repo);
    }

    /// Commit with an explicit distinct message so two empty commits never collide on
    /// the same sha (identical message + author + timestamp would).
    fn commit(cwd: &str, msg: &str) {
        let out = Command::new("git")
            .args([
                "-C",
                cwd,
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "commit",
                "--allow-empty",
                "-m",
                msg,
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
    }

    #[test]
    fn branch_state_reports_no_commits_for_a_fresh_branch() {
        // The regression this guards: a worktree spawned off main, with nothing committed
        // on it yet, has ahead == 0 and used to render as `merged` — the list's strongest
        // "safe to delete" signal, on the branch you just started work on.
        let repo = temp_repo("fresh-state");
        let repo_s = repo.display().to_string();
        let wt = repo.join("wt-fresh");
        let wt_s = wt.display().to_string();
        create_worktree(&repo_s, &wt_s, "fresh", "main").unwrap();
        commit(&repo_s, "on-main-after"); // main moves on; the branch has no commits

        assert_eq!(ahead_behind(&wt_s, "main"), Some((0, 1)), "ahead is 0…");
        assert_eq!(
            branch_state(&wt_s, "main"),
            Some(BranchState::NoCommits),
            "…but it was never merged — it simply has no commits"
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_state_reports_no_commits_when_identical_to_the_base() {
        let repo = temp_repo("same-state");
        let repo_s = repo.display().to_string();
        let wt = repo.join("wt-same");
        let wt_s = wt.display().to_string();
        create_worktree(&repo_s, &wt_s, "same", "main").unwrap();
        assert_eq!(ahead_behind(&wt_s, "main"), Some((0, 0)));
        assert_eq!(branch_state(&wt_s, "main"), Some(BranchState::NoCommits));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_state_reports_merged_after_a_merge_commit() {
        let repo = temp_repo("merged-state");
        let repo_s = repo.display().to_string();
        let wt = repo.join("wt-done");
        let wt_s = wt.display().to_string();
        create_worktree(&repo_s, &wt_s, "done", "main").unwrap();
        commit(&wt_s, "real-work"); // a commit that actually lands…
        let out = Command::new("git")
            .args([
                "-C",
                &repo_s,
                "-c",
                "user.email=t@t",
                "-c",
                "user.name=t",
                "merge",
                "--no-ff",
                "-m",
                "merge done",
                "done",
            ])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );

        // ahead == 0 here too — but HEAD arrived as the merge's second parent, so it is
        // NOT on main's first-parent chain. That's what separates this from the case above.
        assert_eq!(ahead_behind(&wt_s, "main"), Some((0, 1)));
        assert_eq!(branch_state(&wt_s, "main"), Some(BranchState::Merged));
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_state_reports_divergence_when_the_branch_has_its_own_commits() {
        let repo = temp_repo("div-state");
        let repo_s = repo.display().to_string();
        let wt = repo.join("wt-div");
        let wt_s = wt.display().to_string();
        create_worktree(&repo_s, &wt_s, "div", "main").unwrap();
        commit(&wt_s, "on-div");
        commit(&repo_s, "on-main");
        assert_eq!(
            branch_state(&wt_s, "main"),
            Some(BranchState::Diverged {
                ahead: 1,
                behind: 1
            })
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn branch_state_of_a_detached_worktree_is_still_computed() {
        // Detached PR-review checkouts (`git worktree add --detach`) are the case that
        // used to get no badge at all: the caller skipped them for having no branch,
        // even though they can hold real unmerged commits.
        let repo = temp_repo("det-state");
        let repo_s = repo.display().to_string();
        commit(&repo_s, "base-for-detach");
        let wt = repo.join("wt-detached");
        let wt_s = wt.display().to_string();
        let out = Command::new("git")
            .args(["-C", &repo_s, "worktree", "add", "--detach", &wt_s, "HEAD"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        commit(&wt_s, "work-on-detached-head");

        assert_eq!(
            branch_state(&wt_s, "main"),
            Some(BranchState::Diverged {
                ahead: 1,
                behind: 0
            })
        );
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn preview_shows_recent_log_and_uncommitted_files() {
        let repo = temp_repo("prev");
        std::fs::write(repo.join("newfile.txt"), "x").unwrap();
        let text = preview(&repo.display().to_string());
        assert!(text.contains("init"), "recent commit subject shown");
        assert!(text.contains("newfile.txt"), "uncommitted file shown");
        let _ = std::fs::remove_dir_all(&repo);
    }

    #[test]
    fn list_worktrees_reports_the_default_branch() {
        let repo = temp_repo("db");
        let repo_s = repo.display().to_string();
        let project = list_worktrees(&repo_s).unwrap();
        assert_eq!(project.default_branch, "main");
        let _ = std::fs::remove_dir_all(&repo);
    }

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
        let mut caches = Caches::new(11, 22, 33);
        caches.dirty.map.insert("x".into(), (0, 5));
        caches.wt_list.map.insert("y".into(), (0, None));
        caches
            .branch
            .map
            .insert("z".into(), (0, Some(BranchState::NoCommits)));
        caches.invalidate();
        assert_eq!(caches.dirty.ttl, 11, "dirty TTL must survive invalidate");
        assert_eq!(
            caches.wt_list.ttl, 22,
            "wt_list TTL must survive invalidate"
        );
        assert_eq!(
            caches.branch.ttl, 33,
            "branch-state TTL must survive invalidate"
        );
        assert!(caches.dirty.map.is_empty(), "cached data must be cleared");
        assert!(caches.wt_list.map.is_empty(), "cached data must be cleared");
        assert!(caches.branch.map.is_empty(), "cached data must be cleared");
    }
}
