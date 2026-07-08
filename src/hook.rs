//! The `hydra hook <event>` subcommand.
//!
//! Installed into Claude Code's `hooks` config, this runs on every lifecycle event.
//! It is deliberately dumb and fast: read the hook JSON from stdin, read `$TMUX` /
//! `$TMUX_PANE` from the environment, map the event to a status, and atomically write
//! (or remove) the pane's state file. No tmux or git subprocess calls happen here.

use crate::agent::truncate;
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

            // Why the agent is blocked (the Notification's message); cleared the
            // moment it goes back to Working/Idle.
            let attention = attention_for(
                status,
                payload.get("message").and_then(|v| v.as_str()),
                prev.as_ref().and_then(|p| p.attention.clone()),
            );

            // Prefer a fresh prompt as the task summary; otherwise keep the last one
            // so the row stays labelled through Stop/Notification events.
            let task_summary = payload
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|p| truncate(p.trim(), 60))
                .or_else(|| prev.and_then(|p| p.task_summary));

            if status == Status::NeedsInput
                && !was_needs_input
                && crate::config::load().alerts.enabled
            {
                crate::alert::needs_input(&cwd, attention.as_deref());
            }

            let state = AgentState {
                socket: env.socket,
                session_id,
                pane_id: env.pane_id,
                cwd,
                status,
                event,
                task_summary,
                attention,
                updated_at: now_secs(),
            };
            state::write_state(&state)
        }
    }
}

/// The attention text to persist: while NEEDS_INPUT, the (truncated) notification
/// message — kept from the previous state when a repeat Notification carries none.
/// Any other status clears it; a stale "needs permission" line must never outlive
/// the prompt it described.
fn attention_for(status: Status, message: Option<&str>, prev: Option<String>) -> Option<String> {
    match status {
        Status::NeedsInput => message
            .map(|m| truncate(m.trim(), 80))
            .filter(|m| !m.is_empty())
            .or(prev),
        _ => None,
    }
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
    fn attention_set_on_needs_input_and_cleared_on_other_statuses() {
        assert_eq!(
            attention_for(
                Status::NeedsInput,
                Some("needs permission to run Bash"),
                None
            ),
            Some("needs permission to run Bash".to_string())
        );
        // A repeat Notification without a message keeps the previous reason.
        assert_eq!(
            attention_for(Status::NeedsInput, None, Some("old reason".into())),
            Some("old reason".to_string())
        );
        // Working/Idle always clear it — a stale reason must not linger.
        assert_eq!(
            attention_for(Status::Working, Some("ignored"), Some("old".into())),
            None
        );
        assert_eq!(attention_for(Status::Idle, None, Some("old".into())), None);
    }
}
