//! Correlate reported agent state with live tmux panes to produce the display list.
//!
//! The state files say *what each agent is doing* and *where it claims to live*;
//! `tmux list-panes` says *whether that pane still exists and its current window*.
//! Joining on `pane_id` means a dead agent's leftover file matches nothing and simply
//! drops out — no ghost rows, no separate liveness tracking.

use crate::state::{AgentState, Status};
use crate::tmux::Pane;
use crate::worktree::{IdleWorktree, ProjectWorktrees, WorktreeInfo};

/// A working agent that is joined to a live pane and ready to display.
#[derive(Debug, Clone)]
pub struct Agent {
    pub state: AgentState,
    pub pane: Pane,
    /// Status after applying the staleness rule (see `join_and_sort`).
    pub effective_status: Status,
    pub worktree: Option<WorktreeInfo>,
    /// Count of uncommitted changes in the worktree (throttled; 0 if unknown/clean).
    pub dirty: usize,
}

/// A working agent whose `WORKING` status hasn't refreshed in this many seconds is
/// shown as `UNKNOWN` (likely crashed). Idle/NeedsInput agents can legitimately sit
/// for a long time, so staleness only applies to `WORKING`. This is also
/// `Timings::stale_after_secs`'s default; callers now go through `config::load()`, so
/// this constant is only reachable from tests (hence the `allow`).
#[allow(dead_code)]
pub const STALE_AFTER_SECS: u64 = 900;

/// Pure core: join `states` (already for one socket) against live `panes`, keep only
/// those in `session_name` (`None` = every session on the socket), apply staleness,
/// and sort (NeedsInput first, then by window index). Worktree is left unresolved
/// here so this stays IO-free and testable.
pub fn join_and_sort(
    states: Vec<AgentState>,
    panes: &[Pane],
    session_name: Option<&str>,
    now: u64,
    stale_after: u64,
) -> Vec<Agent> {
    let mut agents: Vec<Agent> = states
        .into_iter()
        .filter_map(|state| {
            let pane = panes.iter().find(|p| p.pane_id == state.pane_id)?.clone();
            if session_name.is_some_and(|name| pane.session_name != name) {
                return None;
            }
            let effective_status =
                effective_status(state.status, state.updated_at, now, stale_after);
            Some(Agent {
                state,
                pane,
                effective_status,
                worktree: None,
                dirty: 0,
            })
        })
        .collect();

    agents.sort_by(|a, b| {
        status_rank(a.effective_status)
            .cmp(&status_rank(b.effective_status))
            .then(a.pane.window_index.cmp(&b.pane.window_index))
    });
    agents
}

/// Grace period before a dead agent's leftover state file is deleted. Generous on
/// purpose: leftovers are invisible anyway (the join hides them), so GC is pure disk
/// hygiene and must never race a pane that is briefly absent.
pub const GC_GRACE_SECS: u64 = 3600;

/// State files whose pane no longer exists on this socket AND whose last update is
/// older than `grace_secs` — safe to delete. Returns their (socket, pane_id) keys.
/// `states` must already be filtered to the socket `panes` was listed from.
pub fn dead_states(
    states: &[AgentState],
    panes: &[Pane],
    now: u64,
    grace_secs: u64,
) -> Vec<(String, String)> {
    states
        .iter()
        .filter(|s| !panes.iter().any(|p| p.pane_id == s.pane_id))
        .filter(|s| now.saturating_sub(s.updated_at) > grace_secs)
        .map(|s| (s.socket.clone(), s.pane_id.clone()))
        .collect()
}

/// Candidate anchor directories for idle-worktree discovery: each agent's worktree
/// root in display order, then the popup's own cwd. Order matters — the caller
/// dedupes by repo identity taking the first anchor, so idle-only repos (reachable
/// just via the popup cwd) group after the repos that hold agents.
pub fn idle_anchors(agents: &[Agent], popup_cwd: Option<&str>) -> Vec<String> {
    let mut anchors: Vec<String> = agents
        .iter()
        .filter_map(|a| a.worktree.as_ref().map(|w| w.root.clone()))
        .collect();
    if let Some(cwd) = popup_cwd {
        anchors.push(cwd.to_string());
    }
    anchors
}

/// The per-row detail text: while the agent needs input, the live attention message
/// (why it's blocked — more actionable than the prompt that got it there); otherwise
/// the last task summary.
pub fn detail_text(a: &Agent) -> Option<String> {
    if a.effective_status == Status::NeedsInput {
        if let Some(attention) = &a.state.attention {
            return Some(attention.clone());
        }
    }
    a.state.task_summary.clone()
}

/// Truncate to at most `max` chars (char-boundary safe), adding an ellipsis. Shared
/// by the hook (task summaries / attention) and the row renderer (width fitting).
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Format an age in seconds compactly: `12s`, `4m`, `2h`, `3d`.
pub fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Canonical worktree roots that have a live agent — the occupancy set for `idle_from`.
/// Deliberately spans EVERY session on the socket, not just the displayed (possibly
/// session-filtered) agents: in session spawn mode an agent lives in its own session, and
/// its worktree must not be offered as "idle" just because the current view is scoped to a
/// different session. Pure; `project.entries` are already canonical so we canonicalize here.
pub fn occupied_roots(agents: &[Agent]) -> std::collections::HashSet<String> {
    agents
        .iter()
        .filter_map(|a| a.worktree.as_ref().map(|w| canon(&w.root)))
        .collect()
}

/// Worktrees of the project that have no agent running in them. `occupied` is the set of
/// canonical roots from `occupied_roots`; `project.entries` paths are already canonical.
pub fn idle_from(
    occupied: &std::collections::HashSet<String>,
    project: &ProjectWorktrees,
) -> Vec<IdleWorktree> {
    project
        .entries
        .iter()
        .filter(|(path, _)| !occupied.contains(path))
        .map(|(path, branch)| IdleWorktree {
            path: path.clone(),
            branch: branch.clone(),
            repo_key: project.repo_key.clone(),
            repo_name: project.repo_name.clone(),
        })
        .collect()
}

/// Filter predicate for an idle worktree (branch / repo / path).
pub fn worktree_matches_filter(wt: &IdleWorktree, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    let branch = wt.branch.as_deref().unwrap_or("");
    [branch, wt.repo_name.as_str(), wt.path.as_str()]
        .iter()
        .any(|field| field.to_lowercase().contains(&q))
}

fn canon(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Which subset of the socket's agents the display shows. `Repo` compares each
/// agent's resolved `worktree.repo_key`, so agents must already have their worktree
/// resolved before filtering by it; `Session` and `All` need no worktree.
pub enum Scope<'a> {
    /// Only agents in this tmux session — the fallback when the popup's cwd isn't in
    /// a repo (so there's no repo identity to scope by).
    Session(&'a str),
    /// Agents whose worktree shares this repo identity, across every session on the
    /// socket. The default view when the popup opens inside a repo/worktree.
    Repo(&'a str),
    /// Every agent on the socket, regardless of session or repo.
    All,
}

/// Pick the display scope from the `s`-toggle state and the popup's context. The
/// default (toggle off) is repo-scoped when the popup's cwd resolves to a repo, so a
/// worktree sees the whole repo's agents across sessions; it falls back to the current
/// session only when there's no repo identity to scope by. The toggle on means the
/// whole socket.
pub fn choose_scope<'a>(
    all_sessions: bool,
    popup_repo_key: Option<&'a str>,
    session: &'a str,
) -> Scope<'a> {
    match (all_sessions, popup_repo_key) {
        (true, _) => Scope::All,
        (false, Some(key)) => Scope::Repo(key),
        (false, None) => Scope::Session(session),
    }
}

/// Whether `agent` belongs in the given display `scope`. A worktree-less agent never
/// matches a `Repo` scope (no `repo_key` to compare).
pub fn matches_scope(agent: &Agent, scope: &Scope) -> bool {
    match scope {
        Scope::Session(name) => agent.pane.session_name == *name,
        Scope::Repo(key) => agent.worktree.as_ref().is_some_and(|w| w.repo_key == *key),
        Scope::All => true,
    }
}

/// Case-insensitive substring match against branch, repo, task summary and window name.
/// An empty query matches everything.
pub fn matches_filter(agent: &Agent, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    let branch = agent
        .worktree
        .as_ref()
        .and_then(|w| w.branch.as_deref())
        .unwrap_or("");
    let repo = agent
        .worktree
        .as_ref()
        .map(|w| w.repo_name.as_str())
        .unwrap_or("");
    let summary = agent.state.task_summary.as_deref().unwrap_or("");
    let window = agent.pane.window_name.as_str();
    [branch, repo, summary, window]
        .iter()
        .any(|field| field.to_lowercase().contains(&q))
}

fn effective_status(status: Status, updated_at: u64, now: u64, stale_after: u64) -> Status {
    if status == Status::Working && now.saturating_sub(updated_at) > stale_after {
        Status::Unknown
    } else {
        status
    }
}

fn status_rank(s: Status) -> u8 {
    match s {
        Status::NeedsInput => 0,
        Status::Working => 1,
        Status::Idle => 2,
        Status::Unknown => 3,
    }
}

/// Every `(session, window_index)` whose pane cwd is rooted in `path` — equal to `path`
/// or under it on a path boundary, so `/r/wt-a` never matches `/r/wt-ab`. Deduplicated,
/// so a window with several panes is returned once. Used by worktree removal to tear
/// down all windows a worktree occupies: in session spawn mode that is the shell + agent
/// windows (killing them empties and destroys the dedicated session); in window mode it
/// is just the single agent window.
pub fn windows_under_path(panes: &[Pane], path: &str) -> Vec<(String, u32)> {
    let mut out: Vec<(String, u32)> = Vec::new();
    let prefix = format!("{path}/");
    for p in panes {
        if p.cwd == path || p.cwd.starts_with(&prefix) {
            let key = (p.session_name.clone(), p.window_index);
            if !out.contains(&key) {
                out.push(key);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pane_id: &str, status: Status, updated_at: u64) -> AgentState {
        AgentState {
            socket: "/sock".into(),
            session_id: "1".into(),
            pane_id: pane_id.into(),
            cwd: "/repo".into(),
            status,
            event: "x".into(),
            task_summary: None,
            attention: None,
            updated_at,
        }
    }

    fn pane(pane_id: &str, session: &str, window_index: u32) -> Pane {
        Pane {
            pane_id: pane_id.into(),
            session_name: session.into(),
            window_index,
            window_name: "claude".into(),
            cwd: "/repo".into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        }
    }

    fn agent_with(
        pane_id: &str,
        status: Status,
        window: u32,
        repo: Option<(&str, &str, Option<&str>)>,
    ) -> Agent {
        Agent {
            state: state(pane_id, status, 100),
            pane: pane(pane_id, "proj", window),
            effective_status: status,
            worktree: repo.map(|(key, name, branch)| WorktreeInfo {
                root: "/r".into(),
                repo_key: key.into(),
                repo_name: name.into(),
                branch: branch.map(String::from),
            }),
            dirty: 0,
        }
    }

    #[test]
    fn idle_from_excludes_occupied_worktrees() {
        // /wt/a is occupied; /wt/b and /repo/main are idle.
        let occupied = std::collections::HashSet::from([canon("/wt/a")]);
        let project = ProjectWorktrees {
            repo_key: "/k".into(),
            repo_name: "proj".into(),
            entries: vec![
                ("/repo/main".into(), Some("main".into())),
                ("/wt/a".into(), Some("a".into())),
                ("/wt/b".into(), Some("b".into())),
            ],
        };
        let idle = idle_from(&occupied, &project);
        let paths: Vec<&str> = idle.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, vec!["/repo/main", "/wt/b"]);
    }

    #[test]
    fn occupied_roots_spans_every_session_and_canonicalizes() {
        // Two agents in DIFFERENT sessions, each in its own worktree. Occupancy must not
        // be session-scoped: both roots count, so neither shows as idle in any view.
        let mut a = agent_with("%1", Status::Working, 1, Some(("/k", "proj", Some("a"))));
        a.pane.session_name = "sess-a".into();
        a.worktree.as_mut().unwrap().root = "/wt/a".into();
        let mut b = agent_with("%2", Status::Idle, 1, Some(("/k", "proj", Some("b"))));
        b.pane.session_name = "sess-b".into();
        b.worktree.as_mut().unwrap().root = "/wt/b".into();

        let roots = occupied_roots(&[a, b]);
        assert!(roots.contains(&canon("/wt/a")));
        assert!(roots.contains(&canon("/wt/b")));
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn dead_states_only_flags_long_gone_panes() {
        let now = 10_000;
        let states = vec![
            state("%1", Status::Idle, 0),           // pane alive → kept
            state("%2", Status::Working, 9_990),    // pane gone but recent → kept
            state("%3", Status::NeedsInput, 1_000), // pane gone + old → dead
        ];
        let panes = vec![pane("%1", "proj", 1)];
        let dead = dead_states(&states, &panes, now, GC_GRACE_SECS);
        assert_eq!(dead, vec![("/sock".to_string(), "%3".to_string())]);
    }

    #[test]
    fn idle_anchors_lists_agent_roots_first_then_popup_cwd() {
        let a1 = agent_with("%1", Status::Working, 1, Some(("/a/.git", "alpha", None)));
        let a2 = agent_with("%2", Status::Idle, 2, None); // no worktree → no anchor
        assert_eq!(
            idle_anchors(&[a1, a2], Some("/popup/repo")),
            vec!["/r".to_string(), "/popup/repo".to_string()]
        );
        assert!(idle_anchors(&[], None).is_empty());
    }

    #[test]
    fn detail_text_prefers_attention_only_while_needing_input() {
        let mut a = agent_with("%1", Status::NeedsInput, 1, None);
        a.state.task_summary = Some("build the api".into());
        a.state.attention = Some("needs permission to run Bash".into());
        assert_eq!(
            detail_text(&a),
            Some("needs permission to run Bash".to_string())
        );
        // Back to working: the attention reason no longer applies.
        a.effective_status = Status::Working;
        assert_eq!(detail_text(&a), Some("build the api".to_string()));
    }

    #[test]
    fn truncate_is_char_safe_and_adds_an_ellipsis() {
        assert_eq!(truncate("hi", 60), "hi");
        let t = truncate(&"a".repeat(100), 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.ends_with('…'));
        // Multibyte input must not panic and must land on a char boundary.
        let t = truncate("héllo wörld ☃ quite long indeed", 5);
        assert_eq!(t.chars().count(), 5);
    }

    #[test]
    fn format_age_uses_compact_units() {
        assert_eq!(format_age(5), "5s");
        assert_eq!(format_age(90), "1m");
        assert_eq!(format_age(3600), "1h");
        assert_eq!(format_age(90_000), "1d");
    }

    #[test]
    fn choose_scope_defaults_to_repo_then_falls_back_to_session() {
        // Popup in a repo, toggle off → repo-scoped (across sessions).
        assert!(matches!(
            choose_scope(false, Some("/k"), "proj"),
            Scope::Repo("/k")
        ));
        // Popup NOT in a repo, toggle off → session-scoped fallback.
        assert!(matches!(
            choose_scope(false, None, "proj"),
            Scope::Session("proj")
        ));
        // Toggle on → all sessions, regardless of repo context.
        assert!(matches!(choose_scope(true, Some("/k"), "proj"), Scope::All));
        assert!(matches!(choose_scope(true, None, "proj"), Scope::All));
    }

    #[test]
    fn matches_scope_filters_by_session_repo_or_all() {
        // agent_with puts the pane in session "proj"; repo_key comes from the worktree.
        let a = agent_with("%1", Status::Idle, 1, Some(("/k1", "alpha", None)));
        let b = agent_with("%2", Status::Idle, 1, None); // no worktree

        // Session scope keys off the tmux session name.
        assert!(matches_scope(&a, &Scope::Session("proj")));
        assert!(!matches_scope(&a, &Scope::Session("other")));

        // Repo scope keys off the resolved worktree's repo_key, across sessions.
        assert!(matches_scope(&a, &Scope::Repo("/k1")));
        assert!(!matches_scope(&a, &Scope::Repo("/k2")));
        // A worktree-less agent can never match a repo scope.
        assert!(!matches_scope(&b, &Scope::Repo("/k1")));

        // All keeps everything, worktree or not.
        assert!(matches_scope(&a, &Scope::All));
        assert!(matches_scope(&b, &Scope::All));
    }

    #[test]
    fn filter_matches_branch_repo_and_summary_case_insensitively() {
        let mut a = agent_with(
            "%1",
            Status::Idle,
            1,
            Some(("/a/.git", "alpha", Some("feat/pagination"))),
        );
        a.state.task_summary = Some("refactor cursors".into());
        assert!(matches_filter(&a, ""));
        assert!(matches_filter(&a, "PAGIN"));
        assert!(matches_filter(&a, "alpha"));
        assert!(matches_filter(&a, "cursor"));
        assert!(!matches_filter(&a, "nonexistent"));
    }

    #[test]
    fn joins_only_panes_that_still_exist() {
        let states = vec![
            state("%1", Status::Idle, 100),
            state("%2", Status::Working, 100), // no matching live pane
        ];
        let panes = vec![pane("%1", "proj", 1)];
        let agents = join_and_sort(states, &panes, Some("proj"), 100, STALE_AFTER_SECS);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state.pane_id, "%1");
    }

    #[test]
    fn filters_out_other_sessions() {
        let states = vec![
            state("%1", Status::Idle, 100),
            state("%2", Status::Idle, 100),
        ];
        let panes = vec![pane("%1", "proj", 1), pane("%2", "other", 1)];
        let agents = join_and_sort(states, &panes, Some("proj"), 100, STALE_AFTER_SECS);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].pane.session_name, "proj");
    }

    #[test]
    fn no_session_filter_keeps_every_session() {
        let states = vec![
            state("%1", Status::Idle, 100),
            state("%2", Status::Idle, 100),
        ];
        let panes = vec![pane("%1", "proj", 1), pane("%2", "other", 1)];
        let agents = join_and_sort(states, &panes, None, 100, STALE_AFTER_SECS);
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn sorts_needs_input_first_then_by_window() {
        let states = vec![
            state("%1", Status::Idle, 100),
            state("%2", Status::NeedsInput, 100),
            state("%3", Status::Working, 100),
            state("%4", Status::Working, 100),
        ];
        let panes = vec![
            pane("%1", "proj", 5),
            pane("%2", "proj", 4),
            pane("%3", "proj", 3),
            pane("%4", "proj", 1),
        ];
        let agents = join_and_sort(states, &panes, Some("proj"), 100, STALE_AFTER_SECS);
        let order: Vec<&str> = agents.iter().map(|a| a.state.pane_id.as_str()).collect();
        // NeedsInput(%2), then Working by window index (%4 win1, %3 win3), then Idle(%1).
        assert_eq!(order, vec!["%2", "%4", "%3", "%1"]);
    }

    #[test]
    fn stale_working_agent_becomes_unknown() {
        let states = vec![state("%1", Status::Working, 0)];
        let panes = vec![pane("%1", "proj", 1)];
        let agents = join_and_sort(states, &panes, Some("proj"), 10_000, STALE_AFTER_SECS);
        assert_eq!(agents[0].effective_status, Status::Unknown);
    }

    #[test]
    fn old_idle_agent_stays_idle() {
        // Idle agents can sit indefinitely; staleness must not touch them.
        let states = vec![state("%1", Status::Idle, 0)];
        let panes = vec![pane("%1", "proj", 1)];
        let agents = join_and_sort(states, &panes, Some("proj"), 10_000, STALE_AFTER_SECS);
        assert_eq!(agents[0].effective_status, Status::Idle);
    }

    #[test]
    fn windows_under_path_matches_rooted_windows_dedups_and_respects_boundary() {
        let mk = |id: &str, session: &str, win: u32, cwd: &str| Pane {
            pane_id: id.into(),
            session_name: session.into(),
            window_index: win,
            window_name: "w".into(),
            cwd: cwd.into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        };
        let panes = vec![
            mk("%1", "wt-a", 1, "/root/wt-a"),     // shell window (exact match)
            mk("%2", "wt-a", 2, "/root/wt-a"),     // agent window
            mk("%3", "wt-a", 2, "/root/wt-a/src"), // 2nd pane, same window -> dedup
            mk("%4", "other", 5, "/root/wt-ab"),   // boundary: must NOT match
            mk("%5", "other", 6, "/elsewhere"),    // unrelated
        ];
        let got = windows_under_path(&panes, "/root/wt-a");
        assert_eq!(got, vec![("wt-a".to_string(), 1), ("wt-a".to_string(), 2)]);
    }

    #[test]
    fn windows_under_path_empty_when_nothing_matches() {
        let panes: Vec<Pane> = Vec::new();
        assert!(windows_under_path(&panes, "/root/wt-a").is_empty());
    }
}
