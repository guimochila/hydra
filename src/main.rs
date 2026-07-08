//! Hydra — a tmux popup overseer for Claude Code agents.
//!
//! Subcommands:
//!   hydra              Open the popup TUI (agents in the current tmux session).
//!   hydra ls           Print the agent list to stdout (headless; for verification).
//!   hydra hook <event> Record a lifecycle event (installed into Claude Code hooks).
//!   hydra install      Install hooks + a tmux popup keybinding.
//!   hydra uninstall    Remove them.

mod agent;
mod alert;
mod config;
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

/// The current session's running agents plus the project's idle worktrees.
#[derive(Default)]
pub struct Overview {
    pub agents: Vec<agent::Agent>,
    pub idle: Vec<worktree::IdleWorktree>,
}

/// Resolve the current socket/session, collect agents, and list the project's idle
/// worktrees. Shared by `ls` and the TUI.
pub fn current_overview(caches: &mut worktree::Caches, stale_after: u64) -> Overview {
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

    let agents = agent::collect(&session, states, &panes, now, caches, stale_after);

    // Anchor worktree listing at the popup's cwd, falling back to an agent's cwd.
    let anchor = std::env::current_dir()
        .ok()
        .map(|p| p.display().to_string())
        .filter(|p| caches.worktree.resolve(p).is_some())
        .or_else(|| agents.first().map(|a| a.pane.cwd.clone()));
    let idle = anchor
        .and_then(|cwd| caches.wt_list.get(&cwd, now))
        .map(|project| agent::idle_from(&agents, &project))
        .unwrap_or_default();

    Overview { agents, idle }
}

fn list_command() -> std::io::Result<()> {
    let cfg = config::load();
    let mut caches = worktree::Caches::new(
        cfg.timings.dirty_ttl_secs,
        cfg.timings.worktree_list_ttl_secs,
    );
    let overview = current_overview(&mut caches, cfg.timings.stale_after_secs);
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
        let summary = a.state.task_summary.clone().unwrap_or_default();
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
