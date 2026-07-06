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

// Palette, matched to the user's dracula-lotus tmux theme. Tweak these to re-skin the
// indicator (or point them at another theme's colours).
const CREAM: &str = "#f2ecbc";
const ROSE: &str = "#b35b79"; // battery segment / label
const TEAL: &str = "#5e857a"; // working
const PEACH: &str = "#d9a594"; // idle
const ALERT_BG: &str = "#d7474b"; // needs-input block (the theme's own red)

/// Build the status string with tmux `#[...]` styling, in the theme palette above. When
/// any agent needs input the indicator leads with a soft-red "⚠ N NEEDS INPUT" block
/// that stands out without shouting; otherwise it's a compact `hydra ●N ○N ?N`. Empty
/// when there are no agents. `#[default]` at the end restores the bar's own style.
fn format_indicator(needs: usize, working: usize, idle: usize, unknown: usize) -> String {
    if needs + working + idle + unknown == 0 {
        return String::new();
    }

    let mut out = String::new();
    if needs > 0 {
        // Attention block: cream-on-theme-red, bold, padded — the "handle me" signal.
        out.push_str(&format!(
            "#[fg={CREAM},bg={ALERT_BG},bold] ⚠ {needs} NEEDS INPUT #[default]"
        ));
    } else {
        out.push_str(&format!("#[fg={ROSE},bold]hydra#[default]"));
    }

    // Compact counts for the non-urgent states (needs is already in the block above).
    let mut parts = Vec::new();
    if working > 0 {
        parts.push(format!("#[fg={TEAL}]●{working}"));
    }
    if idle > 0 {
        parts.push(format!("#[fg={PEACH}]○{idle}"));
    }
    if unknown > 0 {
        parts.push(format!("#[fg={ROSE}]?{unknown}"));
    }
    if !parts.is_empty() {
        out.push(' ');
        out.push_str(&parts.join(" "));
        out.push_str("#[default]");
    }
    out
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
        assert!(s.contains("⚠ 1 NEEDS INPUT"));
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
    fn needs_input_shows_a_prominent_block_with_background() {
        let s = format_indicator(2, 0, 0, 0);
        assert!(s.contains("⚠ 2 NEEDS INPUT"));
        assert!(
            s.contains(&format!("bg={ALERT_BG}")),
            "should use an attention background"
        );
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
