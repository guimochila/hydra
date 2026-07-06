//! The `hydra hook <event>` subcommand.
//!
//! Installed into Claude Code's `hooks` config, this runs on every lifecycle event.
//! It is deliberately dumb and fast: read the hook JSON from stdin, read `$TMUX` /
//! `$TMUX_PANE` from the environment, map the event to a status, and atomically write
//! (or remove) the pane's state file. No tmux or git subprocess calls happen here.

use crate::state::{self, AgentState, EventOutcome, Status};
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

/// Entry point for `hydra hook <event>`. `event` may be empty, in which case the
/// event name is taken from the hook payload's `hook_event_name` field.
pub fn run(event: &str) -> std::io::Result<()> {
    let mut buf = String::new();
    // Claude Code pipes the hook payload on stdin; tolerate it being absent/empty.
    let _ = std::io::stdin().read_to_string(&mut buf);
    let payload: serde_json::Value = serde_json::from_str(&buf).unwrap_or(serde_json::Value::Null);

    let event = if event.is_empty() {
        payload
            .get("hook_event_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    } else {
        event.to_string()
    };

    let tmux = std::env::var("TMUX").unwrap_or_default();
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    // Not inside tmux → this agent isn't something Hydra can locate. Silently no-op.
    let env = match state::parse_tmux_env(&tmux, &pane) {
        Some(e) => e,
        None => return Ok(()),
    };

    match state::outcome_for_event(&event) {
        EventOutcome::Ignore => Ok(()),
        EventOutcome::Remove => state::remove_state(&env.socket, &env.pane_id),
        EventOutcome::Set(status) => {
            let prev = state::read_one(&env.socket, &env.pane_id);
            // Alert only on the transition *into* NEEDS_INPUT, not on repeats.
            let was_needs_input = prev.as_ref().map(|p| p.status) == Some(Status::NeedsInput);

            let cwd = payload
                .get("cwd")
                .and_then(|v| v.as_str())
                .map(String::from)
                .or_else(|| prev.as_ref().map(|p| p.cwd.clone()))
                .or_else(|| {
                    std::env::current_dir()
                        .ok()
                        .map(|p| p.display().to_string())
                })
                .unwrap_or_default();

            let session_id = payload
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(String::from)
                .unwrap_or_else(|| env.session_id.clone());

            // Prefer a fresh prompt as the task summary; otherwise keep the last one
            // so the row stays labelled through Stop/Notification events.
            let task_summary = payload
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|p| truncate(p.trim(), 60))
                .or_else(|| prev.and_then(|p| p.task_summary));

            if status == Status::NeedsInput && !was_needs_input {
                crate::alert::needs_input(&cwd);
            }

            let state = AgentState {
                socket: env.socket,
                session_id,
                pane_id: env.pane_id,
                cwd,
                status,
                event,
                task_summary,
                updated_at: now_secs(),
            };
            state::write_state(&state)
        }
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Truncate to at most `max` chars (char-boundary safe), adding an ellipsis.
fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_leaves_short_strings_alone() {
        assert_eq!(truncate("hi", 60), "hi");
    }

    #[test]
    fn truncate_shortens_long_strings_with_ellipsis() {
        let long = "a".repeat(100);
        let t = truncate(&long, 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.ends_with('…'));
    }

    #[test]
    fn truncate_respects_multibyte_boundaries() {
        let s = "héllo wörld ☃ agent task summary that is quite long indeed";
        let t = truncate(s, 5);
        // Must not panic and must be 5 chars incl. ellipsis.
        assert_eq!(t.chars().count(), 5);
    }
}
