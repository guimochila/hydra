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
    "PostToolUse",
    "Notification",
    "Stop",
    "SessionEnd",
];

const TMUX_BEGIN: &str = "# >>> hydra >>>";
const TMUX_END: &str = "# <<< hydra <<<";

/// A commented starter config written by `install` when none exists. Every value equals
/// the built-in default, so writing it changes nothing until the user edits it.
const STARTER_CONFIG: &str = r##"# Hydra configuration. All values below are the built-in defaults; edit and save,
# then rebuild is NOT needed for most settings (they are read at runtime). Changing the
# [popup] key/size requires re-running `hydra install` and `tmux source-file ~/.tmux.conf`.

[timings]
stale_after_secs       = 900   # a WORKING agent silent this long shows as UNKNOWN
refresh_ms             = 250   # popup refresh tick
dirty_ttl_secs         = 3     # throttle for `git status` dirty counts
worktree_list_ttl_secs = 5     # throttle for `git worktree list`

[agent]
command       = "claude"       # launched by `n` (spawn) and Enter (start in worktree)
worktree_root = "~/work/tree"  # where spawned worktrees go (HYDRA_WORKTREE_ROOT wins)
spawn_mode    = "window"       # "window": one window here; "session": dedicated session (shell + agent)

[popup]                        # tmux-side — re-run `hydra install` after changing
key    = "a"                   # prefix + this key opens the popup
width  = "70%"
height = "60%"

[theme.tui]                    # ratatui colors: a name ("green") or "#rrggbb"
highlight_bg = "#32323c"
working      = "green"
needs_input  = "yellow"
idle         = "gray"
unknown      = "darkgray"
footer_key   = "green"         # shortcut keys in the footer keybar
footer_label = "gray"          # the descriptions next to each footer key
header       = "blue"          # repo group headers
branch       = "cyan"          # branch names in agent rows
dirty        = "magenta"       # the uncommitted-change count (Δ3)
worktree_row = "darkgray"      # idle-worktree rows (glyph + "start ⏎")

[theme.status]                 # status-bar palette (tmux color names or "#rrggbb")
label    = "#b35b79"
working  = "#5e857a"
idle     = "#d9a594"
alert_fg = "#f2ecbc"
alert_bg = "#d7474b"
unknown  = "#b35b79"

[alerts]
enabled = true                 # macOS needs-input notifications (HYDRA_ALERTS=0 disables)
"##;

/// Write the starter config at `path` only if it does not already exist. Returns whether
/// a file was written. Creates parent dirs as needed. Never overwrites a user's config.
pub fn write_starter_config_at(path: &std::path::Path) -> io::Result<bool> {
    if path.exists() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, STARTER_CONFIG)?;
    Ok(true)
}

pub fn install() -> io::Result<()> {
    let exe = current_exe_string()?;
    let config = crate::config::load();
    install_hooks(&exe)?;
    install_tmux_binding(&exe, &config.popup)?;

    let wrote_config = crate::config::default_config_path()
        .map(|p| write_starter_config_at(&p))
        .transpose()?
        .unwrap_or(false);

    println!(
        "hydra: installed.\n  \
         • Claude Code hooks → {}\n  \
         • hooks/binding invoke this binary: {}\n  \
         • tmux binding: prefix + {} (popup)\n  \
         • status-line indicator appended to status-right (non-destructive)\n  \
         • config: {}\n\n\
         Reload tmux config with:  tmux source-file ~/.tmux.conf",
        settings_path()?.display(),
        exe,
        config.popup.key,
        if wrote_config {
            "wrote starter ~/.config/hydra/config.toml"
        } else {
            "existing config left untouched"
        },
    );
    // The absolute path above is baked into settings.json and .tmux.conf; a binary
    // living in a build dir moves on the next `cargo clean`/rebuild elsewhere.
    if exe.contains("/target/") {
        println!(
            "\nnote: this binary lives in a cargo target dir. If it moves, hooks break \
             silently — consider copying it to a stable location (e.g. ~/.local/bin) \
             and re-running `hydra install` from there."
        );
    }
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

fn install_tmux_binding(exe: &str, popup: &crate::config::Popup) -> io::Result<()> {
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
         bind-key {key} display-popup -E -w {width} -h {height} \"{exe}\"\n\
         set -g status-right-length 200\n\
         set -ga status-right \" #(\\\"{exe}\\\" status #{{socket_path}} #{{session_name}}) \"\n\
         {TMUX_END}\n",
        key = popup.key,
        width = popup.width,
        height = popup.height,
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

    #[test]
    fn starter_config_writes_when_absent_and_not_when_present() {
        // Use a unique temp path so parallel test runs don't collide.
        let mut path = std::env::temp_dir();
        path.push(format!("hydra-test-config-{}.toml", std::process::id()));
        let _ = std::fs::remove_file(&path);

        // Absent → writes, returns true, and the file parses back to defaults.
        let wrote = write_starter_config_at(&path).unwrap();
        assert!(wrote, "should write when absent");
        let contents = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            crate::config::Config::parse(&contents),
            crate::config::Config::default()
        );

        // Present → does not overwrite, returns false.
        std::fs::write(&path, "[agent]\ncommand = \"mine\"\n").unwrap();
        let wrote_again = write_starter_config_at(&path).unwrap();
        assert!(!wrote_again, "should not overwrite an existing config");
        assert!(std::fs::read_to_string(&path).unwrap().contains("mine"));

        let _ = std::fs::remove_file(&path);
    }
}
