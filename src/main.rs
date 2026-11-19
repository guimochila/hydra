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
mod pr;
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

/// `hydra ls` badge column: wide enough for the longest badge (`no commits`) plus its
/// gutter.
const LS_BADGE: usize = 11;
/// Row overhead: glyph, its gutter, the age cell, three column gutters, the badge.
const LS_OVERHEAD: usize = 1 + 1 + 3 + 2 + 2 + 2 + LS_BADGE;

/// One `hydra ls` row's cells, before column fitting.
struct LsCells {
    glyph: &'static str,
    age: String,
    place: String,
    name: String,
    badge: String,
    detail: String,
}

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
    let mut rows: Vec<LsCells> = Vec::new();
    for a in &overview.agents {
        // Branch (the tmux window name outside a worktree) plus a differing worktree
        // folder — the same name cell the popup's rows use.
        let (label, hint) = agent::agent_name(a);
        rows.push(LsCells {
            glyph: a.effective_status.glyph(),
            age: agent::format_age(now.saturating_sub(a.state.updated_at)),
            place: format!("{}:{}", a.pane.session_name, a.pane.window_index),
            name: ls_name(&label, hint.as_deref()),
            badge: if a.dirty > 0 {
                format!("Δ{}", a.dirty)
            } else {
                String::new()
            },
            detail: agent::detail_text(a).unwrap_or_default(),
        });
    }
    for w in &overview.idle {
        // No agent, so no age or tmux location — the path is the useful free-text cell.
        let (label, hint) = agent::worktree_name(w);
        rows.push(LsCells {
            glyph: "○",
            age: "—".into(),
            place: "—".into(),
            name: ls_name(&label, hint.as_deref()),
            badge: ls_badge(w.state),
            detail: w.path.clone(),
        });
    }
    let widths = ls_widths(term_width(), &rows);
    for r in &rows {
        println!("{}", ls_row(r, widths));
    }
    Ok(())
}

/// The name cell as plain text: `label · folder` when the folder differs. The column is
/// sized to the widest name (`ls_widths`), so both halves show whenever the terminal
/// has the room; when it doesn't, truncation eats the folder before the branch.
fn ls_name(label: &str, hint: Option<&str>) -> String {
    match hint {
        Some(h) => format!("{label} · {h}"),
        None => label.to_string(),
    }
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

/// Cell widths for `rows` on a terminal of `width`: the popup's own split
/// (`agent::row_widths`), so the place and name columns are sized to their widest value
/// and give way (place first, then the free text down to a floor, then the name) only
/// when the terminal is too narrow.
fn ls_widths(width: usize, rows: &[LsCells]) -> agent::RowWidths {
    let want = |f: fn(&LsCells) -> &str| rows.iter().map(|r| f(r).chars().count()).max();
    agent::row_widths(
        width.saturating_sub(LS_OVERHEAD),
        want(|r| &r.place).unwrap_or(0),
        want(|r| &r.name).unwrap_or(0),
    )
}

/// Format one `hydra ls` row into aligned columns. Every variable cell is truncated to
/// its column — an over-long branch or a summary wider than the terminal would
/// otherwise push the following cells out of alignment, or wrap and shear the table.
/// Pure so the layout is testable without a tmux server.
fn ls_row(r: &LsCells, w: agent::RowWidths) -> String {
    let place = agent::truncate(&r.place, w.place);
    let name = agent::truncate(&r.name, w.name);
    let detail = agent::truncate(&r.detail, w.detail);
    format!(
        "{glyph} {age:>3}  {place:<pw$}  {name:<nw$}  {badge:<gw$}{detail}",
        glyph = r.glyph,
        age = r.age,
        badge = r.badge,
        pw = w.place,
        nw = w.name,
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

    fn cells(place: &str, name: &str, badge: &str, detail: &str) -> LsCells {
        LsCells {
            glyph: "●",
            age: "5s".into(),
            place: place.into(),
            name: name.into(),
            badge: badge.into(),
            detail: detail.into(),
        }
    }

    #[test]
    fn ls_row_keeps_columns_aligned_when_cells_overflow() {
        // A long branch and a long summary must not push the later cells right — they
        // get truncated into their own column instead.
        let rows = [
            cells("cet-services:2", "main", "Δ8", "Post it."),
            cells(
                "cet-services:2",
                "feat/b2b-provisioning-automate-jira-po",
                "Δ3",
                &"Can you check this PR and why the CI is failing ".repeat(4),
            ),
        ];
        let w = ls_widths(120, &rows);
        let short = ls_row(&rows[0], w);
        let long = ls_row(&rows[1], w);
        let badge_col = 1 + 1 + 3 + 2 + w.place + 2 + w.name + 2;
        assert_eq!(
            short.chars().skip(badge_col).take(2).collect::<String>(),
            "Δ8"
        );
        assert_eq!(
            long.chars().skip(badge_col).take(2).collect::<String>(),
            "Δ3"
        );
        assert!(long.chars().count() <= 120, "{long}");
    }

    #[test]
    fn ls_shows_long_branches_and_their_folder_in_full_when_there_is_room() {
        // The user's worktree: a 38-char branch in a folder named for another branch.
        let name = ls_name(
            "CET-3200/feat/cet-3723-partner-service",
            Some("CET-3200-feat-cet-3723-partner-identity"),
        );
        let rows = [cells(
            "cet-services-CET-3200-feat-cet-3723-partner-service:3",
            &name,
            "",
            "working on it",
        )];
        let row = ls_row(&rows[0], ls_widths(200, &rows));
        assert!(row.contains(&name), "{row}");
        assert!(row.ends_with("working on it"), "{row}");
        // Narrower: the place gives way before the branch does.
        let w = ls_widths(120, &rows);
        let row = ls_row(&rows[0], w);
        assert!(
            row.contains("CET-3200/feat/cet-3723-partner-service"),
            "{row}"
        );
        assert!(row.chars().count() <= 120);
    }

    #[test]
    fn ls_row_never_exceeds_the_terminal_width() {
        // Wrapping is what shears the table: a row wider than the terminal wraps onto a
        // second line and every column below it looks misaligned.
        let rows = [cells(
            "cet-services:4",
            "feat/b2b-provisioning-automate-jira-po",
            "Δ3",
            &"I want you to check this branch and tell me when ".repeat(4),
        )];
        for width in [60, 80, 100] {
            let row = ls_row(&rows[0], ls_widths(width, &rows));
            assert!(
                row.chars().count() <= width,
                "row width {}",
                row.chars().count()
            );
        }
    }

    #[test]
    fn ls_row_trims_a_row_with_no_free_text() {
        // An idle worktree with no badge must not leave trailing padding behind.
        let rows = [cells("—", "pr-1462 (detached)", "", "")];
        let row = ls_row(&rows[0], ls_widths(120, &rows));
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
