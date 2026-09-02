//! Hydra — a tmux popup overseer for Claude Code agents.
//!
//! Subcommands:
//!   hydra              Open the popup TUI (agents in the current tmux session).
//!   hydra ls           Print the agent list to stdout (headless; for verification).
//!   hydra hook <event> Record a lifecycle event (installed into Claude Code hooks).
//!   hydra install      Install hooks + a tmux popup keybinding.
//!   hydra uninstall    Remove them.
//!   hydra doctor       Check install health (hooks, binding, runtime dir).
//!
//! Internal (not shown in help): `hydra notify <title> <body>` shows one desktop
//! notification and exits. The hook spawns it detached so the blocking `notify-rust`
//! call never slows the hook down (see `alert.rs`).

mod agent;
mod alert;
mod config;
mod doctor;
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
        Some("doctor") => doctor::run(),
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
    /// Preview text for the UI's requested target (agent screen capture or idle
    /// worktree git summary). Filled by the fetch worker, not `current_overview` —
    /// like `scope_label`, it exists so the UI thread never shells out itself.
    pub preview: Option<(fetcher::PreviewTarget, String)>,
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
        let mut wts = agent::idle_from(&occupied, &project);
        // Branch state vs the default branch (throttled), for the badge. Only the default
        // branch itself is skipped — it's its own base. A *detached* worktree (branch
        // `None`) must still be classified: PR checkouts live there and can hold real
        // unmerged commits, so `!= Some(default)` rather than a `Some`-gated compare.
        for w in &mut wts {
            if w.branch.as_deref() != Some(project.default_branch.as_str()) {
                w.state = caches.branch.get(&w.path, &project.default_branch, now);
            }
        }
        idle.extend(wts);
    }

    Overview {
        agents,
        idle,
        scope_label,
        preview: None, // the fetch worker fills this for its requested target
    }
}

/// `hydra ls` column widths, mirroring the popup's row layout so the two agree.
/// The place and branch cells shrink from max toward min on a narrow terminal.
const LS_PLACE_MAX: usize = 18;
const LS_PLACE_MIN: usize = 10;
const LS_BRANCH_MAX: usize = 24;
const LS_BRANCH_MIN: usize = 16;
/// Wide enough for the longest badge (`no commits`) plus its gutter.
const LS_BADGE: usize = 11;
/// The free-text cell is the point of the row, so it gets a floor the other cells
/// yield to before it does.
const LS_DETAIL_MIN: usize = 24;
/// Row overhead: glyph, its gutter, the age cell, three column gutters, the badge.
const LS_OVERHEAD: usize = 1 + 1 + 3 + 2 + 2 + 2 + LS_BADGE;

fn list_command() -> std::io::Result<()> {
    let cfg = config::load();
    let mut caches = worktree::Caches::new(
        cfg.timings.dirty_ttl_secs,
        cfg.timings.worktree_list_ttl_secs,
        cfg.timings.ahead_behind_ttl_secs,
    );
    let overview = current_overview(&mut caches, cfg.timings.stale_after_secs, false);
    if overview.agents.is_empty() && overview.idle.is_empty() {
        println!("(no agents or worktrees in this session)");
        return Ok(());
    }
    let now = now_secs();
    let width = term_width();
    for a in &overview.agents {
        // Branch, falling back to the tmux window name for an agent outside a worktree —
        // same fallback the popup's rows use.
        let branch = a
            .worktree
            .as_ref()
            .and_then(|w| w.branch.clone())
            .unwrap_or_else(|| a.pane.window_name.clone());
        let dirty = if a.dirty > 0 {
            format!("Δ{}", a.dirty)
        } else {
            String::new()
        };
        println!(
            "{}",
            ls_row(
                a.effective_status.glyph(),
                &agent::format_age(now.saturating_sub(a.state.updated_at)),
                &format!("{}:{}", a.pane.session_name, a.pane.window_index),
                &branch,
                &dirty,
                &agent::detail_text(a).unwrap_or_default(),
                width,
            )
        );
    }
    for w in &overview.idle {
        // No agent, so no age or tmux location — the path is the useful free-text cell.
        println!(
            "{}",
            ls_row(
                "○",
                "—",
                "—",
                &agent::worktree_label(w),
                &ls_badge(w.state),
                &w.path,
                width,
            )
        );
    }
    Ok(())
}

/// The cleanup badge as plain text, matching the popup's wording.
fn ls_badge(state: Option<worktree::BranchState>) -> String {
    match state {
        Some(worktree::BranchState::Merged) => "merged".into(),
        Some(worktree::BranchState::NoCommits) => "no commits".into(),
        Some(worktree::BranchState::Diverged { ahead, behind }) if behind > 0 => {
            format!("↑{ahead} ↓{behind}")
        }
        Some(worktree::BranchState::Diverged { ahead, .. }) => format!("↑{ahead}"),
        None => String::new(),
    }
}

/// (place, branch, detail) cell widths for a terminal of `width`. Session-per-worktree
/// names and long branches would eat the whole row on a narrow terminal, so they give
/// up space down to their minimums before the free-text cell drops below its floor.
fn ls_widths(width: usize) -> (usize, usize, usize) {
    let avail = width.saturating_sub(LS_OVERHEAD);
    let mut place = LS_PLACE_MAX;
    let mut branch = LS_BRANCH_MAX;
    let shortfall = LS_DETAIL_MIN.saturating_sub(avail.saturating_sub(place + branch));
    if shortfall > 0 {
        let from_place = (place - LS_PLACE_MIN).min(shortfall);
        place -= from_place;
        branch -= (branch - LS_BRANCH_MIN).min(shortfall - from_place);
    }
    let detail = avail.saturating_sub(place + branch);
    (place, branch, detail)
}

/// Format one `hydra ls` row into aligned columns. Every variable cell is truncated to
/// its column — an over-long branch or a summary wider than the terminal would
/// otherwise push the following cells out of alignment, or wrap and shear the table.
/// Pure so the layout is testable without a tmux server.
fn ls_row(
    glyph: &str,
    age: &str,
    place: &str,
    branch: &str,
    badge: &str,
    detail: &str,
    width: usize,
) -> String {
    let (pw, bw, dw) = ls_widths(width);
    let place = agent::truncate(place, pw);
    let branch = agent::truncate(branch, bw);
    let detail = agent::truncate(detail, dw);
    format!(
        "{glyph} {age:>3}  {place:<pw$}  {branch:<bw$}  {badge:<gw$}{detail}",
        gw = LS_BADGE
    )
    .trim_end()
    .to_string()
}

/// Terminal width for `ls` column fitting. `size()` queries the terminal itself, so it
/// still reports the real width with stdout piped; the fallback covers having no
/// terminal at all (CI, a cron job) or an implausibly narrow one.
fn term_width() -> usize {
    match ratatui::crossterm::terminal::size() {
        Ok((cols, _)) if cols >= 40 => cols as usize,
        _ => 120,
    }
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
         \x20 hydra doctor             Check install health (hooks, binding, runtime dir)\n\
         \x20 hydra version            Print the hydra version\n\n\
         help/version also answer to -h/--help and -V/--version"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ls_row_keeps_columns_aligned_when_cells_overflow() {
        // A long branch and a long summary must not push the later cells right — they
        // get truncated into their own column instead.
        let short = ls_row("○", "3h", "cet-services:2", "main", "Δ8", "Post it.", 120);
        let long = ls_row(
            "●",
            "5s",
            "cet-services:2",
            "feat/b2b-provisioning-automate-jira-po",
            "Δ3",
            "Can you check this PR and why the CI is failing",
            120,
        );
        let (pw, bw, _) = ls_widths(120);
        let badge_col = 1 + 1 + 3 + 2 + pw + 2 + bw + 2;
        assert_eq!(
            short.chars().take(badge_col).count(),
            long.chars().take(badge_col).count()
        );
        // Same column start for the badge in both rows.
        assert_eq!(
            short.chars().skip(badge_col).take(2).collect::<String>(),
            "Δ8"
        );
        assert_eq!(
            long.chars().skip(badge_col).take(2).collect::<String>(),
            "Δ3"
        );
        assert!(
            long.contains("feat/b2b-provisioning-a…"),
            "over-long branch is truncated into its cell: {long}"
        );
    }

    #[test]
    fn ls_widths_protect_the_free_text_cell_on_a_narrow_terminal() {
        // Wide: both cells at max, everything left goes to the free text.
        let (place, branch, detail) = ls_widths(120);
        assert_eq!((place, branch), (LS_PLACE_MAX, LS_BRANCH_MAX));
        assert_eq!(place + branch + detail + LS_OVERHEAD, 120);

        // 80 cols: the place cell (long session-per-worktree names) yields first, and
        // the free text keeps its floor rather than being elided down to nothing.
        let (place, branch, detail) = ls_widths(80);
        assert_eq!(branch, LS_BRANCH_MAX, "branch is not touched first");
        assert!((LS_PLACE_MIN..LS_PLACE_MAX).contains(&place));
        assert!(detail >= LS_DETAIL_MIN, "detail floor held: {detail}");

        // Very narrow: both cells bottom out, but never below their minimums.
        let (place, branch, _) = ls_widths(50);
        assert_eq!((place, branch), (LS_PLACE_MIN, LS_BRANCH_MIN));
    }

    #[test]
    fn ls_row_never_exceeds_the_terminal_width() {
        // Wrapping is what shears the table: a row wider than the terminal wraps onto a
        // second line and every column below it looks misaligned.
        let row = ls_row(
            "○",
            "1h",
            "cet-services:4",
            "feat/b2b-provisioning-automate-jira-po",
            "Δ3",
            &"I want you to check this branch and tell me when ".repeat(4),
            100,
        );
        assert!(
            row.chars().count() <= 100,
            "row width {}",
            row.chars().count()
        );
    }

    #[test]
    fn ls_row_trims_a_row_with_no_free_text() {
        // An idle worktree with no badge must not leave trailing padding behind.
        let row = ls_row("○", "—", "—", "pr-1462 (detached)", "", "", 120);
        assert_eq!(row, row.trim_end(), "no trailing whitespace");
        assert!(row.ends_with("pr-1462 (detached)"));
    }

    #[test]
    fn ls_badge_wording_matches_the_popup() {
        assert_eq!(ls_badge(Some(worktree::BranchState::Merged)), "merged");
        assert_eq!(
            ls_badge(Some(worktree::BranchState::NoCommits)),
            "no commits"
        );
        assert_eq!(
            ls_badge(Some(worktree::BranchState::Diverged {
                ahead: 12,
                behind: 76
            })),
            "↑12 ↓76"
        );
        assert_eq!(
            ls_badge(Some(worktree::BranchState::Diverged {
                ahead: 32,
                behind: 0
            })),
            "↑32"
        );
        assert_eq!(ls_badge(None), "");
    }
}
