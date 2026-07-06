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

    let theme = crate::config::load().theme.status;
    let indicator = format_indicator(needs, working, idle, unknown, &theme);
    if !indicator.is_empty() {
        print!("{indicator}");
    }
    Ok(())
}

/// Build the status string with tmux `#[...]` styling from the configured palette. When
/// any agent needs input the indicator leads with an attention block (alert_fg on
/// alert_bg); otherwise it's a compact `hydra ●N ○N ?N`. Empty when there are no agents.
fn format_indicator(
    needs: usize,
    working: usize,
    idle: usize,
    unknown: usize,
    theme: &crate::config::ThemeStatus,
) -> String {
    if needs + working + idle + unknown == 0 {
        return String::new();
    }

    let mut out = String::new();
    if needs > 0 {
        out.push_str(&format!(
            "#[fg={},bg={},bold] ⚠ {needs} NEEDS INPUT #[default]",
            theme.alert_fg, theme.alert_bg
        ));
    } else {
        out.push_str(&format!("#[fg={},bold]hydra#[default]", theme.label));
    }

    let mut parts = Vec::new();
    if working > 0 {
        parts.push(format!("#[fg={}]●{working}", theme.working));
    }
    if idle > 0 {
        parts.push(format!("#[fg={}]○{idle}", theme.idle));
    }
    if unknown > 0 {
        parts.push(format!("#[fg={}]?{unknown}", theme.unknown));
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
    use crate::config::ThemeStatus;

    #[test]
    fn empty_when_no_agents() {
        assert_eq!(format_indicator(0, 0, 0, 0, &ThemeStatus::default()), "");
    }

    #[test]
    fn shows_each_present_status_with_counts() {
        let s = format_indicator(1, 2, 3, 4, &ThemeStatus::default());
        assert!(s.contains("⚠ 1 NEEDS INPUT"));
        assert!(s.contains("●2"));
        assert!(s.contains("○3"));
        assert!(s.contains("?4"));
    }

    #[test]
    fn omits_zero_categories() {
        let s = format_indicator(0, 2, 0, 0, &ThemeStatus::default());
        assert!(s.contains("●2"));
        assert!(!s.contains('⚠'));
        assert!(!s.contains('○'));
        assert!(!s.contains('?'));
    }

    #[test]
    fn needs_input_shows_a_prominent_block_with_background() {
        let theme = ThemeStatus::default();
        let s = format_indicator(2, 0, 0, 0, &theme);
        assert!(s.contains("⚠ 2 NEEDS INPUT"));
        assert!(
            s.contains(&format!("bg={}", theme.alert_bg)),
            "should use the configured attention background"
        );
    }

    #[test]
    fn stale_only_session_still_shows() {
        let s = format_indicator(0, 0, 0, 2, &ThemeStatus::default());
        assert!(s.contains("?2"));
    }

    #[test]
    fn needs_input_comes_first() {
        let s = format_indicator(1, 1, 1, 1, &ThemeStatus::default());
        let warn = s.find('⚠').unwrap();
        let work = s.find('●').unwrap();
        assert!(warn < work, "needs-input should render before working");
    }

    #[test]
    fn honors_a_custom_palette() {
        let theme = ThemeStatus {
            working: "cyan".to_string(),
            ..ThemeStatus::default()
        };
        let s = format_indicator(0, 1, 0, 0, &theme);
        assert!(s.contains("fg=cyan"));
    }
}
