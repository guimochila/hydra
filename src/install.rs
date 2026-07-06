//! `hydra install` / `hydra uninstall`.
//!
//! Installs two things, both reversible and idempotent:
//!  1. Claude Code hooks in `~/.claude/settings.json` that invoke `hydra hook <event>`.
//!     Our entries are identified by the `hydra hook` command substring, so we merge
//!     alongside any existing hooks (e.g. gitnexus) and can strip only our own.
//!  2. A `display-popup` keybinding in `~/.tmux.conf`, wrapped in marker comments so
//!     it can be found and removed cleanly.

use serde_json::{json, Value};
use std::io;
use std::path::PathBuf;

/// Lifecycle events we register. Chosen to cover the status machine while recovering
/// from NEEDS_INPUT once a tool actually runs (PreToolUse → WORKING).
const HOOK_EVENTS: &[&str] = &[
    "SessionStart",
    "UserPromptSubmit",
    "PreToolUse",
    "Notification",
    "Stop",
    "SessionEnd",
];

const TMUX_BEGIN: &str = "# >>> hydra >>>";
const TMUX_END: &str = "# <<< hydra <<<";
/// prefix + this key opens the popup. `a` = "agents"; unused in the user's config.
const POPUP_KEY: &str = "a";

pub fn install() -> io::Result<()> {
    let exe = current_exe_string()?;
    install_hooks(&exe)?;
    install_tmux_binding(&exe)?;
    println!(
        "hydra: installed.\n  \
         • Claude Code hooks → {}\n  \
         • tmux binding: prefix + {} (popup)\n  \
         • status-line indicator appended to status-right (non-destructive)\n\n\
         Reload tmux config with:  tmux source-file ~/.tmux.conf",
        settings_path()?.display(),
        POPUP_KEY
    );
    Ok(())
}

pub fn uninstall() -> io::Result<()> {
    uninstall_hooks()?;
    uninstall_tmux_binding()?;
    println!("hydra: removed hooks and tmux binding.");
    Ok(())
}

// ---- Claude Code hooks (settings.json) ------------------------------------------

fn install_hooks(exe: &str) -> io::Result<()> {
    let path = settings_path()?;
    let mut root = read_json(&path)?;
    backup(&path)?;

    let hooks = root
        .as_object_mut()
        .expect("settings root is an object")
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let hooks = hooks.as_object_mut().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "settings.json 'hooks' is not an object",
        )
    })?;

    for event in HOOK_EVENTS {
        let command = format!("\"{exe}\" hook {event}");
        let arr = hooks
            .entry((*event).to_string())
            .or_insert_with(|| json!([]));
        let arr = arr.as_array_mut().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "hook event is not an array")
        })?;
        // Drop any prior hydra entry for this event, then add a fresh one.
        arr.retain(|group| !group_is_hydra(group));
        arr.push(json!({
            "hooks": [ { "type": "command", "command": command } ]
        }));
    }

    write_json(&path, &root)
}

fn uninstall_hooks() -> io::Result<()> {
    let path = settings_path()?;
    if !path.exists() {
        return Ok(());
    }
    let mut root = read_json(&path)?;
    backup(&path)?;

    if let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) {
        for value in hooks.values_mut() {
            if let Some(arr) = value.as_array_mut() {
                arr.retain(|group| !group_is_hydra(group));
            }
        }
    }
    write_json(&path, &root)
}

/// True if a hook matcher-group is one we installed (its command mentions `hydra hook`).
fn group_is_hydra(group: &Value) -> bool {
    group
        .get("hooks")
        .and_then(Value::as_array)
        .map(|inner| {
            inner.iter().any(|h| {
                h.get("command")
                    .and_then(Value::as_str)
                    // Our command is `"<path>/hydra" hook <Event>`. Match on both
                    // tokens: the quoting means a single "hydra hook" substring never
                    // appears, and gitnexus-style commands contain "hook" but not
                    // "hydra", so this won't false-match other tools.
                    .map(|c| c.contains("hydra") && c.contains("hook"))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

// ---- tmux binding (~/.tmux.conf) ------------------------------------------------

fn install_tmux_binding(exe: &str) -> io::Result<()> {
    let path = tmux_conf_path()?;
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    // `set -ga status-right` APPENDS to the right of the bar (non-destructive), landing
    // near the battery/weather/time cluster. This composes cleanly on each config reload
    // because the theme re-sets status-right first, resetting the base before we
    // re-append. status-right-length is raised so it isn't truncated. The command's
    // socket/session args come from tmux format expansion (a status-line command has no
    // $TMUX).
    let block = format!(
        "{TMUX_BEGIN}\n\
         bind-key {POPUP_KEY} display-popup -E -w 70% -h 60% \"{exe}\"\n\
         set -g status-right-length 200\n\
         set -ga status-right \" #(\\\"{exe}\\\" status #{{socket_path}} #{{session_name}}) \"\n\
         {TMUX_END}\n"
    );
    let updated = replace_marked_block(&existing, &block);
    std::fs::write(&path, updated)
}

fn uninstall_tmux_binding() -> io::Result<()> {
    let path = tmux_conf_path()?;
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Ok(()),
    };
    let updated = replace_marked_block(&existing, "");
    std::fs::write(&path, updated)
}

/// Replace the region between the hydra markers (inclusive) with `block`. If no marker
/// region exists and `block` is non-empty, append it. Pure/testable.
fn replace_marked_block(existing: &str, block: &str) -> String {
    if let (Some(start), Some(end)) = (existing.find(TMUX_BEGIN), existing.find(TMUX_END)) {
        let end = end + TMUX_END.len();
        // Also consume a trailing newline after the end marker, if present.
        let tail_start = if existing[end..].starts_with('\n') {
            end + 1
        } else {
            end
        };
        let mut out = String::with_capacity(existing.len());
        out.push_str(&existing[..start]);
        out.push_str(block);
        out.push_str(&existing[tail_start..]);
        out
    } else if block.is_empty() {
        existing.to_string()
    } else {
        let mut out = existing.to_string();
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(block);
        out
    }
}

// ---- shared helpers -------------------------------------------------------------

fn current_exe_string() -> io::Result<String> {
    Ok(std::env::current_exe()?.display().to_string())
}

fn home() -> io::Result<PathBuf> {
    dirs::home_dir().ok_or_else(|| io::Error::other("cannot determine home directory"))
}

fn settings_path() -> io::Result<PathBuf> {
    Ok(home()?.join(".claude").join("settings.json"))
}

fn tmux_conf_path() -> io::Result<PathBuf> {
    Ok(home()?.join(".tmux.conf"))
}

fn read_json(path: &PathBuf) -> io::Result<Value> {
    match std::fs::read(path) {
        Ok(bytes) if !bytes.is_empty() => serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
        _ => Ok(json!({})),
    }
}

fn write_json(path: &PathBuf, value: &Value) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(value)?;
    std::fs::write(path, text)
}

fn backup(path: &PathBuf) -> io::Result<()> {
    if path.exists() {
        let bak = path.with_extension("json.hydra.bak");
        std::fs::copy(path, bak)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_block_when_absent() {
        let out =
            replace_marked_block("set -g mouse on\n", "# >>> hydra >>>\nX\n# <<< hydra <<<\n");
        assert!(out.starts_with("set -g mouse on\n"));
        assert!(out.contains("# >>> hydra >>>"));
    }

    #[test]
    fn replaces_existing_block_idempotently() {
        let first = replace_marked_block("a\n", "# >>> hydra >>>\nOLD\n# <<< hydra <<<\n");
        let second = replace_marked_block(&first, "# >>> hydra >>>\nNEW\n# <<< hydra <<<\n");
        assert!(second.contains("NEW"));
        assert!(!second.contains("OLD"));
        assert_eq!(second.matches("# >>> hydra >>>").count(), 1);
    }

    #[test]
    fn removes_block_on_empty_replacement() {
        let with = replace_marked_block("a\n", "# >>> hydra >>>\nX\n# <<< hydra <<<\n");
        let without = replace_marked_block(&with, "");
        assert!(!without.contains("hydra"));
        assert_eq!(without, "a\n");
    }

    #[test]
    fn group_is_hydra_detects_our_command() {
        let ours =
            json!({ "hooks": [ { "type": "command", "command": "\"/x/hydra\" hook Stop" } ] });
        let theirs =
            json!({ "hooks": [ { "type": "command", "command": "node gitnexus-hook.cjs" } ] });
        assert!(group_is_hydra(&ours));
        assert!(!group_is_hydra(&theirs));
    }
}
