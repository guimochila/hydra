//! `hydra doctor` — diagnose a broken install in seconds.
//!
//! The classic silent failure: hooks in `~/.claude/settings.json` point at a binary
//! that moved (e.g. it lived in a cargo target dir and a rebuild relocated it), and
//! agents just stop appearing with no error anywhere. Doctor checks every link in
//! the chain — config, hooks, tmux binding, runtime dir, `$TMUX` — and prints a
//! ✓/✗/! report. Exit is non-zero when any hard check fails, so it can be scripted.

use crate::install::{settings_path, tmux_conf_path, HOOK_EVENTS, TMUX_BEGIN, TMUX_END};
use serde_json::Value;
use std::io;

/// Accumulates the report: `fail` marks a hard breakage, `warn` a suspicion,
/// `ok`/`info` are informational.
struct Report {
    fails: usize,
}

impl Report {
    fn ok(&mut self, msg: &str) {
        println!("✓ {msg}");
    }
    fn warn(&mut self, msg: &str) {
        println!("! {msg}");
    }
    fn fail(&mut self, msg: &str) {
        println!("✗ {msg}");
        self.fails += 1;
    }
}

pub fn run() -> io::Result<()> {
    let mut r = Report { fails: 0 };
    let current_exe = std::env::current_exe()
        .ok()
        .map(|p| p.display().to_string());

    check_config(&mut r);
    check_hooks(&mut r, current_exe.as_deref())?;
    check_tmux_conf(&mut r, current_exe.as_deref())?;
    check_runtime_dir(&mut r);
    check_tmux_env(&mut r);

    if r.fails == 0 {
        println!("\nhydra: all checks passed");
        Ok(())
    } else {
        Err(io::Error::other(format!("{} check(s) failed", r.fails)))
    }
}

fn check_config(r: &mut Report) {
    let (_, notice) = crate::config::load_reporting();
    match notice {
        Some(n) => r.warn(&n),
        None => match crate::config::default_config_path() {
            Some(p) if p.exists() => r.ok(&format!("config parses: {}", p.display())),
            _ => r.ok("no config file — using built-in defaults"),
        },
    }
}

fn check_hooks(r: &mut Report, current_exe: Option<&str>) -> io::Result<()> {
    let path = settings_path()?;
    if !path.exists() {
        r.fail(&format!(
            "no Claude Code settings at {} — run `hydra install`",
            path.display()
        ));
        return Ok(());
    }
    let root: Value = match std::fs::read(&path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
    {
        Some(v) => v,
        None => {
            r.fail(&format!("{} is not valid JSON", path.display()));
            return Ok(());
        }
    };

    let found = hydra_hook_events(&root);
    let missing: Vec<&str> = HOOK_EVENTS
        .iter()
        .filter(|e| !found.iter().any(|(event, _)| event == *e))
        .copied()
        .collect();
    if missing.is_empty() {
        r.ok(&format!("hooks cover all {} events", HOOK_EVENTS.len()));
    } else {
        r.fail(&format!(
            "hooks missing for: {} — run `hydra install`",
            missing.join(", ")
        ));
    }

    let mut exes: Vec<String> = found
        .iter()
        .filter_map(|(_, cmd)| exe_from_command(cmd))
        .collect();
    exes.sort();
    exes.dedup();
    for exe in &exes {
        check_exe(r, exe, "hook", current_exe);
    }
    Ok(())
}

fn check_tmux_conf(r: &mut Report, current_exe: Option<&str>) -> io::Result<()> {
    let path = tmux_conf_path()?;
    let conf = std::fs::read_to_string(&path).unwrap_or_default();
    let exes = exes_in_tmux_block(&conf);
    if exes.is_empty() {
        r.fail(&format!(
            "no hydra block in {} — run `hydra install`",
            path.display()
        ));
        return Ok(());
    }
    r.ok(&format!("tmux binding block present in {}", path.display()));
    for exe in &exes {
        check_exe(r, exe, "tmux binding", current_exe);
    }
    Ok(())
}

/// Shared exe validation: it must exist and be executable, and pointing somewhere
/// other than the running binary is worth a warning (stale install after a move).
fn check_exe(r: &mut Report, exe: &str, what: &str, current_exe: Option<&str>) {
    match std::fs::metadata(exe) {
        Err(_) => r.fail(&format!(
            "{what} points at a missing binary: {exe} — re-run `hydra install`"
        )),
        Ok(meta) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                if meta.permissions().mode() & 0o111 == 0 {
                    r.fail(&format!("{what} binary is not executable: {exe}"));
                    return;
                }
            }
            let _ = meta;
            if let Some(cur) = current_exe {
                if canon(exe) != canon(cur) {
                    r.warn(&format!(
                        "{what} uses {exe}, but doctor is running from {cur} — \
                         re-run `hydra install` from the binary you keep"
                    ));
                    return;
                }
            }
            r.ok(&format!("{what} binary: {exe}"));
        }
    }
}

fn check_runtime_dir(r: &mut Report) {
    let dir = crate::state::runtime_dir();
    let probe = dir.join(".doctor-probe");
    let result = std::fs::create_dir_all(&dir).and_then(|()| std::fs::write(&probe, b"ok"));
    match result {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            r.ok(&format!("runtime dir writable: {}", dir.display()));
        }
        Err(e) => r.fail(&format!("runtime dir {} not writable: {e}", dir.display())),
    }
}

fn check_tmux_env(r: &mut Report) {
    let tmux = std::env::var("TMUX").unwrap_or_default();
    let pane = std::env::var("TMUX_PANE").unwrap_or_default();
    match crate::state::parse_tmux_env(&tmux, &pane) {
        Some(env) => r.ok(&format!("inside tmux (socket {})", env.socket)),
        None => r.warn(
            "not inside tmux — fine for a plain shell; hooks only record agents running in tmux",
        ),
    }
}

/// Canonicalize for comparison; the input when it can't be resolved.
fn canon(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Every (event, command) pair in `settings.hooks` that is a hydra hook entry.
fn hydra_hook_events(settings: &Value) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let Some(hooks) = settings.get("hooks").and_then(Value::as_object) else {
        return out;
    };
    for (event, groups) in hooks {
        let Some(arr) = groups.as_array() else {
            continue;
        };
        for group in arr {
            if !crate::install::group_is_hydra(group) {
                continue;
            }
            let cmd = group
                .get("hooks")
                .and_then(Value::as_array)
                .and_then(|inner| {
                    inner
                        .iter()
                        .find_map(|h| h.get("command").and_then(Value::as_str))
                });
            if let Some(cmd) = cmd {
                out.push((event.clone(), cmd.to_string()));
            }
        }
    }
    out
}

/// The executable path at the front of a hook command: a `"quoted path"` (what
/// `install` writes, so spaces survive) or the first bare token.
fn exe_from_command(cmd: &str) -> Option<String> {
    let cmd = cmd.trim();
    if let Some(rest) = cmd.strip_prefix('"') {
        return rest
            .split('"')
            .next()
            .filter(|s| !s.is_empty())
            .map(str::to_string);
    }
    cmd.split_whitespace().next().map(str::to_string)
}

/// Absolute paths mentioned inside the hydra-marked block of `~/.tmux.conf`. The
/// block quotes the exe two ways (`"exe"` on the bind line, `\"exe\"` inside the
/// status-right string), so after unescaping we take both quoted segments that are
/// paths and bare path-shaped tokens. Outside the markers nothing is considered.
fn exes_in_tmux_block(conf: &str) -> Vec<String> {
    let (Some(start), Some(end)) = (conf.find(TMUX_BEGIN), conf.find(TMUX_END)) else {
        return Vec::new();
    };
    let block = conf[start..end].replace("\\\"", "\"");
    let mut out: Vec<String> = Vec::new();
    for (i, part) in block.split('"').enumerate() {
        let quoted_path = i % 2 == 1 && part.starts_with('/');
        let bare_path = part.starts_with('/') && !part.contains(char::is_whitespace);
        if (quoted_path || bare_path) && !out.iter().any(|p| p == part) {
            out.push(part.to_string());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exe_from_command_handles_quoted_and_bare_paths() {
        assert_eq!(
            exe_from_command("\"/x y/hydra\" hook Stop").as_deref(),
            Some("/x y/hydra")
        );
        assert_eq!(
            exe_from_command("/usr/local/bin/hydra hook Stop").as_deref(),
            Some("/usr/local/bin/hydra")
        );
        assert_eq!(exe_from_command(""), None);
    }

    #[test]
    fn hydra_hook_events_lists_covered_events() {
        let settings = json!({
            "hooks": {
                "Stop": [
                    { "hooks": [ { "type": "command", "command": "\"/x/hydra\" hook Stop" } ] },
                    { "hooks": [ { "type": "command", "command": "node gitnexus-hook.cjs" } ] }
                ],
                "Notification": [
                    { "hooks": [ { "type": "command", "command": "node other.js" } ] }
                ]
            }
        });
        let found = hydra_hook_events(&settings);
        // Stop is covered by a hydra command; Notification only by someone else's.
        assert_eq!(
            found,
            vec![("Stop".to_string(), "\"/x/hydra\" hook Stop".to_string())]
        );
    }

    #[test]
    fn exes_in_tmux_block_finds_paths_inside_the_marked_region_only() {
        let conf = "set -g mouse on\n\
                    bind-key q display-popup \"/elsewhere/other\"\n\
                    # >>> hydra >>>\n\
                    bind-key a display-popup -E -w 70% -h 60% \"/opt/bin/hydra\"\n\
                    set -g status-right-length 200\n\
                    set -ga status-right \" #(\\\"/opt/bin/hydra\\\" status #{socket_path} #{session_name}) \"\n\
                    # <<< hydra <<<\n";
        let exes = exes_in_tmux_block(conf);
        assert_eq!(exes, vec!["/opt/bin/hydra".to_string()]);
        // No marked block → nothing (not even lookalike lines elsewhere).
        assert!(exes_in_tmux_block("bind-key a run \"/opt/bin/hydra\"\n").is_empty());
    }
}
