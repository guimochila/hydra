//! The popup TUI: a live, vim-navigated, repo-grouped list of the session's agents.
//!
//! Data comes from the background fetch worker (`fetcher.rs`), which re-reads state
//! files + `tmux list-panes` + throttled git on its own tick — the UI thread only
//! drains snapshots, handles keys (polled at `INPUT_POLL_MS`) and draws, so input
//! never blocks on a slow `git status`. Enter jumps to the selected agent's window
//! and exits so the `-E` popup closes on the agent.
//!
//! Rows are either a repo header or an agent; navigation skips headers, and selection
//! is tracked by pane id so it sticks to the same agent as the list reorders.

use crate::agent::{self, Agent};
use crate::state::Status;
use crate::tmux;
use crate::worktree::{Caches, IdleWorktree};
use std::time::{SystemTime, UNIX_EPOCH};

use ansi_to_tui::IntoText;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState};
use ratatui::{Frame, Terminal};
use std::time::Duration;

/// Entry point for `hydra` with no args. No-op with a message when outside tmux.
pub fn run() -> std::io::Result<()> {
    if tmux::current_socket().is_none() {
        eprintln!("hydra: not running inside tmux — open me from a tmux popup");
        return Ok(());
    }
    let (config, config_notice) = crate::config::load_reporting();
    let colors = TuiColors::from_config(&config.theme.tui);
    // The caches move into the background worker; the UI only receives snapshots.
    let caches = Caches::new(
        config.timings.dirty_ttl_secs,
        config.timings.worktree_list_ttl_secs,
    );
    let fetcher = crate::fetcher::spawn(
        caches,
        config.timings.refresh_ms,
        config.timings.stale_after_secs,
    );
    let mut terminal = ratatui::init();
    let mut app = App {
        show_preview: true,
        colors,
        config,
        message: config_notice,
        ..App::default()
    };
    let result = app.run(&mut terminal, &fetcher);
    ratatui::restore();
    result
}

#[derive(Debug, PartialEq, Eq)]
enum Action {
    None,
    Quit,
    /// Jump to the selected agent's window, then exit.
    Jump,
    /// Start `claude` in the selected idle worktree, then exit.
    Start,
    /// Ask the fetch worker for an immediate refetch; `invalidate` drops its caches
    /// first (after a mutation, so the change shows on the next snapshot).
    Refresh {
        invalidate: bool,
    },
}

/// A quick reply to a pending prompt.
enum Response {
    /// Accept the highlighted default (send Enter).
    Approve,
    /// Reject / cancel the prompt (send Escape).
    Deny,
    /// Select option N of a multi-option dialog (send the digit itself) — less
    /// fragile than Enter/Escape when the default isn't the answer you want.
    Pick(char),
}

/// A rendered line: a repo header, a running agent (`view` index), or an idle
/// worktree you can start an agent in (`idle_view` index).
enum Row {
    Header { label: String, count: usize },
    Agent(usize),
    Worktree(usize),
}

/// What keystrokes currently do.
#[derive(Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Normal,
    /// Editing the filter query.
    Filter,
    /// Editing a message to send to the selected agent.
    Send,
    /// Editing the name/branch for a new agent to spawn.
    Spawn,
    /// Awaiting y/N confirmation of a worktree removal.
    Confirm,
}

/// A pending worktree removal, awaiting confirmation.
#[derive(Debug)]
struct RemoveTarget {
    /// Worktree path to remove.
    path: String,
    /// Branch label, for the prompt.
    branch: String,
    /// A worktree of the same repo to run git from (never `path` itself).
    base_cwd: String,
    /// If this worktree has a running agent, its (socket, session, window) to kill first.
    agent: Option<(String, String, u32)>,
    /// Whether the worktree has uncommitted changes (removal needs `--force`).
    dirty: bool,
}

/// TUI colors resolved from config strings once at startup.
struct TuiColors {
    highlight_bg: Color,
    working: Color,
    needs_input: Color,
    idle: Color,
    unknown: Color,
    footer_key: Color,
    footer_label: Color,
    header: Color,
    branch: Color,
    dirty: Color,
    worktree_row: Color,
}

impl Default for TuiColors {
    fn default() -> Self {
        Self {
            highlight_bg: Color::Rgb(50, 50, 60),
            working: Color::Green,
            needs_input: Color::Yellow,
            idle: Color::Gray,
            unknown: Color::DarkGray,
            footer_key: Color::Green,
            footer_label: Color::Gray,
            header: Color::Blue,
            branch: Color::Cyan,
            dirty: Color::Magenta,
            worktree_row: Color::DarkGray,
        }
    }
}

impl TuiColors {
    fn from_config(t: &crate::config::ThemeTui) -> Self {
        use crate::config::parse_color;
        let d = TuiColors::default();
        Self {
            highlight_bg: parse_color(&t.highlight_bg, d.highlight_bg),
            working: parse_color(&t.working, d.working),
            needs_input: parse_color(&t.needs_input, d.needs_input),
            idle: parse_color(&t.idle, d.idle),
            unknown: parse_color(&t.unknown, d.unknown),
            footer_key: parse_color(&t.footer_key, d.footer_key),
            footer_label: parse_color(&t.footer_label, d.footer_label),
            header: parse_color(&t.header, d.header),
            branch: parse_color(&t.branch, d.branch),
            dirty: parse_color(&t.dirty, d.dirty),
            worktree_row: parse_color(&t.worktree_row, d.worktree_row),
        }
    }
}

/// How long `event::poll` waits for a key each pass. Bounds input latency and how
/// quickly a worker snapshot is picked up; redrawing at ≤20 Hz is cheap under
/// ratatui's buffer diffing.
const INPUT_POLL_MS: u64 = 50;

#[derive(Default)]
struct App {
    config: crate::config::Config,
    colors: TuiColors,
    /// Whether the preview pane is shown.
    show_preview: bool,
    /// Show agents from every session on the socket, not just the current one.
    all_sessions: bool,
    /// All agents this tick (status-sorted), before filtering.
    agents: Vec<Agent>,
    /// All idle worktrees this tick, before filtering.
    idle: Vec<IdleWorktree>,
    /// Agents passing the current filter — what `Row::Agent` indexes into.
    view: Vec<Agent>,
    /// Idle worktrees passing the current filter — what `Row::Worktree` indexes into.
    idle_view: Vec<IdleWorktree>,
    rows: Vec<Row>,
    list_state: ListState,
    pending_g: bool,
    mode: Mode,
    /// Active filter query (may be empty).
    filter: String,
    /// Buffer for the message being composed in Send mode.
    send_input: String,
    /// Buffer for the name/branch being composed in Spawn mode.
    spawn_input: String,
    /// Stable key of the selection (agent pane id, or `wt:<path>`) so it survives
    /// reordering/rebuilds.
    selected_key: Option<String>,
    /// A worktree removal awaiting y/N confirmation.
    pending_remove: Option<RemoveTarget>,
    /// Transient status line (e.g. "✓ approved win 4"); cleared on the next keypress.
    message: Option<String>,
    /// Whether the first snapshot has arrived (before that, show "loading…").
    loaded: bool,
    /// Bumped per received snapshot; part of the preview memo key.
    data_seq: u64,
    /// (pane id, data_seq) the current `preview_text` was captured for.
    preview_cache: Option<(String, u64)>,
    /// Captured screen content of the selected agent (with SGR sequences).
    preview_text: String,
}

impl App {
    fn run<B: ratatui::backend::Backend<Error = std::io::Error>>(
        &mut self,
        terminal: &mut Terminal<B>,
        fetcher: &crate::fetcher::Fetcher,
    ) -> std::io::Result<()> {
        loop {
            // Drain worker snapshots, keeping only the newest.
            let mut fresh = false;
            while let Ok(overview) = fetcher.snap_rx.try_recv() {
                self.agents = overview.agents;
                self.idle = overview.idle;
                fresh = true;
            }
            if fresh {
                self.loaded = true;
                self.data_seq += 1;
                self.rebuild_rows();
            }
            self.refresh_preview();
            terminal.draw(|f| self.draw(f))?;

            // Short poll so keys AND snapshots are both picked up promptly (the
            // worker paces the actual refetch cadence, not this loop).
            if event::poll(Duration::from_millis(INPUT_POLL_MS))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match self.handle_key(key) {
                        Action::Quit => break,
                        Action::Jump => {
                            self.jump()?;
                            break;
                        }
                        Action::Start => {
                            // Only exit on success; on failure stay open and show why.
                            if self.start_selected_worktree() {
                                break;
                            }
                        }
                        Action::Refresh { invalidate } => {
                            fetcher.request_refresh(invalidate, self.all_sessions);
                        }
                        Action::None => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Memoized capture of the selected agent's screen: one tmux call per selection
    /// change or data snapshot, none on pure-keystroke redraws.
    fn refresh_preview(&mut self) {
        if !self.show_preview {
            return;
        }
        let Some(a) = self.selected_agent() else {
            self.preview_cache = None;
            self.preview_text.clear();
            return;
        };
        let key = (a.pane.pane_id.clone(), self.data_seq);
        if self.preview_cache.as_ref() == Some(&key) {
            return;
        }
        self.preview_text = tmux::capture_pane(&a.state.socket, &a.pane.pane_id);
        self.preview_cache = Some(key);
    }

    /// Rebuild the filtered views and the header/agent/worktree rows, grouped by repo
    /// (agents first, then idle worktrees), then restore the selection by key.
    fn rebuild_rows(&mut self) {
        self.view = self
            .agents
            .iter()
            .filter(|a| agent::matches_filter(a, &self.filter))
            .cloned()
            .collect();
        self.idle_view = self
            .idle
            .iter()
            .filter(|w| agent::worktree_matches_filter(w, &self.filter))
            .cloned()
            .collect();

        self.rows = self.build_grouped_rows();
        self.restore_selection(self.selected_key.clone());
    }

    /// Group agents and idle worktrees under repo headers. Repo order: repos holding
    /// agents first (in the status-sorted order agents appear), then repos with only
    /// idle worktrees. Agents with no worktree fall under a "no worktree" group.
    fn build_grouped_rows(&self) -> Vec<Row> {
        // Ordered list of (repo_key, label). None repo_key = the "no worktree" group.
        let mut order: Vec<(Option<String>, String)> = Vec::new();
        let mut seen: std::collections::HashSet<Option<String>> = std::collections::HashSet::new();
        for a in &self.view {
            let key = a.worktree.as_ref().map(|w| w.repo_key.clone());
            let label = a
                .worktree
                .as_ref()
                .map(|w| w.repo_name.clone())
                .unwrap_or_else(|| "no worktree".into());
            if seen.insert(key.clone()) {
                order.push((key, label));
            }
        }
        for w in &self.idle_view {
            let key = Some(w.repo_key.clone());
            if seen.insert(key.clone()) {
                order.push((key, w.repo_name.clone()));
            }
        }

        let mut rows = Vec::new();
        for (key, label) in order {
            let agent_idxs: Vec<usize> = self
                .view
                .iter()
                .enumerate()
                .filter(|(_, a)| a.worktree.as_ref().map(|w| w.repo_key.clone()) == key)
                .map(|(i, _)| i)
                .collect();
            let wt_idxs: Vec<usize> = self
                .idle_view
                .iter()
                .enumerate()
                .filter(|(_, w)| Some(w.repo_key.clone()) == key)
                .map(|(i, _)| i)
                .collect();
            rows.push(Row::Header {
                label,
                count: agent_idxs.len() + wt_idxs.len(),
            });
            rows.extend(agent_idxs.into_iter().map(Row::Agent));
            rows.extend(wt_idxs.into_iter().map(Row::Worktree));
        }
        rows
    }

    /// Point the selection at the row whose key matches, else the first selectable row.
    fn restore_selection(&mut self, key: Option<String>) {
        let target = key.and_then(|k| self.rows.iter().position(|r| self.row_key(r) == Some(&k)));
        let row = target.or_else(|| self.first_selectable_row());
        self.list_state.select(row);
        let new_key = row
            .and_then(|r| self.rows.get(r))
            .and_then(|row| self.row_key(row))
            .cloned();
        self.selected_key = new_key;
    }

    /// Stable key for a row (agent pane id, or `wt:<path>`); `None` for headers.
    fn row_key<'a>(&'a self, row: &Row) -> Option<&'a String> {
        match row {
            Row::Agent(i) => self.view.get(*i).map(|a| &a.pane.pane_id),
            Row::Worktree(i) => self.idle_view.get(*i).map(|w| &w.path),
            Row::Header { .. } => None,
        }
    }

    fn is_selectable(row: &Row) -> bool {
        matches!(row, Row::Agent(_) | Row::Worktree(_))
    }

    fn first_selectable_row(&self) -> Option<usize> {
        self.rows.iter().position(Self::is_selectable)
    }

    fn last_selectable_row(&self) -> Option<usize> {
        self.rows.iter().rposition(Self::is_selectable)
    }

    fn selected_agent(&self) -> Option<&Agent> {
        match self.list_state.selected().and_then(|r| self.rows.get(r)) {
            Some(Row::Agent(i)) => self.view.get(*i),
            _ => None,
        }
    }

    fn selected_worktree(&self) -> Option<&IdleWorktree> {
        match self.list_state.selected().and_then(|r| self.rows.get(r)) {
            Some(Row::Worktree(i)) => self.idle_view.get(*i),
            _ => None,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> Action {
        match self.mode {
            Mode::Filter => {
                self.handle_filter_key(key);
                Action::None
            }
            Mode::Send => {
                self.handle_send_key(key);
                Action::None
            }
            Mode::Spawn => {
                self.handle_spawn_key(key);
                Action::None
            }
            Mode::Confirm => self.handle_confirm_key(key.code),
            Mode::Normal => self.handle_normal_key(key.code),
        }
    }

    /// Readline-style editing shared by the three text-input modes. Returns whether
    /// the key was a handled (or swallowed) Ctrl chord.
    fn handle_edit_chord(key: KeyEvent, buf: &mut String) -> bool {
        if !key.modifiers.contains(KeyModifiers::CONTROL) {
            return false;
        }
        match key.code {
            KeyCode::Char('u') => buf.clear(),
            KeyCode::Char('w') => delete_last_word(buf),
            _ => {} // swallow other Ctrl chords rather than typing them literally
        }
        true
    }

    fn handle_normal_key(&mut self, code: KeyCode) -> Action {
        self.message = None;
        let g_was_pending = self.pending_g;
        self.pending_g = false;
        match code {
            KeyCode::Char('q') => return Action::Quit,
            // Esc clears an active filter first; only quits when there's nothing to clear.
            KeyCode::Esc => {
                if self.filter.is_empty() {
                    return Action::Quit;
                }
                self.filter.clear();
                self.rebuild_rows();
            }
            KeyCode::Char('/') => self.mode = Mode::Filter,
            KeyCode::Char('j') | KeyCode::Down => self.move_by(1),
            KeyCode::Char('k') | KeyCode::Up => self.move_by(-1),
            KeyCode::Char('G') => self.set_selected_row(self.last_selectable_row()),
            KeyCode::Char('g') => {
                if g_was_pending {
                    self.set_selected_row(self.first_selectable_row());
                } else {
                    self.pending_g = true;
                }
            }
            KeyCode::Char('r') => return Action::Refresh { invalidate: false },
            KeyCode::Char('p') => self.show_preview = !self.show_preview,
            KeyCode::Char('s') => {
                // The stale scope stays visible until the worker's next snapshot
                // (near-immediate: the refresh request wakes it).
                self.all_sessions = !self.all_sessions;
                return Action::Refresh { invalidate: false };
            }
            KeyCode::Char('n') => {
                self.spawn_input.clear();
                self.mode = Mode::Spawn;
            }
            KeyCode::Tab => self.select_next_needs_input(),
            KeyCode::Char('x') => self.begin_remove(),
            // Interaction (Phase 3): approve/deny a pending prompt, or compose a message.
            KeyCode::Char('a') => self.respond(Response::Approve),
            KeyCode::Char('d') => self.respond(Response::Deny),
            KeyCode::Char(c @ '1'..='3') => self.respond(Response::Pick(c)),
            KeyCode::Char('i') => {
                if self.selected_agent().is_some() {
                    self.send_input.clear();
                    self.mode = Mode::Send;
                }
            }
            KeyCode::Enter if self.selected_agent().is_some() => return Action::Jump,
            KeyCode::Enter if self.selected_worktree().is_some() => return Action::Start,
            _ => {}
        }
        Action::None
    }

    fn handle_filter_key(&mut self, key: KeyEvent) {
        if Self::handle_edit_chord(key, &mut self.filter) {
            self.rebuild_rows();
            return;
        }
        match key.code {
            // Esc abandons the filter; Enter commits it and returns to normal mode.
            KeyCode::Esc => {
                self.filter.clear();
                self.mode = Mode::Normal;
                self.rebuild_rows();
            }
            KeyCode::Enter => self.mode = Mode::Normal,
            KeyCode::Backspace => {
                self.filter.pop();
                self.rebuild_rows();
            }
            KeyCode::Down => self.move_by(1),
            KeyCode::Up => self.move_by(-1),
            KeyCode::Char(c) => {
                self.filter.push(c);
                self.rebuild_rows();
            }
            _ => {}
        }
    }

    fn handle_send_key(&mut self, key: KeyEvent) {
        if Self::handle_edit_chord(key, &mut self.send_input) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.send_input.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let text = std::mem::take(&mut self.send_input);
                self.mode = Mode::Normal;
                if !text.is_empty() {
                    self.send_message(&text);
                }
            }
            KeyCode::Backspace => {
                self.send_input.pop();
            }
            KeyCode::Char(c) => self.send_input.push(c),
            _ => {}
        }
    }

    fn handle_spawn_key(&mut self, key: KeyEvent) {
        if Self::handle_edit_chord(key, &mut self.spawn_input) {
            return;
        }
        match key.code {
            KeyCode::Esc => {
                self.spawn_input.clear();
                self.mode = Mode::Normal;
            }
            KeyCode::Enter => {
                let name = std::mem::take(&mut self.spawn_input);
                self.mode = Mode::Normal;
                let name = name.trim();
                if !name.is_empty() {
                    self.spawn_agent(name);
                }
            }
            KeyCode::Backspace => {
                self.spawn_input.pop();
            }
            KeyCode::Char(c) => self.spawn_input.push(c),
            _ => {}
        }
    }

    /// Create a worktree + tmux window running `claude` for a new agent. Uses an
    /// existing agent (selected, else first) to infer the repo, socket and session,
    /// falling back to the popup's own cwd when no agent exists yet — so the first
    /// agent of a session can be spawned from Hydra too.
    fn spawn_agent(&mut self, name: &str) {
        let Some((socket, session, cwd)) = self.spawn_context() else {
            self.message = Some("spawn needs an agent, or open the popup from a git repo".into());
            return;
        };
        let path = worktree_root(&self.config).join(sanitize(name));
        let path_str = path.display().to_string();
        let base = crate::worktree::default_branch(&cwd);

        if let Err(e) = crate::worktree::create_worktree(&cwd, &path_str, name, &base) {
            self.message = Some(format!("worktree failed: {e}"));
            return;
        }
        self.message = match crate::tmux::new_window(
            &socket,
            &session,
            &sanitize(name),
            &path_str,
            &self.config.agent.command,
        ) {
            Ok(_window_id) => Some(format!("✓ spawned {name}")),
            Err(e) => Some(format!("window failed: {e}")),
        };
    }

    /// Socket/session/cwd anchoring a spawn: the selected agent, else the first
    /// agent, else the popup's own cwd when it lives inside a git repo.
    fn spawn_context(&self) -> Option<(String, String, String)> {
        if let Some(a) = self.selected_agent().or_else(|| self.view.first()) {
            return Some((
                a.state.socket.clone(),
                a.pane.session_name.clone(),
                a.pane.cwd.clone(),
            ));
        }
        let cwd = std::env::current_dir().ok()?.display().to_string();
        crate::worktree::resolve(&cwd)?; // one-shot action; uncached is fine
        let socket = tmux::current_socket()?;
        let session = tmux::current_session(&socket)?;
        Some((socket, session, cwd))
    }

    /// Approve (Enter), deny (Escape) or pick option N (digit) on the selected
    /// agent's pending prompt. Only acts when that agent is actually waiting for
    /// input, so a stray keypress can't submit an Enter to a busy or idle agent.
    fn respond(&mut self, response: Response) {
        let Some((socket, pane, window, status)) = self.selected_target() else {
            return;
        };
        if status != Status::NeedsInput {
            self.message = Some(format!("win {window} isn't waiting for input"));
            return;
        }
        // The in-memory status can be a refresh-tick stale; re-read the state file at
        // the last moment so the keystroke can't hit an agent that already moved on.
        let fresh = crate::state::read_one(&socket, &pane).map(|s| s.status);
        if fresh != Some(Status::NeedsInput) {
            self.message = Some(format!("win {window} is no longer waiting for input"));
            return;
        }
        let (key, verb) = match response {
            Response::Approve => ("Enter".to_string(), "approved".to_string()),
            Response::Deny => ("Escape".to_string(), "denied".to_string()),
            Response::Pick(c) => (c.to_string(), format!("picked {c} in")),
        };
        self.message = Some(match tmux::send_key(&socket, &pane, &key) {
            Ok(()) => format!("✓ {verb} win {window}"),
            Err(e) => format!("{verb} failed: {e}"),
        });
    }

    fn send_message(&mut self, text: &str) {
        let Some((socket, pane, window, _)) = self.selected_target() else {
            return;
        };
        self.message = Some(match tmux::send_text(&socket, &pane, text) {
            Ok(()) => format!("✓ sent to win {window}"),
            Err(e) => format!("send failed: {e}"),
        });
    }

    /// Owned copy of the selected agent's target coordinates, so callers can mutate
    /// `self` (e.g. set `message`) without holding a borrow on the agent.
    fn selected_target(&self) -> Option<(String, String, u32, Status)> {
        self.selected_agent().map(|a| {
            (
                a.state.socket.clone(),
                a.pane.pane_id.clone(),
                a.pane.window_index,
                a.effective_status,
            )
        })
    }

    /// Move selection by `delta` selectable rows (agents + worktrees, skipping headers).
    fn move_by(&mut self, delta: isize) {
        let selectable: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| Self::is_selectable(r))
            .map(|(i, _)| i)
            .collect();
        if selectable.is_empty() {
            return;
        }
        let cur_row = self.list_state.selected().unwrap_or(selectable[0]);
        let cur_pos = selectable.iter().position(|&r| r == cur_row).unwrap_or(0) as isize;
        let next_pos = (cur_pos + delta).clamp(0, selectable.len() as isize - 1) as usize;
        self.set_selected_row(Some(selectable[next_pos]));
    }

    fn set_selected_row(&mut self, row: Option<usize>) {
        self.list_state.select(row);
        self.selected_key = row
            .and_then(|r| self.rows.get(r))
            .and_then(|row| self.row_key(row))
            .cloned();
    }

    /// Cycle selection to the next agent that needs input (wrapping). No-op with a hint
    /// when none are waiting.
    fn select_next_needs_input(&mut self) {
        let waiting: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| match r {
                Row::Agent(i) => self.view[*i].effective_status == Status::NeedsInput,
                _ => false,
            })
            .map(|(i, _)| i)
            .collect();
        if waiting.is_empty() {
            self.message = Some("no agents need input".into());
            return;
        }
        let cur = self.list_state.selected().unwrap_or(0);
        let next = waiting
            .iter()
            .find(|&&r| r > cur)
            .copied()
            .unwrap_or(waiting[0]);
        self.set_selected_row(Some(next));
    }

    /// Start `claude` in the selected idle worktree: open a new window (named after the
    /// branch) in the current session, in the worktree's directory. The new agent then
    /// appears via its own SessionStart hook. `new-window` switches to it, so exiting the
    /// popup lands the user on the new agent.
    ///
    /// Returns `true` on success (caller exits the popup). On any failure it records a
    /// footer message and returns `false` so the popup stays open with the reason
    /// visible, rather than closing silently.
    fn start_selected_worktree(&mut self) -> bool {
        let command = self.config.agent.command.clone();
        let Some(wt) = self.selected_worktree() else {
            self.message = Some("no worktree selected".into());
            return false;
        };
        let name = sanitize(&wt.branch.clone().unwrap_or_else(|| wt.path.clone()));
        let path = wt.path.clone();
        let Some(socket) = tmux::current_socket() else {
            self.message = Some("not inside tmux".into());
            return false;
        };
        let Some(session) = tmux::current_session(&socket) else {
            self.message = Some("could not resolve the current tmux session".into());
            return false;
        };
        match tmux::new_window(&socket, &session, &name, &path, &command) {
            Ok(window_id) => {
                // Switch to the new window so closing the popup lands the user on it.
                let _ = tmux::select_window_id(&socket, &window_id);
                true
            }
            Err(e) => {
                self.message = Some(format!("start failed: {e}"));
                false
            }
        }
    }

    /// Begin removing the selected worktree: build the target and enter confirm mode,
    /// or show why it can't be removed.
    fn begin_remove(&mut self) {
        match self.remove_target() {
            Ok(target) => {
                self.pending_remove = Some(target);
                self.mode = Mode::Confirm;
            }
            Err(msg) => self.message = Some(msg),
        }
    }

    /// Build a `RemoveTarget` from the current selection, or an error explaining why the
    /// selection can't be removed (main worktree, Hydra's own cwd, or not a worktree).
    fn remove_target(&self) -> Result<RemoveTarget, String> {
        let (path, branch, repo_key, agent, dirty) = if let Some(a) = self.selected_agent() {
            let wt = a
                .worktree
                .as_ref()
                .ok_or_else(|| "agent isn't in a git worktree".to_string())?;
            (
                wt.root.clone(),
                wt.branch
                    .clone()
                    .unwrap_or_else(|| a.pane.window_name.clone()),
                wt.repo_key.clone(),
                Some((
                    a.state.socket.clone(),
                    a.pane.session_name.clone(),
                    a.pane.window_index,
                )),
                a.dirty > 0,
            )
        } else if let Some(w) = self.selected_worktree() {
            (
                w.path.clone(),
                w.branch.clone().unwrap_or_else(|| "(detached)".into()),
                w.repo_key.clone(),
                None,
                crate::worktree::is_dirty(&w.path),
            )
        } else {
            return Err("nothing selected".into());
        };

        let base_cwd = repo_key
            .strip_suffix("/.git")
            .unwrap_or(&repo_key)
            .to_string();
        let canon_path = canon(&path);
        if canon_path == canon(&base_cwd) {
            return Err("can't remove the main worktree".into());
        }
        if let Ok(cwd) = std::env::current_dir() {
            if canon_path == canon(&cwd.to_string_lossy()) {
                return Err("can't remove the worktree Hydra is running in".into());
            }
        }
        Ok(RemoveTarget {
            path,
            branch,
            base_cwd,
            agent,
            dirty,
        })
    }

    fn handle_confirm_key(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => self.do_remove(),
            _ => {
                self.pending_remove = None;
                self.mode = Mode::Normal;
                Action::None
            }
        }
    }

    /// Perform the confirmed removal: kill the agent's window (if any), then
    /// `git worktree remove` (forcing when dirty). Branch is kept. On success the
    /// returned Refresh invalidates the worker's caches so the row disappears on
    /// the next snapshot (the UI stays responsive while git re-scans).
    fn do_remove(&mut self) -> Action {
        self.mode = Mode::Normal;
        let Some(target) = self.pending_remove.take() else {
            return Action::None;
        };
        if let Some((socket, session, window)) = &target.agent {
            if let Err(e) = crate::tmux::kill_window(socket, session, *window) {
                self.message = Some(format!("kill window failed: {e}"));
                return Action::None;
            }
        }
        match crate::worktree::remove_worktree(&target.base_cwd, &target.path, target.dirty) {
            Ok(()) => {
                self.message = Some(format!("✓ removed {}", target.branch));
                self.selected_key = None;
                Action::Refresh { invalidate: true }
            }
            Err(e) => {
                self.message = Some(format!("remove failed: {e}"));
                Action::None
            }
        }
    }

    fn jump(&mut self) -> std::io::Result<()> {
        if let Some(a) = self.selected_agent() {
            tmux::jump_to(
                &a.state.socket,
                &a.pane.session_name,
                a.pane.window_index,
                &a.pane.pane_id,
            )?;
        }
        Ok(())
    }

    fn draw(&mut self, frame: &mut Frame) {
        let chunks =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).split(frame.area());

        // Split the body into list + preview when the preview is on and there's room.
        let preview_on = self.show_preview && self.selected_agent().is_some();
        let (list_area, preview_area) = if preview_on {
            let cols = Layout::horizontal([Constraint::Percentage(48), Constraint::Percentage(52)])
                .split(chunks[0]);
            (cols[0], Some(cols[1]))
        } else {
            (chunks[0], None)
        };

        let block = Block::default()
            .borders(Borders::ALL)
            .title(self.title())
            .title_style(Style::default().add_modifier(Modifier::BOLD));

        if self.rows.is_empty() {
            let msg = if !self.loaded {
                "  loading…" // the popup opens before the worker's first snapshot
            } else if self.filter.is_empty() {
                "  no Claude Code agents in this session"
            } else {
                "  no agents match filter"
            };
            let empty = List::new([ListItem::new(Line::from(Span::raw(msg).dim()))]).block(block);
            frame.render_widget(empty, list_area);
        } else {
            let now = now_secs();
            let width = list_area.width;
            let items: Vec<ListItem> = self
                .rows
                .iter()
                .map(|row| match row {
                    Row::Header { label, count } => header_row(label, *count, &self.colors),
                    Row::Agent(i) => {
                        agent_row(&self.view[*i], now, &self.colors, self.all_sessions, width)
                    }
                    Row::Worktree(i) => worktree_row(&self.idle_view[*i], &self.colors),
                })
                .collect();
            let list = List::new(items).block(block).highlight_style(
                Style::default()
                    .bg(self.colors.highlight_bg)
                    .add_modifier(Modifier::BOLD),
            );
            frame.render_stateful_widget(list, list_area, &mut self.list_state);
        }

        if let Some(area) = preview_area {
            self.draw_preview(frame, area);
        }

        frame.render_widget(self.footer(), chunks[1]);
    }

    fn draw_preview(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let Some(a) = self.selected_agent() else {
            return;
        };
        let title = format!(" preview · win {} ", a.pane.window_index);
        let block = Block::default()
            .borders(Borders::ALL)
            .title(title)
            .title_style(Style::default().add_modifier(Modifier::DIM));
        // Show the tail of the agent's visible screen (the most recent output/prompt),
        // with its real colors: capture-pane -e keeps the SGR sequences and
        // ansi-to-tui turns them into styled ratatui lines. Unparseable output falls
        // back to plain text rather than an empty preview. The capture itself is
        // memoized in `refresh_preview`.
        let text = self
            .preview_text
            .into_text()
            .unwrap_or_else(|_| ratatui::text::Text::raw(self.preview_text.clone()));
        let rows = area.height.saturating_sub(2) as usize;
        let skip = text.lines.len().saturating_sub(rows);
        let tail: Vec<Line> = text.lines.into_iter().skip(skip).collect();
        let para = ratatui::widgets::Paragraph::new(tail).block(block);
        frame.render_widget(para, area);
    }

    fn title(&self) -> String {
        let session = if self.all_sessions {
            "all sessions".to_string()
        } else {
            self.agents
                .first()
                .map(|a| a.pane.session_name.clone())
                .or_else(|| tmux::current_socket().and_then(|s| tmux::current_session(&s)))
                .unwrap_or_else(|| "?".into())
        };
        let needs = self
            .agents
            .iter()
            .filter(|a| a.effective_status == Status::NeedsInput)
            .count();
        let total = self.agents.len();
        if needs > 0 {
            format!(" Hydra · {session} · {total} agents · ⚠ {needs} ")
        } else {
            format!(" Hydra · {session} · {total} agents ")
        }
    }

    fn footer(&self) -> Line<'static> {
        match self.mode {
            Mode::Filter => Line::from(vec![
                Span::styled("/", Style::default().fg(Color::Yellow)),
                Span::raw(self.filter.clone()),
                Span::styled("▊", Style::default().fg(Color::Yellow)),
                Span::raw("  ").dim(),
                Span::raw("⏎ apply  Esc clear").dim(),
            ]),
            Mode::Send => Line::from(vec![
                Span::styled("send › ", Style::default().fg(Color::Green)),
                Span::raw(self.send_input.clone()),
                Span::styled("▊", Style::default().fg(Color::Green)),
                Span::raw("  ").dim(),
                Span::raw("⏎ send  Esc cancel").dim(),
            ]),
            Mode::Spawn => Line::from(vec![
                Span::styled("spawn › ", Style::default().fg(Color::Blue)),
                Span::raw(self.spawn_input.clone()),
                Span::styled("▊", Style::default().fg(Color::Blue)),
                Span::raw("  ").dim(),
                Span::raw("⏎ create worktree+claude  Esc cancel").dim(),
            ]),
            Mode::Confirm => {
                let mut spans = vec![Span::styled(
                    format!(
                        " remove worktree {}?",
                        self.pending_remove
                            .as_ref()
                            .map(|t| t.branch.as_str())
                            .unwrap_or("")
                    ),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                )];
                if let Some(t) = &self.pending_remove {
                    if t.agent.is_some() {
                        spans.push(Span::raw(" kills its agent").dim());
                    }
                    if t.dirty {
                        spans.push(Span::styled(
                            " ⚠ uncommitted changes (force)",
                            Style::default().fg(Color::Yellow),
                        ));
                    }
                }
                spans.push(Span::raw("   "));
                spans.push(Span::raw("y confirm  n cancel").dim());
                Line::from(spans)
            }
            Mode::Normal => {
                // A transient action result takes over the footer until the next key.
                if let Some(msg) = &self.message {
                    return Line::from(Span::styled(
                        format!(" {msg}"),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                // key/label pairs, colored from the theme (footer_key / footer_label).
                let key = |k: &str| {
                    Span::styled(
                        k.to_string(),
                        Style::default()
                            .fg(self.colors.footer_key)
                            .add_modifier(Modifier::BOLD),
                    )
                };
                let label = |l: &str| {
                    Span::styled(l.to_string(), Style::default().fg(self.colors.footer_label))
                };
                let pairs = [
                    ("j/k", "move"),
                    ("⏎", "start/jump"),
                    ("a", "✓"),
                    ("d", "✗"),
                    ("1-3", "pick"),
                    ("i", "send"),
                    ("n", "new"),
                    ("x", "remove"),
                    ("⇥", "next⚠"),
                    ("/", "filter"),
                    ("p", "preview"),
                    ("s", "sess"),
                    ("q", "quit"),
                ];
                let mut spans = vec![Span::raw(" ")];
                for (i, (k, l)) in pairs.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(key(k));
                    spans.push(Span::raw(" "));
                    spans.push(label(l));
                }
                if !self.filter.is_empty() {
                    spans.push(Span::raw("   "));
                    spans.push(Span::styled(
                        format!("filter: {}", self.filter),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                Line::from(spans)
            }
        }
    }
}

fn header_row(label: &str, count: usize, colors: &TuiColors) -> ListItem<'static> {
    let line = Line::from(vec![
        Span::styled(
            format!("▸ {label}"),
            Style::default()
                .fg(colors.header)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" ({count})")).dim(),
    ]);
    ListItem::new(line)
}

/// An idle worktree with no agent: dimmed, with a "start ⏎" affordance.
fn worktree_row(w: &IdleWorktree, colors: &TuiColors) -> ListItem<'static> {
    let branch = w.branch.clone().unwrap_or_else(|| "(detached)".into());
    let line = Line::from(vec![
        Span::styled("  ○", Style::default().fg(colors.worktree_row)),
        Span::raw("  —   —      "),
        Span::styled(
            format!("{:<24}", agent::truncate(&branch, 23)),
            Style::default().fg(colors.branch).dim(),
        ),
        Span::styled("start ⏎", Style::default().fg(colors.worktree_row)),
    ]);
    ListItem::new(line)
}

fn agent_row(
    a: &Agent,
    now: u64,
    colors: &TuiColors,
    show_session: bool,
    width: u16,
) -> ListItem<'static> {
    let (color, glyph) = match a.effective_status {
        Status::NeedsInput => (colors.needs_input, a.effective_status.glyph()),
        Status::Working => (colors.working, a.effective_status.glyph()),
        Status::Idle => (colors.idle, a.effective_status.glyph()),
        Status::Unknown => (colors.unknown, a.effective_status.glyph()),
    };

    let branch = a
        .worktree
        .as_ref()
        .and_then(|w| w.branch.clone())
        .unwrap_or_else(|| a.pane.window_name.clone());

    let age = agent::format_age(now.saturating_sub(a.state.updated_at));
    // In all-sessions view each row says which session it lives in.
    let place = if show_session {
        format!("{}:{}  ", a.pane.session_name, a.pane.window_index)
    } else {
        format!("win {:>2}  ", a.pane.window_index)
    };
    let dirty = (a.dirty > 0).then(|| format!("Δ{} ", a.dirty));

    // Truncate the variable cells to the row's actual width: the branch to its
    // 24-char cell, the detail to whatever is left (long text otherwise overflows
    // off-screen instead of wrapping — List items are single lines).
    let branch_cell = format!("{:<24}", agent::truncate(&branch, 23));
    let fixed = 2 + 1 + 5 // indent + glyph + " age "
        + place.chars().count()
        + branch_cell.chars().count()
        + dirty.as_deref().map_or(0, |d| d.chars().count())
        + 2; // block borders
    let budget = (width as usize).saturating_sub(fixed);
    let detail = agent::truncate(&agent::detail_text(a).unwrap_or_default(), budget);
    // The attention message ("needs permission to run Bash") is the row's most
    // actionable text — color it like the status instead of dimming it away.
    let detail_span = if a.effective_status == Status::NeedsInput && a.state.attention.is_some() {
        Span::styled(detail, Style::default().fg(colors.needs_input)).dim()
    } else {
        Span::raw(detail).dim()
    };

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            glyph.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {age:>3} "), Style::default().fg(color)),
        Span::raw(place),
        Span::styled(branch_cell, Style::default().fg(colors.branch)),
    ];
    if let Some(d) = dirty {
        spans.push(Span::styled(d, Style::default().fg(colors.dirty)));
    }
    spans.push(detail_span);
    ListItem::new(Line::from(spans))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Directory new worktrees are created under, from config (`[agent].worktree_root`;
/// `HYDRA_WORKTREE_ROOT` is already folded in at load). A leading `~` is expanded.
fn worktree_root(config: &crate::config::Config) -> std::path::PathBuf {
    expand_tilde(&config.agent.worktree_root)
}

/// Expand a leading `~` / `~/` to the home directory. Other paths pass through.
fn expand_tilde(path: &str) -> std::path::PathBuf {
    if path == "~" {
        return dirs::home_dir().unwrap_or_default();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return dirs::home_dir().unwrap_or_default().join(rest);
    }
    std::path::PathBuf::from(path)
}

/// Canonicalize a path for comparison (resolves `..`/symlinks); input on failure.
fn canon(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Delete the trailing word plus any whitespace after it (readline Ctrl-W).
fn delete_last_word(s: &mut String) {
    while s.chars().last().is_some_and(char::is_whitespace) {
        s.pop();
    }
    while s.chars().last().is_some_and(|c| !c.is_whitespace()) {
        s.pop();
    }
}

/// Make a name safe as a single path segment (slashes/whitespace → `-`). The branch
/// keeps the original name; only the worktree directory leaf is sanitized.
fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c == '/' || c.is_whitespace() {
                '-'
            } else {
                c
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::sanitize;
    use super::*;
    use crate::state::AgentState;
    use crate::tmux::Pane;
    use crate::worktree::WorktreeInfo;
    use ratatui::backend::TestBackend;

    /// A displayable agent with a synthetic pane/state, optionally in a repo.
    /// `repo` = (repo_key, repo_name, branch, worktree_root).
    fn agent(
        pane_id: &str,
        status: Status,
        window: u32,
        repo: Option<(&str, &str, &str, &str)>,
    ) -> Agent {
        Agent {
            state: AgentState {
                socket: "/sock".into(),
                session_id: "1".into(),
                pane_id: pane_id.into(),
                cwd: "/repo".into(),
                status,
                event: "x".into(),
                task_summary: None,
                attention: None,
                updated_at: 100,
            },
            pane: Pane {
                pane_id: pane_id.into(),
                session_name: "proj".into(),
                window_index: window,
                window_name: "claude".into(),
                cwd: "/repo".into(),
                window_active: false,
                pane_tty: "/dev/ttys000".into(),
            },
            effective_status: status,
            worktree: repo.map(|(key, name, branch, root)| WorktreeInfo {
                root: root.into(),
                repo_key: key.into(),
                repo_name: name.into(),
                branch: Some(branch.into()),
            }),
            dirty: 0,
        }
    }

    fn idle_wt(path: &str, branch: &str, repo_key: &str, repo_name: &str) -> IdleWorktree {
        IdleWorktree {
            path: path.into(),
            branch: Some(branch.into()),
            repo_key: repo_key.into(),
            repo_name: repo_name.into(),
        }
    }

    /// An App with the given data, rows built and selection restored — the state the
    /// TUI is in right after a fetch tick.
    fn app_with(agents: Vec<Agent>, idle: Vec<IdleWorktree>) -> App {
        let mut app = App {
            agents,
            idle,
            ..App::default()
        };
        app.rebuild_rows();
        app
    }

    /// Press a plain (unmodified) key, as the run loop would deliver it.
    fn press(app: &mut App, code: KeyCode) -> Action {
        app.handle_key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    /// Pane id (or `wt:` path) of the currently selected row, for assertions.
    fn selected_key_of(app: &App) -> Option<String> {
        app.list_state
            .selected()
            .and_then(|r| app.rows.get(r))
            .and_then(|row| app.row_key(row))
            .cloned()
    }

    #[test]
    fn groups_agents_under_repo_headers_with_idle_worktrees_after() {
        let app = app_with(
            vec![
                agent(
                    "%1",
                    Status::Working,
                    1,
                    Some(("/a/.git", "alpha", "f1", "/a")),
                ),
                agent(
                    "%2",
                    Status::Idle,
                    2,
                    Some(("/a/.git", "alpha", "f2", "/a2")),
                ),
                agent("%3", Status::Idle, 3, None), // no worktree
            ],
            vec![
                idle_wt("/b/wt", "feat-x", "/b/.git", "beta"), // idle-only repo
                idle_wt("/a/wt", "feat-y", "/a/.git", "alpha"),
            ],
        );
        let labels: Vec<(String, usize)> = app
            .rows
            .iter()
            .filter_map(|r| match r {
                Row::Header { label, count } => Some((label.clone(), *count)),
                _ => None,
            })
            .collect();
        // Agent repos first (in agent order), idle-only repos after.
        assert_eq!(
            labels,
            vec![
                ("alpha".to_string(), 3), // 2 agents + 1 idle worktree
                ("no worktree".to_string(), 1),
                ("beta".to_string(), 1),
            ]
        );
        // Within a group: agent rows come before worktree rows.
        let alpha_rows: Vec<&Row> = app.rows[1..4].iter().collect();
        assert!(matches!(alpha_rows[0], Row::Agent(_)));
        assert!(matches!(alpha_rows[1], Row::Agent(_)));
        assert!(matches!(alpha_rows[2], Row::Worktree(_)));
    }

    #[test]
    fn selection_sticks_to_pane_id_across_reorder() {
        let mut app = app_with(
            vec![
                agent(
                    "%1",
                    Status::Idle,
                    1,
                    Some(("/a/.git", "alpha", "f1", "/a1")),
                ),
                agent(
                    "%2",
                    Status::Idle,
                    2,
                    Some(("/a/.git", "alpha", "f2", "/a2")),
                ),
            ],
            vec![],
        );
        app.selected_key = Some("%2".into());
        app.rebuild_rows();
        assert_eq!(selected_key_of(&app), Some("%2".to_string()));

        // The list reorders (e.g. %2's status now sorts it first): selection follows.
        app.agents.swap(0, 1);
        app.rebuild_rows();
        assert_eq!(selected_key_of(&app), Some("%2".to_string()));
    }

    #[test]
    fn selection_falls_back_to_first_selectable_when_key_vanishes() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        app.selected_key = Some("%gone".into());
        app.rebuild_rows();
        assert_eq!(selected_key_of(&app), Some("%1".to_string()));
    }

    #[test]
    fn movement_skips_headers_and_clamps() {
        let mut app = app_with(
            vec![
                agent(
                    "%1",
                    Status::Idle,
                    1,
                    Some(("/a/.git", "alpha", "f1", "/a1")),
                ),
                agent(
                    "%2",
                    Status::Idle,
                    2,
                    Some(("/b/.git", "beta", "f2", "/b1")),
                ),
            ],
            vec![],
        );
        // Layout: header(alpha), %1, header(beta), %2.
        assert_eq!(selected_key_of(&app), Some("%1".to_string()));
        app.move_by(1); // must skip the beta header
        assert_eq!(selected_key_of(&app), Some("%2".to_string()));
        app.move_by(1); // clamped at the end
        assert_eq!(selected_key_of(&app), Some("%2".to_string()));
        app.move_by(-5); // clamped at the start
        assert_eq!(selected_key_of(&app), Some("%1".to_string()));
    }

    #[test]
    fn gg_and_shift_g_jump_to_first_and_last() {
        let mut app = app_with(
            vec![
                agent(
                    "%1",
                    Status::Idle,
                    1,
                    Some(("/a/.git", "alpha", "f1", "/a1")),
                ),
                agent(
                    "%2",
                    Status::Idle,
                    2,
                    Some(("/a/.git", "alpha", "f2", "/a2")),
                ),
                agent(
                    "%3",
                    Status::Idle,
                    3,
                    Some(("/a/.git", "alpha", "f3", "/a3")),
                ),
            ],
            vec![],
        );
        press(&mut app, KeyCode::Char('G'));
        assert_eq!(selected_key_of(&app), Some("%3".to_string()));
        press(&mut app, KeyCode::Char('g'));
        press(&mut app, KeyCode::Char('g'));
        assert_eq!(selected_key_of(&app), Some("%1".to_string()));
    }

    #[test]
    fn tab_cycles_needs_input_and_wraps() {
        let mut app = app_with(
            vec![
                agent(
                    "%1",
                    Status::NeedsInput,
                    1,
                    Some(("/a/.git", "alpha", "f1", "/a1")),
                ),
                agent(
                    "%2",
                    Status::Idle,
                    2,
                    Some(("/a/.git", "alpha", "f2", "/a2")),
                ),
                agent(
                    "%3",
                    Status::NeedsInput,
                    3,
                    Some(("/a/.git", "alpha", "f3", "/a3")),
                ),
            ],
            vec![],
        );
        assert_eq!(selected_key_of(&app), Some("%1".to_string()));
        press(&mut app, KeyCode::Tab);
        assert_eq!(selected_key_of(&app), Some("%3".to_string()));
        press(&mut app, KeyCode::Tab); // wraps past %3 back to %1
        assert_eq!(selected_key_of(&app), Some("%1".to_string()));
    }

    #[test]
    fn tab_with_no_waiting_agents_shows_a_hint() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        press(&mut app, KeyCode::Tab);
        assert_eq!(app.message.as_deref(), Some("no agents need input"));
    }

    #[test]
    fn remove_target_rejects_the_main_worktree() {
        // Worktree root == the repo's main dir (repo_key minus /.git).
        let app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/repo/main/.git", "main-repo", "main", "/repo/main")),
            )],
            vec![],
        );
        assert_eq!(
            app.remove_target().unwrap_err(),
            "can't remove the main worktree"
        );
    }

    #[test]
    fn remove_target_rejects_hydras_own_cwd() {
        let cwd = std::env::current_dir().unwrap().display().to_string();
        let app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/elsewhere/.git", "other", "b", cwd.as_str())),
            )],
            vec![],
        );
        assert_eq!(
            app.remove_target().unwrap_err(),
            "can't remove the worktree Hydra is running in"
        );
    }

    #[test]
    fn remove_target_accepts_a_linked_worktree_and_enters_confirm() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/repo/main/.git", "main-repo", "feat", "/wt/feat")),
            )],
            vec![],
        );
        let target = app.remove_target().expect("linked worktree is removable");
        assert_eq!(target.path, "/wt/feat");
        assert_eq!(target.base_cwd, "/repo/main");
        assert!(target.agent.is_some(), "running agent must be killed first");

        press(&mut app, KeyCode::Char('x'));
        assert!(app.mode == Mode::Confirm && app.pending_remove.is_some());
        // Anything but y/Y cancels.
        press(&mut app, KeyCode::Char('n'));
        assert!(app.mode == Mode::Normal && app.pending_remove.is_none());
    }

    #[test]
    fn keys_enter_and_leave_the_input_modes() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        press(&mut app, KeyCode::Char('/'));
        assert!(app.mode == Mode::Filter);
        press(&mut app, KeyCode::Char('x'));
        assert_eq!(app.filter, "x"); // typed into the filter, not treated as remove
        press(&mut app, KeyCode::Esc);
        assert!(app.mode == Mode::Normal && app.filter.is_empty());

        press(&mut app, KeyCode::Char('i'));
        assert!(app.mode == Mode::Send);
        press(&mut app, KeyCode::Esc);
        assert!(app.mode == Mode::Normal);

        press(&mut app, KeyCode::Char('n'));
        assert!(app.mode == Mode::Spawn);
        press(&mut app, KeyCode::Esc);
        assert!(app.mode == Mode::Normal);
    }

    #[test]
    fn render_smoke_test_shows_headers_agents_and_worktrees() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::NeedsInput,
                4,
                Some(("/a/.git", "alpha", "feat/pagination", "/a1")),
            )],
            vec![idle_wt("/a/wt", "feat-idle", "/a/.git", "alpha")],
        );
        app.show_preview = false; // keep the test hermetic (no tmux capture)

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("▸ alpha"), "repo header rendered");
        assert!(text.contains("⚠"), "needs-input glyph rendered");
        assert!(text.contains("feat/pagination"), "branch rendered");
        assert!(text.contains("feat-idle"), "idle worktree rendered");
        assert!(text.contains("start ⏎"), "start affordance rendered");
        assert!(text.contains("⚠ 1"), "title shows the needs-input count");
    }

    #[test]
    fn respond_requires_a_fresh_needs_input_state_file() {
        // The in-memory agent says NeedsInput, but no state file backs it up (the
        // agent moved on / the file is gone) — respond must refuse to send keys.
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::NeedsInput,
                4,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        press(&mut app, KeyCode::Char('a'));
        assert_eq!(
            app.message.as_deref(),
            Some("win 4 is no longer waiting for input")
        );
    }

    #[test]
    fn number_keys_are_gated_like_approve() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Working,
                4,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        press(&mut app, KeyCode::Char('2'));
        assert_eq!(
            app.message.as_deref(),
            Some("win 4 isn't waiting for input")
        );
    }

    #[test]
    fn ctrl_w_and_ctrl_u_edit_the_send_buffer() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        press(&mut app, KeyCode::Char('i'));
        for c in "fix the api tests".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        app.handle_key(KeyEvent::new(KeyCode::Char('w'), KeyModifiers::CONTROL));
        assert_eq!(app.send_input, "fix the api ");
        app.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(app.send_input, "");
    }

    #[test]
    fn delete_last_word_handles_spaces_and_unicode() {
        let mut s = "fix the tests  ".to_string();
        delete_last_word(&mut s);
        assert_eq!(s, "fix the ");
        let mut s = "héllo wörld".to_string();
        delete_last_word(&mut s);
        assert_eq!(s, "héllo ");
        let mut s = String::new();
        delete_last_word(&mut s); // must not panic on empty
        assert_eq!(s, "");
    }

    #[test]
    fn refresh_keys_return_refresh_actions_for_the_worker() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        assert_eq!(
            press(&mut app, KeyCode::Char('r')),
            Action::Refresh { invalidate: false }
        );
        // `s` flips the scope and asks the worker to refetch with it.
        assert!(!app.all_sessions);
        assert_eq!(
            press(&mut app, KeyCode::Char('s')),
            Action::Refresh { invalidate: false }
        );
        assert!(app.all_sessions);
    }

    #[test]
    fn sanitize_makes_a_safe_path_segment() {
        assert_eq!(sanitize("feat/pagination api"), "feat-pagination-api");
        assert_eq!(sanitize("simple"), "simple");
    }

    #[test]
    fn footer_keybar_colors_keys_and_labels_from_theme() {
        use super::*;
        // A distinctive, non-default palette so we know the config drove the colors.
        let app = App {
            colors: TuiColors {
                footer_key: Color::Rgb(1, 2, 3),
                footer_label: Color::Rgb(4, 5, 6),
                ..TuiColors::default()
            },
            ..App::default()
        };
        let spans = app.footer().spans;
        // The `j/k` shortcut is the first key: themed color + bold.
        let key = spans
            .iter()
            .find(|s| s.content == "j/k")
            .expect("j/k key span present");
        assert_eq!(key.style.fg, Some(Color::Rgb(1, 2, 3)));
        assert!(key.style.add_modifier.contains(Modifier::BOLD));
        // Its `move` label uses the label color (not bold).
        let label = spans
            .iter()
            .find(|s| s.content == "move")
            .expect("move label span present");
        assert_eq!(label.style.fg, Some(Color::Rgb(4, 5, 6)));
        assert!(!label.style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn expand_tilde_expands_leading_home() {
        use super::expand_tilde;
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("~/work/tree"), home.join("work/tree"));
        assert_eq!(
            expand_tilde("/abs/path"),
            std::path::PathBuf::from("/abs/path")
        );
    }
}
