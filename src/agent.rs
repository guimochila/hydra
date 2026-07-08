//! Correlate reported agent state with live tmux panes to produce the display list.
//!
//! The state files say *what each agent is doing* and *where it claims to live*;
//! `tmux list-panes` says *whether that pane still exists and its current window*.
//! Joining on `pane_id` means a dead agent's leftover file matches nothing and simply
//! drops out — no ghost rows, no separate liveness tracking.

use crate::state::{AgentState, Status};
use crate::tmux::Pane;
use crate::worktree::{Caches, IdleWorktree, ProjectWorktrees, WorktreeInfo};

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

/// Full pipeline: join `states` with the given live `panes`, and resolve each
/// surviving agent's worktree and (throttled) uncommitted-change count. The caller
/// provides `panes` (one `list_panes` call serves both this join and the GC).
pub fn collect(
    session_name: Option<&str>,
    states: Vec<AgentState>,
    panes: &[Pane],
    now: u64,
    caches: &mut Caches,
    stale_after: u64,
) -> Vec<Agent> {
    let mut agents = join_and_sort(states, panes, session_name, now, stale_after);
    for agent in &mut agents {
        agent.worktree = caches.worktree.resolve(&agent.pane.cwd);
        agent.dirty = caches.dirty.count(&agent.pane.cwd, now);
    }
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

/// Worktrees of the project that have no agent running in them. `project.entries` paths
/// are already canonical; agent worktree roots are canonicalized here to match.
pub fn idle_from(agents: &[Agent], project: &ProjectWorktrees) -> Vec<IdleWorktree> {
    let occupied: std::collections::HashSet<String> = agents
        .iter()
        .filter_map(|a| a.worktree.as_ref().map(|w| canon(&w.root)))
        .collect();
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
    fn idle_from_excludes_worktrees_with_a_running_agent() {
        // Agent occupies /wt/a; /wt/b and /repo/main are idle.
        let mut occupied_agent = agent_with("%1", Status::Idle, 1, Some(("/k", "proj", Some("a"))));
        occupied_agent.worktree.as_mut().unwrap().root = "/wt/a".into();
        let project = ProjectWorktrees {
            repo_key: "/k".into(),
            repo_name: "proj".into(),
            entries: vec![
                ("/repo/main".into(), Some("main".into())),
                ("/wt/a".into(), Some("a".into())),
                ("/wt/b".into(), Some("b".into())),
            ],
        };
        let idle = idle_from(&[occupied_agent], &project);
        let paths: Vec<&str> = idle.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, vec!["/repo/main", "/wt/b"]);
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
}
