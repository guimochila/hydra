//! Passive attention alerts: notify the human when an agent starts needing input, so
//! they don't have to keep opening the popup to notice.
//!
//! Fired from the hook (see `hook.rs`) only on the *transition* into NEEDS_INPUT.
//! Best-effort and fire-and-forget: firing is gated by the caller via
//! `[alerts].enabled` (or `HYDRA_ALERTS=0`), and the hook must stay cheap
//! (see `CLAUDE.md`: "Keep hook.rs cheap"), so `notify` never blocks it.
//!
//! The delivery mechanism is per-OS:
//!   - macOS: shell out to `osascript` (an Apple-signed binary whose `display
//!     notification` is entitled to post to Notification Center). Spawning it is itself
//!     the detach, so there's no extra indirection. We can't use `notify-rust` here:
//!     its macOS backend (`mac-notification-sys`) is built on the deprecated
//!     `NSUserNotification` API, which silently no-ops on modern macOS for a
//!     non-bundled CLI binary (the call returns `Ok` but nothing is ever displayed).
//!   - Linux/Windows: `notify-rust`, an in-process library whose `.show()` *blocks*
//!     (D-Bus / WinRT round-trip). To keep the hook fast we don't call it inline —
//!     `spawn_notify` launches `hydra notify <title> <body>` as a detached child that
//!     calls `show` and exits, so the hook returns instantly.

use std::process::{Command, Stdio};

/// The notification title / brand mark. The 🐍 is the universal, asset-free brand
/// mark that renders on every platform; the app name is set separately (`show`).
const TITLE: &str = "🐍 Hydra";

/// Announce that the agent in `cwd` is waiting for input, with the notification's
/// reason when available. Fire-and-forget: spawns a detached `hydra notify` child so
/// the caller (the hook) never blocks. Whether alerts fire at all is decided by the
/// caller (config-gated).
pub fn needs_input(cwd: &str, message: Option<&str>) {
    notify(TITLE, &alert_body(cwd, message));
}

/// Deliver one desktop notification, fire-and-forget. Per-OS mechanism (see module
/// docs): macOS spawns `osascript` directly; elsewhere we spawn the detached
/// `hydra notify` child so `notify-rust`'s blocking call never slows the hook.
#[cfg(target_os = "macos")]
fn notify(title: &str, body: &str) {
    let _ = Command::new("osascript")
        .args(osascript_args(title, body))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
}

#[cfg(not(target_os = "macos"))]
fn notify(title: &str, body: &str) {
    spawn_notify(title, body);
}

/// The `osascript` argv for `display notification`. `title`/`body` ride in as
/// `on run argv` items rather than being interpolated into the AppleScript source, so
/// they need no escaping and survive quotes/backslashes/newlines intact (unlike a
/// quoted string literal). Pure so it can be unit-tested.
#[cfg(target_os = "macos")]
fn osascript_args<'a>(title: &'a str, body: &'a str) -> [&'a str; 9] {
    [
        "-e",
        "on run argv",
        "-e",
        "display notification (item 1 of argv) with title (item 2 of argv)",
        "-e",
        "end run",
        "--",
        body,  // item 1 of argv
        title, // item 2 of argv
    ]
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
/// the fire-and-forget contract. Non-macOS only — macOS spawns `osascript` directly
/// (see `notify`), so it never routes through the `hydra notify` / `notify-rust` path.
#[cfg(not(target_os = "macos"))]
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

    // The escaping-free contract: title/body ride in as `on run argv` items and are
    // never interpolated into the AppleScript source, so adversarial text (quotes,
    // backslashes, newlines) reaches Notification Center intact rather than breaking or
    // being stripped from the script literal.
    #[cfg(target_os = "macos")]
    #[test]
    fn osascript_passes_text_as_argv_not_interpolated() {
        let body = "wt-a: run `ls` with \"quotes\" \\ and\nnewline";
        let title = "🐍 Hydra";
        let args = osascript_args(title, body);

        // The AppleScript source references argv positionally and never embeds the text.
        assert!(args.contains(&"display notification (item 1 of argv) with title (item 2 of argv)"));
        for a in &args {
            assert!(
                !a.contains("quotes") || *a == body,
                "text leaked into the script source: {a:?}"
            );
        }

        // `--` separates osascript's own flags from the argv passed to `on run`, and
        // body/title follow in that exact order (item 1 = body, item 2 = title).
        let sep = args.iter().position(|a| *a == "--").expect("-- present");
        assert_eq!(args[sep + 1], body);
        assert_eq!(args[sep + 2], title);
    }
}
