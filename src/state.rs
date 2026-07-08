//! Runtime state shared between the `hydra hook` writers and the TUI reader.
//!
//! Each Claude Code agent self-reports its tmux location and lifecycle status by
//! writing one small JSON file per pane into the runtime directory. The hook is the
//! only writer; the TUI is the only reader. Correlation with live tmux panes happens
//! later (see `agent.rs`) — this module only owns the on-disk representation.

use serde::{Deserialize, Serialize};
use std::hash::{Hash, Hasher};
use std::io::Write;
use std::path::PathBuf;

/// Derived lifecycle status of an agent, computed from the last hook event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum Status {
    /// Busy: thinking or running a tool.
    Working,
    /// Wants the human: permission prompt or idle-waiting notification.
    NeedsInput,
    /// Finished its turn, awaiting the next prompt.
    Idle,
    /// State file is stale — the agent may have crashed.
    Unknown,
}

impl Status {
    /// Glyph shown in the list. Kept here so the state machine and UI agree.
    pub fn glyph(self) -> &'static str {
        match self {
            Status::Working => "●",
            Status::NeedsInput => "⚠",
            Status::Idle => "○",
            Status::Unknown => "?",
        }
    }
}

/// What a hook event should do to the agent's persisted state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventOutcome {
    /// Set the status to this value.
    Set(Status),
    /// Remove the agent's state file (session ended).
    Remove,
    /// Unknown event — do nothing.
    Ignore,
}

/// Map a Claude Code hook event name to the effect it has on agent state.
pub fn outcome_for_event(event: &str) -> EventOutcome {
    match event {
        // SubagentStop means a *subagent* finished — the parent agent is still
        // processing its result, so it stays WORKING (Idle here would flicker).
        "SessionStart" | "UserPromptSubmit" | "PreToolUse" | "PostToolUse" | "SubagentStop" => {
            EventOutcome::Set(Status::Working)
        }
        "Notification" => EventOutcome::Set(Status::NeedsInput),
        "Stop" => EventOutcome::Set(Status::Idle),
        "SessionEnd" => EventOutcome::Remove,
        _ => EventOutcome::Ignore,
    }
}

/// The parts of `$TMUX` / `$TMUX_PANE` that pin an agent to a tmux server + pane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmuxEnv {
    /// Socket path — the tmux server the agent runs under (handles nesting).
    pub socket: String,
    /// Numeric session id from `$TMUX` (not the session name; join yields the name).
    pub session_id: String,
    /// Pane id, e.g. `%7`.
    pub pane_id: String,
}

/// Parse `$TMUX` (`socket,pid,session`) plus `$TMUX_PANE`. Returns `None` when either
/// is missing/malformed, i.e. the agent isn't actually inside tmux.
pub fn parse_tmux_env(tmux: &str, pane: &str) -> Option<TmuxEnv> {
    let mut parts = tmux.splitn(3, ',');
    let socket = parts.next()?.to_string();
    let _pid = parts.next()?;
    let session_id = parts.next()?.to_string();
    if socket.is_empty() || pane.is_empty() {
        return None;
    }
    Some(TmuxEnv {
        socket,
        session_id,
        pane_id: pane.to_string(),
    })
}

/// One agent's persisted state. `socket` + `pane_id` are the join key against tmux.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    pub socket: String,
    pub session_id: String,
    pub pane_id: String,
    pub cwd: String,
    pub status: Status,
    pub event: String,
    #[serde(default)]
    pub task_summary: Option<String>,
    /// Why the agent needs input (the Notification's message), set only while
    /// NEEDS_INPUT; cleared on any Working/Idle transition. `default` keeps state
    /// files written by older hydra versions parseable.
    #[serde(default)]
    pub attention: Option<String>,
    /// Unix seconds of the last update.
    pub updated_at: u64,
}

/// Directory holding per-pane state files. Prefers `$XDG_RUNTIME_DIR/hydra`, falls
/// back to a per-user dir under the system temp dir (macOS has no XDG_RUNTIME_DIR).
pub fn runtime_dir() -> PathBuf {
    if let Ok(x) = std::env::var("XDG_RUNTIME_DIR") {
        if !x.is_empty() {
            return PathBuf::from(x).join("hydra");
        }
    }
    let user = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
    std::env::temp_dir().join(format!("hydra-{user}"))
}

/// Stable, filesystem-safe file name for a given (socket, pane). The socket path is
/// hashed so nested/alternate sockets don't collide and no path separators leak in.
pub fn state_file_name(socket: &str, pane_id: &str) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    socket.hash(&mut h);
    let socket_hash = h.finish();
    let pane = pane_id.trim_start_matches('%');
    format!("pane-{socket_hash:x}-{pane}.json")
}

/// Create the runtime dir if needed, owner-only (0700). The `/tmp/hydra-<user>`
/// fallback would otherwise be world-readable, and state files carry prompt text.
fn create_runtime_dir(dir: &std::path::Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(dir)
}

/// Write a state record atomically (temp file + rename) so the reader never sees a
/// half-written file. Creates the runtime dir on demand.
pub fn write_state(state: &AgentState) -> std::io::Result<()> {
    let dir = runtime_dir();
    create_runtime_dir(&dir)?;
    let final_path = dir.join(state_file_name(&state.socket, &state.pane_id));
    let tmp_path = dir.join(format!(
        ".{}.tmp",
        state_file_name(&state.socket, &state.pane_id)
    ));
    let json = serde_json::to_vec_pretty(state)?;
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(&json)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp_path, &final_path)
}

/// Remove a state file (used on `SessionEnd`). Missing file is not an error.
pub fn remove_state(socket: &str, pane_id: &str) -> std::io::Result<()> {
    let path = runtime_dir().join(state_file_name(socket, pane_id));
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e),
    }
}

/// Read a single agent's state file, if present and parseable.
pub fn read_one(socket: &str, pane_id: &str) -> Option<AgentState> {
    let path = runtime_dir().join(state_file_name(socket, pane_id));
    let bytes = std::fs::read(&path).ok()?;
    serde_json::from_slice::<AgentState>(&bytes).ok()
}

/// Read every parseable state file in the runtime dir. Unreadable/garbage files are
/// skipped rather than failing the whole read.
pub fn read_all() -> Vec<AgentState> {
    let dir = runtime_dir();
    let mut out = Vec::new();
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return out,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(state) = serde_json::from_slice::<AgentState>(&bytes) {
                out.push(state);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_mapping_covers_lifecycle() {
        assert_eq!(
            outcome_for_event("UserPromptSubmit"),
            EventOutcome::Set(Status::Working)
        );
        assert_eq!(
            outcome_for_event("PreToolUse"),
            EventOutcome::Set(Status::Working)
        );
        assert_eq!(
            outcome_for_event("PostToolUse"),
            EventOutcome::Set(Status::Working)
        );
        assert_eq!(
            outcome_for_event("Notification"),
            EventOutcome::Set(Status::NeedsInput)
        );
        assert_eq!(outcome_for_event("Stop"), EventOutcome::Set(Status::Idle));
        // A subagent stopping must NOT idle the parent agent — it's still working.
        assert_eq!(
            outcome_for_event("SubagentStop"),
            EventOutcome::Set(Status::Working)
        );
        assert_eq!(outcome_for_event("SessionEnd"), EventOutcome::Remove);
        assert_eq!(outcome_for_event("SomethingElse"), EventOutcome::Ignore);
    }

    #[test]
    fn notification_then_tool_transitions_back_to_working() {
        // A permission prompt (Notification) followed by approval (PreToolUse)
        // should leave the agent WORKING, not stuck on NEEDS_INPUT.
        assert_eq!(
            outcome_for_event("Notification"),
            EventOutcome::Set(Status::NeedsInput)
        );
        assert_eq!(
            outcome_for_event("PreToolUse"),
            EventOutcome::Set(Status::Working)
        );
    }

    #[test]
    fn parses_well_formed_tmux_env() {
        let env = parse_tmux_env("/private/tmp/tmux-501/default,1234,2", "%7").unwrap();
        assert_eq!(env.socket, "/private/tmp/tmux-501/default");
        assert_eq!(env.session_id, "2");
        assert_eq!(env.pane_id, "%7");
    }

    #[test]
    fn rejects_missing_tmux_env() {
        assert!(parse_tmux_env("", "%7").is_none());
        assert!(parse_tmux_env("/sock,1234,2", "").is_none());
        assert!(parse_tmux_env("just-a-socket", "%7").is_none());
    }

    #[test]
    fn file_name_is_path_safe_and_socket_scoped() {
        let a = state_file_name("/private/tmp/tmux-501/default", "%7");
        let b = state_file_name("/private/tmp/tmux-501/other", "%7");
        assert!(!a.contains('/'));
        assert!(a.ends_with("-7.json"));
        // Same pane id on different sockets must not collide.
        assert_ne!(a, b);
    }

    #[test]
    fn agent_state_round_trips_through_json() {
        let s = AgentState {
            socket: "/sock".into(),
            session_id: "2".into(),
            pane_id: "%7".into(),
            cwd: "/repo/wt-a".into(),
            status: Status::NeedsInput,
            event: "Notification".into(),
            task_summary: Some("refactor api".into()),
            attention: Some("permission to run Bash".into()),
            updated_at: 42,
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: AgentState = serde_json::from_str(&json).unwrap();
        assert_eq!(s, back);
        // Status serializes as a screaming-snake string.
        assert!(json.contains("\"NEEDS_INPUT\""));
    }

    #[test]
    fn state_files_without_attention_still_parse() {
        // A file written by an older hydra (no `attention` key) must deserialize.
        let old = r#"{"socket":"/sock","session_id":"2","pane_id":"%7","cwd":"/repo",
                      "status":"IDLE","event":"Stop","updated_at":42}"#;
        let s: AgentState = serde_json::from_str(old).unwrap();
        assert_eq!(s.attention, None);
        assert_eq!(s.task_summary, None);
    }
}
