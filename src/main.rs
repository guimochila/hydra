//! Hydra — a tmux popup overseer for Claude Code agents.
//!
//! Subcommands:
//!   hydra              Open the popup TUI (agents in the current tmux session).
//!   hydra ls           Print the agent list to stdout (headless; for verification).
//!   hydra hook <event> Record a lifecycle event (installed into Claude Code hooks).
//!   hydra install      Install hooks + a tmux popup keybinding.
//!   hydra uninstall    Remove them.

mod agent;
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

/// Resolve the current socket/session and collect agents. Shared by `ls` and the TUI.
pub fn current_agents(cache: &mut worktree::WorktreeCache) -> Vec<agent::Agent> {
    let socket = match tmux::current_socket() {
        Some(s) => s,
        None => return Vec::new(),
    };
    let session = match tmux::current_session(&socket) {
        Some(s) => s,
        None => return Vec::new(),
    };
    let states: Vec<_> = state::read_all()
        .into_iter()
        .filter(|s| s.socket == socket)
        .collect();
    agent::collect(&socket, &session, states, now_secs(), cache)
}

fn list_command() -> std::io::Result<()> {
    let mut cache = worktree::WorktreeCache::default();
    let agents = current_agents(&mut cache);
    if agents.is_empty() {
        println!("(no agents in this session)");
        return Ok(());
    }
    for a in &agents {
        let branch = a
            .worktree
            .as_ref()
            .and_then(|w| w.branch.clone())
            .unwrap_or_else(|| "-".into());
        let summary = a.state.task_summary.clone().unwrap_or_default();
        println!(
            "{} win {:>2}  {:<12} {:<28} {}",
            a.effective_status.glyph(),
            a.pane.window_index,
            branch,
            a.pane.window_name,
            summary
        );
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
         \x20 hydra install          Install hooks + tmux popup keybinding\n\
         \x20 hydra uninstall        Remove hooks + keybinding"
    );
}
