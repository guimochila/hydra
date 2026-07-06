//! `hydra status <socket> <session>` — a compact, always-on indicator for the tmux
//! status line. Meant to be invoked by tmux from `status-right` (via `#(...)`) on each
//! `status-interval`, so there's no daemon: tmux polls us, we print counts, done.
//!
//! Socket and session are passed as args (tmux expands `#{socket_path}` /
//! `#{session_name}`) because a command run from the status line has no `$TMUX`.

use crate::agent;
use crate::state::{self, Status};
use std::time::{SystemTime, UNIX_EPOCH};

/// Print the indicator for the given server/session. Output embeds tmux `#[fg=...]`
/// colour directives and is empty when the session has no agents (so the status line
/// stays clean).
pub fn run(socket: &str, session: &str) -> std::io::Result<()> {
    let states: Vec<_> = state::read_all()
        .into_iter()
        .filter(|s| s.socket == socket)
        .collect();
    let panes = crate::tmux::list_panes(socket);
    let agents = agent::join_and_sort(states, &panes, session, now_secs(), agent::STALE_AFTER_SECS);

    let mut needs = 0;
    let mut working = 0;
    let mut idle = 0;
    let mut unknown = 0;
    for a in &agents {
        match a.effective_status {
            Status::NeedsInput => needs += 1,
            Status::Working => working += 1,
            Status::Idle => idle += 1,
            Status::Unknown => unknown += 1,
        }
    }

    let indicator = format_indicator(needs, working, idle, unknown);
    if !indicator.is_empty() {
        print!("{indicator}");
    }
    Ok(())
}

/// Build the status string. NeedsInput first (most urgent), then working, idle, and
/// stale/unknown (`?`). Empty when there are no agents at all.
fn format_indicator(needs: usize, working: usize, idle: usize, unknown: usize) -> String {
    if needs + working + idle + unknown == 0 {
        return String::new();
    }
    let mut parts = Vec::new();
    if needs > 0 {
        parts.push(format!("#[fg=yellow]⚠{needs}"));
    }
    if working > 0 {
        parts.push(format!("#[fg=green]●{working}"));
    }
    if idle > 0 {
        parts.push(format!("#[fg=colour244]○{idle}"));
    }
    if unknown > 0 {
        parts.push(format!("#[fg=colour244]?{unknown}"));
    }
    format!("#[fg=colour244]hydra {}#[fg=default]", parts.join(" "))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_when_no_agents() {
        assert_eq!(format_indicator(0, 0, 0, 0), "");
    }

    #[test]
    fn shows_each_present_status_with_counts() {
        let s = format_indicator(1, 2, 3, 4);
        assert!(s.contains("⚠1"));
        assert!(s.contains("●2"));
        assert!(s.contains("○3"));
        assert!(s.contains("?4"));
    }

    #[test]
    fn omits_zero_categories() {
        let s = format_indicator(0, 2, 0, 0);
        assert!(s.contains("●2"));
        assert!(!s.contains('⚠'));
        assert!(!s.contains('○'));
        assert!(!s.contains('?'));
    }

    #[test]
    fn stale_only_session_still_shows() {
        // A session whose agents are all stale must not render blank.
        let s = format_indicator(0, 0, 0, 2);
        assert!(s.contains("?2"));
    }

    #[test]
    fn needs_input_comes_first() {
        let s = format_indicator(1, 1, 1, 1);
        let warn = s.find('⚠').unwrap();
        let work = s.find('●').unwrap();
        assert!(warn < work, "needs-input should render before working");
    }
}
