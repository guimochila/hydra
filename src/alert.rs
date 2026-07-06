//! Passive attention alerts: notify the human when an agent starts needing input, so
//! they don't have to keep opening the popup to notice.
//!
//! Fired from the hook (see `hook.rs`) only on the *transition* into NEEDS_INPUT.
//! Best-effort and fire-and-forget: we spawn the notifier detached and never wait, so
//! the hook stays fast. Firing is gated by the caller via `[alerts].enabled` (or `HYDRA_ALERTS=0`).

use std::process::{Command, Stdio};

/// Announce that the agent in `cwd` is waiting for input. No-op if the platform notifier
/// isn't available. Whether alerts fire at all is decided by the caller (config-gated).
pub fn needs_input(cwd: &str) {
    let label = dir_label(cwd);
    notify("🐍 Hydra", &format!("{label} needs your input"));
}

/// Basename of the worktree dir, as a short human label.
fn dir_label(cwd: &str) -> String {
    cwd.trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
        .unwrap_or(cwd)
        .to_string()
}

#[cfg(target_os = "macos")]
fn notify(title: &str, body: &str) {
    // AppleScript string literals are double-quoted; strip quotes/backslashes to keep
    // the one-liner well-formed regardless of the label.
    let safe = |s: &str| s.replace(['"', '\\'], "");
    let script = format!(
        "display notification \"{}\" with title \"{}\"",
        safe(body),
        safe(title)
    );
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(not(target_os = "macos"))]
fn notify(_title: &str, _body: &str) {
    // No notifier wired for non-macOS yet; the status-line indicator still surfaces it.
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
}
