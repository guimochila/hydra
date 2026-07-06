//! Correlate reported agent state with live tmux panes to produce the display list.
//!
//! The state files say *what each agent is doing* and *where it claims to live*;
//! `tmux list-panes` says *whether that pane still exists and its current window*.
//! Joining on `pane_id` means a dead agent's leftover file matches nothing and simply
//! drops out — no ghost rows, no separate liveness tracking.

use crate::state::{AgentState, Status};
use crate::tmux::Pane;
use crate::worktree::{WorktreeCache, WorktreeInfo};

/// A working agent that is joined to a live pane and ready to display.
#[derive(Debug, Clone)]
pub struct Agent {
    pub state: AgentState,
    pub pane: Pane,
    /// Status after applying the staleness rule (see `join_and_sort`).
    pub effective_status: Status,
    pub worktree: Option<WorktreeInfo>,
}

/// A working agent whose `WORKING` status hasn't refreshed in this many seconds is
/// shown as `UNKNOWN` (likely crashed). Idle/NeedsInput agents can legitimately sit
/// for a long time, so staleness only applies to `WORKING`.
pub const STALE_AFTER_SECS: u64 = 900;

/// Pure core: join `states` (already for one socket) against live `panes`, keep only
/// those in `session_name`, apply staleness, and sort (NeedsInput first, then by
/// window index). Worktree is left unresolved here so this stays IO-free and testable.
pub fn join_and_sort(
    states: Vec<AgentState>,
    panes: &[Pane],
    session_name: &str,
    now: u64,
    stale_after: u64,
) -> Vec<Agent> {
    let mut agents: Vec<Agent> = states
        .into_iter()
        .filter_map(|state| {
            let pane = panes.iter().find(|p| p.pane_id == state.pane_id)?.clone();
            if pane.session_name != session_name {
                return None;
            }
            let effective_status =
                effective_status(state.status, state.updated_at, now, stale_after);
            Some(Agent {
                state,
                pane,
                effective_status,
                worktree: None,
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

/// Full pipeline: read live panes on `socket`, join with `states`, and resolve each
/// surviving agent's worktree via the cache.
pub fn collect(
    socket: &str,
    session_name: &str,
    states: Vec<AgentState>,
    now: u64,
    cache: &mut WorktreeCache,
) -> Vec<Agent> {
    let panes = crate::tmux::list_panes(socket);
    let mut agents = join_and_sort(states, &panes, session_name, now, STALE_AFTER_SECS);
    for agent in &mut agents {
        agent.worktree = cache.resolve(&agent.pane.cwd);
    }
    agents
}

/// A repo header plus the indices (into the agent slice passed to `group_by_repo`) of
/// the agents beneath it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoGroup {
    pub label: String,
    pub indices: Vec<usize>,
}

/// Group agents under their owning repo, preserving first-appearance order so the
/// group holding the most urgent agent (agents arrive already status-sorted) comes
/// first. Grouped by the repo's common git dir; labelled by the repo name. Agents with
/// no resolvable worktree fall under a "no worktree" group.
pub fn group_by_repo(agents: &[Agent]) -> Vec<RepoGroup> {
    use std::collections::HashMap;
    let mut order: Vec<String> = Vec::new();
    let mut groups: HashMap<String, RepoGroup> = HashMap::new();
    for (i, a) in agents.iter().enumerate() {
        let (key, label) = match &a.worktree {
            Some(w) => (w.repo_key.clone(), w.repo_name.clone()),
            None => ("\u{0}none".to_string(), "no worktree".to_string()),
        };
        if !groups.contains_key(&key) {
            order.push(key.clone());
        }
        groups
            .entry(key)
            .or_insert_with(|| RepoGroup {
                label,
                indices: Vec::new(),
            })
            .indices
            .push(i);
    }
    order
        .into_iter()
        .filter_map(|k| groups.remove(&k))
        .collect()
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
        }
    }

    #[test]
    fn groups_agents_by_repo_common_dir() {
        let agents = vec![
            agent_with(
                "%1",
                Status::NeedsInput,
                1,
                Some(("/a/.git", "alpha", Some("feat"))),
            ),
            agent_with(
                "%2",
                Status::Working,
                2,
                Some(("/b/.git", "beta", Some("main"))),
            ),
            agent_with(
                "%3",
                Status::Idle,
                3,
                Some(("/a/.git", "alpha", Some("fix"))),
            ),
        ];
        let groups = group_by_repo(&agents);
        // alpha appears first (holds the NeedsInput agent) and gathers %1 and %3.
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].label, "alpha");
        assert_eq!(groups[0].indices, vec![0, 2]);
        assert_eq!(groups[1].label, "beta");
        assert_eq!(groups[1].indices, vec![1]);
    }

    #[test]
    fn agents_without_worktree_group_separately() {
        let agents = vec![
            agent_with("%1", Status::Idle, 1, None),
            agent_with("%2", Status::Idle, 2, Some(("/a/.git", "alpha", None))),
        ];
        let groups = group_by_repo(&agents);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].label, "no worktree");
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
        let agents = join_and_sort(states, &panes, "proj", 100, STALE_AFTER_SECS);
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
        let agents = join_and_sort(states, &panes, "proj", 100, STALE_AFTER_SECS);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].pane.session_name, "proj");
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
        let agents = join_and_sort(states, &panes, "proj", 100, STALE_AFTER_SECS);
        let order: Vec<&str> = agents.iter().map(|a| a.state.pane_id.as_str()).collect();
        // NeedsInput(%2), then Working by window index (%4 win1, %3 win3), then Idle(%1).
        assert_eq!(order, vec!["%2", "%4", "%3", "%1"]);
    }

    #[test]
    fn stale_working_agent_becomes_unknown() {
        let states = vec![state("%1", Status::Working, 0)];
        let panes = vec![pane("%1", "proj", 1)];
        let agents = join_and_sort(states, &panes, "proj", 10_000, STALE_AFTER_SECS);
        assert_eq!(agents[0].effective_status, Status::Unknown);
    }

    #[test]
    fn old_idle_agent_stays_idle() {
        // Idle agents can sit indefinitely; staleness must not touch them.
        let states = vec![state("%1", Status::Idle, 0)];
        let panes = vec![pane("%1", "proj", 1)];
        let agents = join_and_sort(states, &panes, "proj", 10_000, STALE_AFTER_SECS);
        assert_eq!(agents[0].effective_status, Status::Idle);
    }
}
