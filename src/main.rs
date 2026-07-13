//! Hydra — a tmux popup overseer for Claude Code agents.
//!
//! Subcommands:
//!   hydra              Open the popup TUI (agents in the current tmux session).
//!   hydra ls           Print the agent list to stdout (headless; for verification).
//!   hydra hook <event> Record a lifecycle event (installed into Claude Code hooks).
//!   hydra install      Install hooks + a tmux popup keybinding.
//!   hydra uninstall    Remove them.
//!
//! Internal (not shown in help): `hydra notify <title> <body>` shows one desktop
//! notification and exits. The hook spawns it detached so the blocking `notify-rust`
//! call never slows the hook down (see `alert.rs`).

mod agent;
mod alert;
mod config;
mod fetcher;
mod hook;
mod install;
mod state;
mod status;
mod tmux;
mod ui;
mod worktree;

use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let cmd = args.first().map(String::as_str);

    let result: std::io::Result<()> = match cmd {
        None => ui::run(),
        Some("ls") => list_command(),
        Some("status") => status::run(
            args.get(1).map(String::as_str).unwrap_or(""),
            args.get(2).map(String::as_str).unwrap_or(""),
        ),
        Some("hook") => hook::run(args.get(1).map(String::as_str).unwrap_or("")),
        Some("notify") => {
            // Internal: shows one desktop notification and exits. Spawned detached by
            // the hook (via alert::spawn_notify) so notify-rust's blocking call never
            // slows the hook. Kept out of `print_help` — not a user-facing command.
            alert::show(
                args.get(1).map(String::as_str).unwrap_or(""),
                args.get(2).map(String::as_str).unwrap_or(""),
            );
            Ok(())
        }
        Some("install") => install::install(),
        Some("uninstall") => install::uninstall(),
        Some("help") | Some("-h") | Some("--help") => {
            print_help();
            Ok(())
        }
        Some("version") | Some("-V") | Some("--version") => {
            println!("hydra {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => {
            eprintln!("hydra: unknown command '{other}'\n");
            print_help();
            return ExitCode::from(2);
        }
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("hydra: {e}");
            ExitCode::FAILURE
        }
    }
}

/// The in-scope running agents plus the project's idle worktrees.
#[derive(Default)]
pub struct Overview {
    pub agents: Vec<agent::Agent>,
    pub idle: Vec<worktree::IdleWorktree>,
    /// Human label for the active view scope (repo name / `"all sessions"` / session
    /// name), computed here so the UI thread renders the header without any git/tmux
    /// work of its own.
    pub scope_label: String,
}

/// Resolve the current socket/session, collect agents (session-scoped, or every
/// session on the socket when `all_sessions`), and list idle worktrees across all
/// repos in view. Shared by `ls` and the TUI.
pub fn current_overview(
    caches: &mut worktree::Caches,
    stale_after: u64,
    all_sessions: bool,
) -> Overview {
    let socket = match tmux::current_socket() {
        Some(s) => s,
        None => return Overview::default(),
    };
    let session = match tmux::current_session(&socket) {
        Some(s) => s,
        None => return Overview::default(),
    };
    let states: Vec<_> = state::read_all()
        .into_iter()
        .filter(|s| s.socket == socket)
        .collect();
    let now = now_secs();
    let panes = tmux::list_panes(&socket);

    // GC: a state file whose pane is long gone (crashed agent, no SessionEnd) is
    // invisible in the join but would otherwise sit on disk forever. Best-effort.
    for (sock, pane_id) in agent::dead_states(&states, &panes, now, agent::GC_GRACE_SECS) {
        let _ = state::remove_state(&sock, &pane_id);
    }

    // Resolve every agent's worktree once (roots + repo_key). This one pass serves both
    // occupancy — which must span EVERY session on the socket so a session-mode agent's
    // worktree never shows as idle — and the repo-scoped display filter below.
    let mut all_agents = agent::join_and_sort(states, &panes, None, now, stale_after);
    for a in &mut all_agents {
        a.worktree = caches.worktree.resolve(&a.pane.cwd);
    }
    let occupied = agent::occupied_roots(&all_agents);

    // The popup's own cwd → its repo identity. Drives both the default scope (repo-scoped
    // when we're inside a repo) and idle discovery.
    let popup_cwd = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string());
    let popup_wt = popup_cwd
        .as_deref()
        .and_then(|p| caches.worktree.resolve(p));
    let popup_repo_key = popup_wt.as_ref().map(|w| w.repo_key.as_str());

    // Default view is repo-scoped (this repo's agents across sessions); `s` flips to the
    // whole socket; a non-repo popup cwd falls back to the current session.
    let scope = agent::choose_scope(all_sessions, popup_repo_key, &session);
    let scope_label = match &scope {
        agent::Scope::All => "all sessions".to_string(),
        // In repo scope popup_wt is always Some (that's why we chose Repo).
        agent::Scope::Repo(_) => popup_wt
            .as_ref()
            .map(|w| w.repo_name.clone())
            .unwrap_or_else(|| session.clone()),
        agent::Scope::Session(_) => session.clone(),
    };

    // Display set: filter the resolved agents by scope, then add throttled dirty counts
    // only for what's shown.
    let mut agents: Vec<agent::Agent> = all_agents
        .into_iter()
        .filter(|a| agent::matches_scope(a, &scope))
        .collect();
    for a in &mut agents {
        a.dirty = caches.dirty.count(&a.pane.cwd, now);
    }

    // Idle worktrees for every repo in view: each displayed agent's repo plus the popup's
    // own cwd (when it's in a repo) — deduped by repo identity, first anchor wins.
    let popup_anchor = popup_wt.as_ref().and(popup_cwd.as_deref());
    let mut idle = Vec::new();
    let mut seen_repos = std::collections::HashSet::new();
    for anchor in agent::idle_anchors(&agents, popup_anchor) {
        let Some(project) = caches.wt_list.get(&anchor, now) else {
            continue;
        };
        if !seen_repos.insert(project.repo_key.clone()) {
            continue;
        }
        idle.extend(agent::idle_from(&occupied, &project));
    }

    Overview {
        agents,
        idle,
        scope_label,
    }
}

fn list_command() -> std::io::Result<()> {
    let cfg = config::load();
    let mut caches = worktree::Caches::new(
        cfg.timings.dirty_ttl_secs,
        cfg.timings.worktree_list_ttl_secs,
    );
    let overview = current_overview(&mut caches, cfg.timings.stale_after_secs, false);
    if overview.agents.is_empty() && overview.idle.is_empty() {
        println!("(no agents or worktrees in this session)");
        return Ok(());
    }
    for a in &overview.agents {
        let branch = a
            .worktree
            .as_ref()
            .and_then(|w| w.branch.clone())
            .unwrap_or_else(|| "-".into());
        let summary = agent::detail_text(a).unwrap_or_default();
        println!(
            "{} win {:>2}  {:<20} {:<28} {}",
            a.effective_status.glyph(),
            a.pane.window_index,
            branch,
            a.pane.window_name,
            summary
        );
    }
    for w in &overview.idle {
        let branch = w.branch.clone().unwrap_or_else(|| "(detached)".into());
        println!("○ idle    {:<20} {}  start", branch, w.path);
    }
    Ok(())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn print_help() {
    println!(
        "hydra — tmux Claude Code agent overseer\n\n\
         USAGE:\n\
         \x20 hydra                    Open the popup TUI\n\
         \x20 hydra ls                 Print the agent list (headless)\n\
         \x20 hydra status <sock> <s>  Print the status-line indicator for a session\n\
         \x20 hydra hook <event>       Record a Claude Code lifecycle event\n\
         \x20 hydra install            Install hooks + tmux popup keybinding\n\
         \x20 hydra uninstall          Remove hooks + keybinding\n\
         \x20 hydra version            Print the hydra version"
    );
}
