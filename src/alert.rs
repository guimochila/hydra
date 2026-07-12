//! Passive attention alerts: notify the human when an agent starts needing input, so
//! they don't have to keep opening the popup to notice.
//!
//! Fired from the hook (see `hook.rs`) only on the *transition* into NEEDS_INPUT.
//! Best-effort and fire-and-forget: the actual desktop notification is shown by
//! `notify-rust`, which is an in-process (blocking) library. To keep the hook fast
//! (see `CLAUDE.md`: "Keep hook.rs cheap"), we don't call it inline — instead
//! `spawn_notify` launches `hydra notify <title> <body>` as a detached child that
//! calls `show` and exits, so the hook returns instantly. Firing is gated by the
//! caller via `[alerts].enabled` (or `HYDRA_ALERTS=0`).

use std::process::{Command, Stdio};

/// The notification title / brand mark. The 🐍 is the universal, asset-free brand
/// mark that renders on every platform; the app name is set separately (`show`).
const TITLE: &str = "🐍 Hydra";

/// Announce that the agent in `cwd` is waiting for input, with the notification's
/// reason when available. Fire-and-forget: spawns a detached `hydra notify` child so
/// the caller (the hook) never blocks. Whether alerts fire at all is decided by the
/// caller (config-gated).
pub fn needs_input(cwd: &str, message: Option<&str>) {
    spawn_notify(TITLE, &alert_body(cwd, message));
}

/// The notification body: the worktree label plus the reason, or a generic fallback.
/// Pure so it can be unit-tested; the wording is the only human-facing string.
fn alert_body(cwd: &str, message: Option<&str>) -> String {
    let label = dir_label(cwd);
    match message {
        Some(m) if !m.is_empty() => format!("{label}: {m}"),
        _ => format!("{label} needs your input"),
    }
}

/// Basename of the worktree dir, as a short human label.
fn dir_label(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

/// Spawn `hydra notify <title> <body>` detached and return immediately. Args go
/// through argv (no shell), so `title`/`body` need no escaping regardless of content.
/// Best-effort: a missing `current_exe` or spawn failure is silently ignored, matching
/// the fire-and-forget contract.
fn spawn_notify(title: &str, body: &str) {
    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let _ = Command::new(exe)
        .args(["notify", title, body])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

/// Show a desktop notification via `notify-rust` (cross-platform: zbus on Linux/BSD,
/// mac-notification-sys on macOS, WinRT on Windows). Runs in the short-lived
/// `hydra notify` process, which exits right after — we use no notification actions,
/// so there's nothing to stay alive for. Best-effort; failures are ignored.
pub fn show(title: &str, body: &str) {
    let _ = notify_rust::Notification::new()
        .appname("Hydra") // proper attribution on Linux; harmless elsewhere
        .summary(title)
        .body(body)
        .icon("dialog-information") // freedesktop themed name (Linux); ignored on macOS
        .show();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dir_label_is_the_basename() {
        assert_eq!(dir_label("/Users/me/proj/wt-a"), "wt-a");
        assert_eq!(dir_label("/Users/me/proj/wt-a/"), "wt-a");
        assert_eq!(dir_label("solo"), "solo");
    }

    #[test]
    fn alert_body_uses_reason_when_present_and_falls_back_otherwise() {
        assert_eq!(
            alert_body("/Users/me/proj/wt-a", Some("needs permission to run Bash")),
            "wt-a: needs permission to run Bash"
        );
        // Empty message is treated as no message.
        assert_eq!(
            alert_body("/Users/me/proj/wt-a", Some("")),
            "wt-a needs your input"
        );
        assert_eq!(
            alert_body("/Users/me/proj/wt-a", None),
            "wt-a needs your input"
        );
    }
}
