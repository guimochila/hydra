//! Thin, socket-parameterized wrappers over the `tmux` CLI.
//!
//! Everything is keyed by socket path so Hydra can target whichever tmux server an
//! agent reported via `$TMUX` — including nested servers. We shell out rather than
//! link a tmux library: it's robust across tmux versions and trivial to reason about.

use std::process::Command;

/// A live tmux pane, as reported by `list-panes -a`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pane {
    pub pane_id: String,
    pub session_name: String,
    pub window_index: u32,
    pub window_name: String,
    pub cwd: String,
    /// Whether this pane's window is the session's active window.
    pub window_active: bool,
    /// Controlling tty of the pane, used to locate the outer pane that hosts a nested
    /// tmux client (that client's tty equals its host pane's tty).
    pub pane_tty: String,
}

/// Format string mirroring the fields parsed in `parse_pane_line` (tab-separated).
const PANE_FORMAT: &str = "#{pane_id}\t#{session_name}\t#{window_index}\t#{window_name}\t#{pane_current_path}\t#{window_active}\t#{pane_tty}";

/// The socket path of the tmux server Hydra itself is running under, from `$TMUX`.
pub fn current_socket() -> Option<String> {
    let tmux = std::env::var("TMUX").ok()?;
    let socket = tmux.split(',').next()?;
    if socket.is_empty() {
        None
    } else {
        Some(socket.to_string())
    }
}

/// Name of the session Hydra's client is currently attached to on `socket`.
pub fn current_session(socket: &str) -> Option<String> {
    let out = tmux_output(socket, &["display-message", "-p", "#{session_name}"])?;
    let name = out.trim();
    if name.is_empty() {
        None
    } else {
        Some(name.to_string())
    }
}

/// All live panes across every session on `socket`.
pub fn list_panes(socket: &str) -> Vec<Pane> {
    let out = match tmux_output(socket, &["list-panes", "-a", "-F", PANE_FORMAT]) {
        Some(o) => o,
        None => return Vec::new(),
    };
    out.lines().filter_map(parse_pane_line).collect()
}

/// Bring the view to an agent. `agent_socket` is the server the agent runs on.
///
/// Same-server case (the norm): just select the agent's window/pane. Nested case
/// (agent on a different tmux server than Hydra's popup): also focus the outer pane
/// that hosts the inner tmux client, so the user actually sees the inner view. The
/// host pane is found by matching the inner client's tty to an outer pane's tty; if
/// that can't be resolved we still selected the inner window (best effort).
pub fn jump_to(
    agent_socket: &str,
    session: &str,
    window_index: u32,
    pane_id: &str,
) -> std::io::Result<()> {
    let target_window = format!("{session}:{window_index}");
    run(agent_socket, &["select-window", "-t", &target_window])?;
    run(agent_socket, &["select-pane", "-t", pane_id])?;

    if let Some(host_socket) = current_socket() {
        if host_socket != agent_socket {
            focus_host_pane(&host_socket, agent_socket);
        }
    }
    Ok(())
}

/// On `host_socket`, select the pane that hosts a client of `inner_socket`. Best effort:
/// silently returns if no client/tty/pane can be matched.
fn focus_host_pane(host_socket: &str, inner_socket: &str) {
    for tty in client_ttys(inner_socket) {
        let panes = list_panes(host_socket);
        if let Some(host) = match_pane_by_tty(&panes, &tty) {
            let target = format!("{}:{}", host.session_name, host.window_index);
            let _ = run(host_socket, &["select-window", "-t", &target]);
            let _ = run(host_socket, &["select-pane", "-t", &host.pane_id]);
            return;
        }
    }
}

/// TTYs of clients attached to `socket` (each nested client occupies one host pane).
fn client_ttys(socket: &str) -> Vec<String> {
    match tmux_output(socket, &["list-clients", "-F", "#{client_tty}"]) {
        Some(out) => out
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(String::from)
            .collect(),
        None => Vec::new(),
    }
}

/// Find the pane whose controlling tty matches `tty`.
fn match_pane_by_tty<'a>(panes: &'a [Pane], tty: &str) -> Option<&'a Pane> {
    panes.iter().find(|p| p.pane_tty == tty)
}

/// Send a named tmux key (e.g. `Enter`, `Escape`) to `pane_id` on `socket`.
/// Used for quick approve (Enter = accept the highlighted default) / deny (Escape).
pub fn send_key(socket: &str, pane_id: &str, key: &str) -> std::io::Result<()> {
    run(socket, &["send-keys", "-t", pane_id, key])
}

/// Type `text` literally into `pane_id`, then press Enter to submit it. `-l --` makes
/// tmux treat the text as literal input rather than key names, and stops flag parsing
/// so a leading dash in the message can't be misread as an option.
pub fn send_text(socket: &str, pane_id: &str, text: &str) -> std::io::Result<()> {
    run(socket, &["send-keys", "-t", pane_id, "-l", "--", text])?;
    run(socket, &["send-keys", "-t", pane_id, "Enter"])
}

fn tmux_output(socket: &str, args: &[&str]) -> Option<String> {
    let out = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn run(socket: &str, args: &[&str]) -> std::io::Result<()> {
    let status = Command::new("tmux")
        .arg("-S")
        .arg(socket)
        .args(args)
        .status()?;
    if status.success() {
        Ok(())
    } else {
        Err(std::io::Error::other(format!(
            "tmux {args:?} exited with {status}"
        )))
    }
}

/// Parse one tab-separated line produced by `PANE_FORMAT`. Returns `None` on malformed
/// lines (e.g. a window index that isn't a number) rather than panicking.
fn parse_pane_line(line: &str) -> Option<Pane> {
    let mut f = line.split('\t');
    let pane_id = f.next()?.to_string();
    let session_name = f.next()?.to_string();
    let window_index = f.next()?.parse::<u32>().ok()?;
    let window_name = f.next()?.to_string();
    let cwd = f.next()?.to_string();
    let window_active = f.next()? == "1";
    let pane_tty = f.next()?.to_string();
    if pane_id.is_empty() {
        return None;
    }
    Some(Pane {
        pane_id,
        session_name,
        window_index,
        window_name,
        cwd,
        window_active,
        pane_tty,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_well_formed_pane_line() {
        let line = "%7\tcet-services\t3\tclaude\t/repo/wt-a\t1\t/dev/ttys004";
        let p = parse_pane_line(line).unwrap();
        assert_eq!(p.pane_id, "%7");
        assert_eq!(p.session_name, "cet-services");
        assert_eq!(p.window_index, 3);
        assert_eq!(p.window_name, "claude");
        assert_eq!(p.cwd, "/repo/wt-a");
        assert!(p.window_active);
        assert_eq!(p.pane_tty, "/dev/ttys004");
    }

    #[test]
    fn rejects_line_with_non_numeric_window_index() {
        assert!(parse_pane_line("%7\tsess\tNOPE\tname\t/cwd\t0\t/dev/ttys0").is_none());
    }

    #[test]
    fn rejects_truncated_line() {
        assert!(parse_pane_line("%7\tsess\t3").is_none());
    }

    #[test]
    fn matches_host_pane_by_tty() {
        let panes = vec![
            Pane {
                pane_id: "%1".into(),
                session_name: "outer".into(),
                window_index: 3,
                window_name: "[tmux]".into(),
                cwd: "/x".into(),
                window_active: true,
                pane_tty: "/dev/ttys004".into(),
            },
            Pane {
                pane_id: "%2".into(),
                session_name: "outer".into(),
                window_index: 1,
                window_name: "shell".into(),
                cwd: "/x".into(),
                window_active: false,
                pane_tty: "/dev/ttys009".into(),
            },
        ];
        assert_eq!(
            match_pane_by_tty(&panes, "/dev/ttys004").unwrap().pane_id,
            "%1"
        );
        assert!(match_pane_by_tty(&panes, "/dev/ttys999").is_none());
    }

    #[test]
    fn current_socket_parses_tmux_env_var() {
        // Exercised indirectly: the socket is the substring before the first comma.
        let tmux = "/private/tmp/tmux-501/default,1234,2";
        assert_eq!(
            tmux.split(',').next(),
            Some("/private/tmp/tmux-501/default")
        );
    }
}
