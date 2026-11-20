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

use crate::agent::{self, session_name, Agent};
use crate::fetcher::PreviewTarget;
use crate::state::Status;
use crate::tmux;
use crate::worktree::{BranchState, Caches, IdleWorktree};
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
    let all_sessions = starts_all_sessions(&config);
    // The caches move into the background worker; the UI only receives snapshots.
    let caches = Caches::new(
        config.timings.dirty_ttl_secs,
        config.timings.worktree_list_ttl_secs,
        config.timings.ahead_behind_ttl_secs,
    );
    let fetcher = crate::fetcher::spawn(
        caches,
        config.timings.refresh_ms,
        config.timings.stale_after_secs,
        all_sessions,
    );
    let mut terminal = ratatui::init();
    let mut app = App {
        show_preview: true,
        colors,
        all_sessions,
        message: config_notice,
        config,
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
    /// Start an agent in the selected idle worktree, then exit. `resume` picks the
    /// configured resume command (continue the last conversation) over a fresh start.
    Start {
        resume: bool,
    },
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
    /// Awaiting y/N confirmation of a worktree removal, a close, or an interrupt.
    Confirm,
}

/// `tmux::kill_window`'s shape (socket, session, window index), for the test seam.
type KillWindowFn = fn(&str, &str, u32) -> std::io::Result<()>;

/// The selected row resolved to the worktree it lives in — the part a removal (`x`) and
/// a close (`X`) share: which directory, and which tmux windows are rooted in it.
#[derive(Debug)]
struct SelectedWorktree {
    /// Worktree root path.
    path: String,
    /// Branch label (directory basename for a detached worktree), for prompts.
    label: String,
    /// The repo's main worktree dir (`repo_key` minus `/.git`).
    base_cwd: String,
    /// The socket its windows live on: the agent's own when there is one, else the
    /// popup's. `None` only outside tmux, where there is nothing to tear down.
    socket: Option<String>,
    /// A running agent's pane id: its window is killed even when its pane cwd no longer
    /// matches the worktree. A pane id, not a (session, index) — indices shift under
    /// `renumber-windows` between the key press and the confirm (see `agent::own_window`).
    agent: Option<String>,
    /// The owning repo's display name and the branch, to name the worktree's dedicated
    /// session (`session_name`).
    repo_name: String,
    branch: Option<String>,
}

/// A pending close (`X`): kill every window rooted in the worktree, keep the worktree.
#[derive(Debug)]
struct CloseTarget {
    /// Worktree path whose windows are closed (and which stays on disk).
    path: String,
    /// Branch label, for the prompt.
    label: String,
    socket: String,
    /// The running agent's pane id (re-resolved to its window at confirm time).
    agent: Option<String>,
    /// `Some(session)` closes only Hydra's own windows — that dedicated session plus the
    /// agent's window (`agent::CloseScope::Owned`); `None` closes every rooted window.
    owned_session: Option<String>,
    /// How many windows the close would kill when it was confirmed-for, for the prompt.
    windows: usize,
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
    /// The socket to tear windows down on: the agent's own when there is one, else the
    /// popup's. `None` only outside tmux, where there is nothing to tear down.
    socket: Option<String>,
    /// If this worktree has a running agent, its pane id — its window is killed even when
    /// its pane cwd no longer matches the worktree.
    agent: Option<String>,
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

/// The normal-mode footer keybar: (key, label) pairs, in display order.
const KEYBAR: &[(&str, &str)] = &[
    ("j/k", "move"),
    ("⏎", "start/jump"),
    ("c", "resume"),
    ("a", "✓"),
    ("d", "✗"),
    ("1-9", "pick"),
    ("i", "send"),
    ("n", "new"),
    ("x", "remove"),
    ("X", "close"),
    ("I", "interrupt"),
    ("⇥", "next⚠"),
    ("/", "filter"),
    ("p", "preview"),
    ("s", "scope"),
    ("q", "quit"),
];

/// Rendered width of one `key label` pair.
fn keybar_pair_width(&(k, l): &(&str, &str)) -> usize {
    k.chars().count() + 1 + l.chars().count()
}

/// Rendered width of a keybar row: 1-column indent + pairs + 2-space separators.
fn keybar_row_width(row: &[(&str, &str)]) -> usize {
    1 + row.iter().map(keybar_pair_width).sum::<usize>() + 2 * row.len().saturating_sub(1)
}

/// Greedily pack the keybar pairs into rows of at most `width` columns, so a narrow
/// popup wraps whole `key label` entries onto extra footer lines instead of clipping
/// them off the right edge. Every row keeps at least one pair (pathological widths
/// still terminate), and order is preserved.
fn pack_keybar<'a>(pairs: &'a [(&'a str, &'a str)], width: usize) -> Vec<&'a [(&'a str, &'a str)]> {
    let mut rows = Vec::new();
    let mut start = 0;
    let mut used = 1; // leading indent
    for (i, pair) in pairs.iter().enumerate() {
        let w = keybar_pair_width(pair);
        if i > start && used + 2 + w > width {
            rows.push(&pairs[start..i]);
            start = i;
            used = 1 + w;
        } else {
            used += if i == start { w } else { 2 + w };
        }
    }
    rows.push(&pairs[start..]);
    rows
}

#[derive(Default)]
struct App {
    config: crate::config::Config,
    colors: TuiColors,
    /// Whether the preview pane is shown.
    show_preview: bool,
    /// Flip the default repo-scoped view to every session on the socket.
    all_sessions: bool,
    /// Header label for the active view scope, taken from the latest snapshot
    /// (`current_overview` computes it so the UI does no git/tmux work).
    scope_label: String,
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
    /// A close (kill the worktree's windows, keep the worktree) awaiting confirmation.
    pending_close: Option<CloseTarget>,
    /// Test seam for the pane listing a close plans against; `None` = `tmux::list_panes`.
    list_panes_fn: Option<fn(&str) -> Vec<tmux::Pane>>,
    /// Test seam for the popup's socket; `None` = `tmux::current_socket`.
    current_socket_fn: Option<fn() -> Option<String>>,
    /// Test seam for the session the popup's client is attached to on a socket;
    /// `None` = `tmux::current_session`.
    client_session_fn: Option<fn(&str) -> Option<String>>,
    /// Test seam for the pane Hydra itself runs in; `None` = the `TMUX_PANE` env var.
    self_pane_fn: Option<fn() -> Option<String>>,
    /// Test seam for killing one window (socket, session, index); `None` =
    /// `tmux::kill_window`.
    kill_window_fn: Option<KillWindowFn>,
    /// An interrupt of a working agent awaiting y/N confirmation:
    /// (socket, pane_id, window_index).
    pending_interrupt: Option<(String, String, u32)>,
    /// Transient status line (e.g. "✓ approved win 4"); cleared on the next keypress.
    message: Option<String>,
    /// Whether the first snapshot has arrived (before that, show "loading…").
    loaded: bool,
    /// The latest snapshot's preview: the worker-fetched text and the target it was
    /// fetched for. Rendered only while it still matches the current selection.
    preview: Option<(PreviewTarget, String)>,
    /// The preview target last requested from the worker, to avoid re-sending an
    /// identical request every input poll.
    last_preview_req: Option<PreviewTarget>,
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
                self.scope_label = overview.scope_label;
                self.preview = overview.preview;
                fresh = true;
            }
            if fresh {
                self.loaded = true;
                self.rebuild_rows();
            }
            self.sync_preview_request(fetcher);
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
                        Action::Start { resume } => {
                            // Only exit on success; on failure stay open and show why.
                            if self.start_selected_worktree(resume) {
                                break;
                            }
                        }
                        Action::Refresh { invalidate } => {
                            fetcher.request_refresh(
                                invalidate,
                                self.all_sessions,
                                self.desired_preview(),
                            );
                            self.last_preview_req = self.desired_preview();
                        }
                        Action::None => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// The preview the worker should be fetching right now: the selection's target
    /// while the preview pane is shown, nothing otherwise.
    fn desired_preview(&self) -> Option<PreviewTarget> {
        if !self.show_preview {
            return None;
        }
        self.preview_target_for_selection()
    }

    /// Preview target of the selected row: a live pane capture for an agent, a git
    /// summary for an idle worktree.
    fn preview_target_for_selection(&self) -> Option<PreviewTarget> {
        if let Some(a) = self.selected_agent() {
            return Some(PreviewTarget::Pane {
                socket: a.state.socket.clone(),
                pane_id: a.pane.pane_id.clone(),
            });
        }
        self.selected_worktree().map(|w| PreviewTarget::Worktree {
            path: w.path.clone(),
        })
    }

    /// Tell the worker when the preview target changes (selection moved, preview
    /// toggled, rows rebuilt) so the next snapshot carries the right text. The
    /// worker keeps refreshing the last target on its own tick — this only needs to
    /// fire on changes, keeping the input loop free of tmux/git work entirely.
    fn sync_preview_request(&mut self, fetcher: &crate::fetcher::Fetcher) {
        let desired = self.desired_preview();
        if desired != self.last_preview_req {
            fetcher.request_refresh(false, self.all_sessions, desired.clone());
            self.last_preview_req = desired;
        }
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
            KeyCode::Char('X') => self.begin_close(),
            KeyCode::Char('I') => self.begin_interrupt(),
            // Interaction (Phase 3): approve/deny a pending prompt, or compose a message.
            KeyCode::Char('a') => self.respond(Response::Approve),
            KeyCode::Char('d') => self.respond(Response::Deny),
            KeyCode::Char(c @ '1'..='9') => self.respond(Response::Pick(c)),
            KeyCode::Char('i') => {
                if self.selected_agent().is_some() {
                    self.send_input.clear();
                    self.mode = Mode::Send;
                }
            }
            KeyCode::Enter if self.selected_agent().is_some() => return Action::Jump,
            KeyCode::Enter if self.selected_worktree().is_some() => {
                return Action::Start { resume: false }
            }
            KeyCode::Char('c') if self.selected_worktree().is_some() => {
                return Action::Start { resume: true }
            }
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
    /// agent of a session can be spawned from Hydra too. Input is `name[: prompt]`:
    /// with a prompt, the agent is launched with it as its first task.
    fn spawn_agent(&mut self, input: &str) {
        let (target, prompt) = parse_spawn_input(input);
        if matches!(target, SpawnTarget::Name("")) {
            self.message = Some("spawn needs a name or PR (name|pr-url[: prompt])".into());
            return;
        }
        let Some((socket, session, cwd)) = self.spawn_context() else {
            self.message = Some("spawn needs an agent, or open the popup from a git repo".into());
            return;
        };
        // A PR becomes an ordinary local branch first; everything below then treats it
        // exactly like a named spawn whose branch happens to exist already.
        let name = match target {
            SpawnTarget::Name(n) => n.to_string(),
            SpawnTarget::Pr(pr) => match crate::worktree::fetch_pr(&cwd, &pr) {
                Ok(branch) => branch,
                Err(e) => {
                    self.message = Some(format!("fetch failed: {e}"));
                    return;
                }
            },
        };
        let name = name.as_str();
        let command = agent_command_with_prompt(&self.config.agent.command, prompt);
        let path = worktree_root(&self.config).join(sanitize(name));
        let path_str = path.display().to_string();
        let base = crate::worktree::default_branch(&cwd);

        if let Err(e) = crate::worktree::create_worktree(&cwd, &path_str, name, &base) {
            self.message = Some(format!("worktree failed: {e}"));
            return;
        }
        self.message = match self.config.spawn_mode() {
            crate::config::SpawnMode::Window => match crate::tmux::new_window(
                &socket,
                &session,
                &sanitize(name),
                &path_str,
                &command,
            ) {
                Ok(_window_id) => Some(format!("✓ spawned {name}")),
                Err(e) => Some(format!("window failed: {e}")),
            },
            crate::config::SpawnMode::Session => {
                // New worktree => branch == name. Stay in the popup (as window mode's
                // `n` does), so several can be spawned in a row. Scope the session name by
                // the repo (of `cwd`, where the worktree was created) so same-branch
                // spawns in different repos don't collide on one session name.
                let repo = crate::worktree::resolve(&cwd)
                    .map(|w| w.repo_name)
                    .unwrap_or_default();
                let sess = session_name(&repo, Some(name), &path_str);
                if crate::tmux::session_exists(&socket, &sess) {
                    Some(format!("session {sess} already exists"))
                } else {
                    match crate::tmux::new_session(
                        &socket,
                        &sess,
                        &path_str,
                        &sanitize(name),
                        &command,
                        &self.config.agent.extra_scripts(&base),
                    ) {
                        Ok(agent_win) => {
                            // Make the Claude window (window 2) the session's current
                            // window, so a later Enter that reuses this session (via
                            // switch_client) lands on Claude, not the shell window that
                            // new_session leaves current.
                            let _ = crate::tmux::select_window_id(&socket, &agent_win);
                            Some(format!("✓ spawned {name} in session {sess}"))
                        }
                        Err(e) => Some(format!("session failed: {e}")),
                    }
                }
            }
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

    /// Start an agent in the selected idle worktree: open a new window (named after the
    /// branch) in the current session, in the worktree's directory. The new agent then
    /// appears via its own SessionStart hook. `new-window` switches to it, so exiting the
    /// popup lands the user on the new agent. `resume` runs the configured resume
    /// command (`claude --continue`) so the worktree's last conversation picks up
    /// where it left off, instead of a fresh start.
    ///
    /// Returns `true` on success (caller exits the popup). On any failure it records a
    /// footer message and returns `false` so the popup stays open with the reason
    /// visible, rather than closing silently.
    fn start_selected_worktree(&mut self, resume: bool) -> bool {
        let command = if resume {
            self.config.agent.resume_command.clone()
        } else {
            self.config.agent.command.clone()
        };
        let Some(wt) = self.selected_worktree() else {
            self.message = Some("no worktree selected".into());
            return false;
        };
        let branch = wt.branch.clone();
        let name = sanitize(&branch.clone().unwrap_or_else(|| wt.path.clone()));
        let path = wt.path.clone();
        let repo = wt.repo_name.clone();
        let Some(socket) = tmux::current_socket() else {
            self.message = Some("not inside tmux".into());
            return false;
        };
        let Some(session) = tmux::current_session(&socket) else {
            self.message = Some("could not resolve the current tmux session".into());
            return false;
        };
        match self.config.spawn_mode() {
            crate::config::SpawnMode::Window => {
                match tmux::new_window(&socket, &session, &name, &path, &command) {
                    Ok(window_id) => {
                        // Switch to the new window so closing the popup lands on it.
                        let _ = tmux::select_window_id(&socket, &window_id);
                        true
                    }
                    Err(e) => {
                        self.message = Some(format!("start failed: {e}"));
                        false
                    }
                }
            }
            crate::config::SpawnMode::Session => {
                let sess = session_name(&repo, branch.as_deref(), &path);
                if crate::tmux::session_exists(&socket, &sess) {
                    // Idempotent: reuse the existing session instead of erroring.
                    match crate::tmux::switch_client(&socket, &sess) {
                        Ok(()) => true,
                        Err(e) => {
                            self.message = Some(format!("switch failed: {e}"));
                            false
                        }
                    }
                } else {
                    // `{base}` in an extra window needs the default branch; skip the
                    // git call when there are none.
                    let extras = if self.config.agent.extra_windows.is_empty() {
                        Vec::new()
                    } else {
                        let base = crate::worktree::default_branch(&path);
                        self.config.agent.extra_scripts(&base)
                    };
                    match crate::tmux::new_session(&socket, &sess, &path, &name, &command, &extras)
                    {
                        Ok(agent_win) => {
                            // Land on the Claude window (window 2), then attach.
                            let _ = tmux::select_window_id(&socket, &agent_win);
                            match crate::tmux::switch_client(&socket, &sess) {
                                Ok(()) => true,
                                Err(e) => {
                                    self.message = Some(format!("switch failed: {e}"));
                                    false
                                }
                            }
                        }
                        Err(e) => {
                            self.message = Some(format!("start failed: {e}"));
                            false
                        }
                    }
                }
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

    /// Resolve the selected row (agent or idle worktree) to its worktree and the tmux
    /// windows it can own. Shared by removal and close.
    fn selected_worktree_ref(&self) -> Result<SelectedWorktree, String> {
        let (path, label, repo_key, socket, agent, repo_name, branch) =
            if let Some(a) = self.selected_agent() {
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
                    Some(a.state.socket.clone()),
                    Some(a.pane.pane_id.clone()),
                    wt.repo_name.clone(),
                    wt.branch.clone(),
                )
            } else if let Some(w) = self.selected_worktree() {
                (
                    w.path.clone(),
                    agent::worktree_label(w),
                    w.repo_key.clone(),
                    // No agent to self-report a socket, so use the popup's: an idle worktree's
                    // leftover windows (session mode's shell + agent) live on this server.
                    self.popup_socket(),
                    None,
                    w.repo_name.clone(),
                    w.branch.clone(),
                )
            } else {
                return Err("nothing selected".into());
            };
        let base_cwd = repo_key
            .strip_suffix("/.git")
            .unwrap_or(&repo_key)
            .to_string();
        Ok(SelectedWorktree {
            path,
            label,
            base_cwd,
            socket,
            agent,
            repo_name,
            branch,
        })
    }

    /// Build a `RemoveTarget` from the current selection, or an error explaining why the
    /// selection can't be removed (main worktree, Hydra's own cwd, or not a worktree).
    fn remove_target(&self) -> Result<RemoveTarget, String> {
        let sel = self.selected_worktree_ref()?;
        let canon_path = canon(&sel.path);
        if canon_path == canon(&sel.base_cwd) {
            return Err("can't remove the main worktree".into());
        }
        if is_hydras_cwd(&sel.path) {
            return Err("can't remove the worktree Hydra is running in".into());
        }
        let dirty = match self.selected_agent() {
            Some(a) => a.dirty > 0,
            None => crate::worktree::is_dirty(&sel.path),
        };
        Ok(RemoveTarget {
            path: sel.path,
            branch: sel.label,
            base_cwd: sel.base_cwd,
            socket: sel.socket,
            agent: sel.agent,
            dirty,
        })
    }

    /// Panes on `socket`, through the test seam when one is set.
    fn panes_on(&self, socket: &str) -> Vec<tmux::Pane> {
        match self.list_panes_fn {
            Some(f) => f(socket),
            None => tmux::list_panes(socket),
        }
    }

    /// The popup's own tmux socket, through the test seam when one is set.
    fn popup_socket(&self) -> Option<String> {
        match self.current_socket_fn {
            Some(f) => f(),
            None => tmux::current_socket(),
        }
    }

    /// Kill `windows` on `socket` in the given order (callers pass `agent::windows_to_kill`
    /// order: highest index first, so `renumber-windows` can't shift the ones still to
    /// go). Shared by remove (`x`) and close (`X`).
    fn kill_windows(&self, socket: &str, windows: &[(String, u32)]) -> Result<(), String> {
        for (session, window) in windows {
            match self.kill_window_fn {
                Some(f) => f(socket, session, *window),
                None => tmux::kill_window(socket, session, *window),
            }
            .map_err(|e| format!("kill window failed: {e}"))?;
        }
        Ok(())
    }

    /// Plan a close of `path`'s windows on `socket` against the live panes (see
    /// `agent::windows_to_close`). The agent's window comes from `own_pane` looked up in
    /// *this* listing, so a window renumbered since the key press can't be mistaken for
    /// it. `owned_session` selects `CloseScope::Owned` (main checkout) over `Rooted`. The
    /// guard is tmux membership, not cwd: the session the popup's client is attached to
    /// (only knowable on the popup's own socket) must not lose every window, and
    /// worktrees nested inside `path` keep theirs.
    fn plan_close(
        &self,
        socket: &str,
        path: &str,
        own_pane: Option<&str>,
        owned_session: Option<&str>,
    ) -> Result<Vec<(String, u32)>, agent::CloseRefusal> {
        let panes = self.panes_on(socket);
        let self_pane = match self.self_pane_fn {
            Some(f) => f(),
            None => std::env::var("TMUX_PANE").ok(),
        };
        let client_session = if self.popup_socket().as_deref() == Some(socket) {
            match self.client_session_fn {
                Some(f) => f(socket),
                None => tmux::current_session(socket),
            }
        } else {
            None
        };
        let other_roots: Vec<String> = self
            .agents
            .iter()
            .filter_map(|a| a.worktree.as_ref().map(|w| w.root.clone()))
            .chain(self.idle.iter().map(|w| w.path.clone()))
            .collect();
        let scope = match owned_session {
            Some(session) => agent::CloseScope::Owned { session },
            None => agent::CloseScope::Rooted,
        };
        agent::windows_to_close(
            &panes,
            path,
            own_pane,
            scope,
            agent::CloseGuard {
                other_roots: &other_roots,
                self_pane: self_pane.as_deref(),
                client_session: client_session.as_deref(),
            },
        )
    }

    /// Plan a close of the selection's windows against the live panes, or explain why
    /// it can't happen (Hydra's own cwd/pane/session, nothing open). The main worktree is
    /// fine: removal refuses it only because git can't remove it, and close keeps it
    /// anyway. But the main checkout is where the user's own editor/shell windows live,
    /// so there the close is limited to what Hydra owns (`CloseScope::Owned`): the
    /// worktree's dedicated session (if any) and the agent's own window. A linked
    /// worktree holds nothing but its own windows, so it closes everything rooted in it.
    fn close_target(&self) -> Result<CloseTarget, String> {
        let sel = self.selected_worktree_ref()?;
        if is_hydras_cwd(&sel.path) {
            return Err("can't close the worktree Hydra is running in".into());
        }
        let Some(socket) = sel.socket.clone() else {
            return Err(agent::CloseRefusal::NothingOpen.message(&sel.label));
        };
        let owned_session = (canon(&sel.path) == canon(&sel.base_cwd))
            .then(|| session_name(&sel.repo_name, sel.branch.as_deref(), &sel.path));
        let wins = self
            .plan_close(
                &socket,
                &sel.path,
                sel.agent.as_deref(),
                owned_session.as_deref(),
            )
            .map_err(|r| r.message(&sel.label))?;
        Ok(CloseTarget {
            path: sel.path,
            label: sel.label,
            socket,
            agent: sel.agent,
            owned_session,
            windows: wins.len(),
        })
    }

    /// Begin closing the selected worktree's windows/session (worktree kept): enter
    /// confirm mode, or show why there's nothing to close.
    fn begin_close(&mut self) {
        match self.close_target() {
            Ok(target) => {
                self.pending_close = Some(target);
                self.mode = Mode::Confirm;
            }
            Err(msg) => self.message = Some(msg),
        }
    }

    /// Perform the confirmed close: re-plan against the panes as they are *now* (indices
    /// may have shifted since the prompt; the agent's window is re-resolved from its pane
    /// id, and dropped if that pane is gone), kill them top-down, and keep the worktree. The
    /// row comes back as an idle worktree on the next (invalidated) snapshot; selection
    /// follows it there by path.
    fn do_close(&mut self) -> Action {
        self.mode = Mode::Normal;
        let Some(t) = self.pending_close.take() else {
            return Action::None;
        };
        let wins = match self.plan_close(
            &t.socket,
            &t.path,
            t.agent.as_deref(),
            t.owned_session.as_deref(),
        ) {
            Ok(w) => w,
            Err(r) => {
                self.message = Some(r.message(&t.label));
                return Action::Refresh { invalidate: true };
            }
        };
        if let Err(e) = self.kill_windows(&t.socket, &wins) {
            self.message = Some(e);
            return Action::Refresh { invalidate: true };
        }
        self.message = Some(format!(
            "✓ closed {} ({} window{}) · worktree kept",
            t.label,
            wins.len(),
            if wins.len() == 1 { "" } else { "s" }
        ));
        self.selected_key = Some(t.path);
        Action::Refresh { invalidate: true }
    }

    fn handle_confirm_key(&mut self, code: KeyCode) -> Action {
        match code {
            KeyCode::Char('y') | KeyCode::Char('Y') => {
                if self.pending_interrupt.is_some() {
                    self.do_interrupt()
                } else if self.pending_close.is_some() {
                    self.do_close()
                } else {
                    self.do_remove()
                }
            }
            _ => {
                self.pending_remove = None;
                self.pending_close = None;
                self.pending_interrupt = None;
                self.mode = Mode::Normal;
                Action::None
            }
        }
    }

    /// Begin interrupting the selected agent: gated on it actually WORKING (an idle
    /// or waiting agent has nothing to interrupt; a stray Escape into a prompt would
    /// dismiss it), confirmed before anything is sent.
    fn begin_interrupt(&mut self) {
        let Some((_, _, window, status)) = self.selected_target() else {
            return;
        };
        if status != Status::Working {
            self.message = Some(format!("win {window} isn't working"));
            return;
        }
        let a = self.selected_agent().expect("selected_target came from it");
        self.pending_interrupt = Some((
            a.state.socket.clone(),
            a.pane.pane_id.clone(),
            a.pane.window_index,
        ));
        self.mode = Mode::Confirm;
    }

    /// Perform the confirmed interrupt: re-read the state file at the last moment
    /// (same discipline as `respond`) so the Escape can't hit an agent that already
    /// stopped or is now waiting on a prompt — then send Escape, which stops
    /// Claude Code's current turn.
    fn do_interrupt(&mut self) -> Action {
        self.mode = Mode::Normal;
        let Some((socket, pane, window)) = self.pending_interrupt.take() else {
            return Action::None;
        };
        let fresh = crate::state::read_one(&socket, &pane).map(|s| s.status);
        if fresh != Some(Status::Working) {
            self.message = Some(format!("win {window} is no longer working"));
            return Action::None;
        }
        self.message = Some(match tmux::send_key(&socket, &pane, "Escape") {
            Ok(()) => format!("✓ interrupted win {window}"),
            Err(e) => format!("interrupt failed: {e}"),
        });
        Action::None
    }

    /// Perform the confirmed removal: kill every tmux window rooted in the worktree
    /// (agent or not), then `git worktree remove` (forcing when dirty). Branch is kept.
    /// On success the
    /// returned Refresh invalidates the worker's caches so the row disappears on
    /// the next snapshot (the UI stays responsive while git re-scans).
    fn do_remove(&mut self) -> Action {
        self.mode = Mode::Normal;
        let Some(target) = self.pending_remove.take() else {
            return Action::None;
        };
        // Kill every window rooted in the worktree, not just the agent's own — and do it
        // whether or not the row has a live agent. An idle worktree still owns its windows
        // (session mode: shell + agent), and leaving them behind orphans a whole session on
        // a directory git is about to delete.
        // Session mode: killing shell + agent empties and so destroys the dedicated
        // session. Window mode: only the agent window is rooted here, as before.
        if let Some(socket) = &target.socket {
            let panes = self.panes_on(socket);
            let wins = crate::agent::windows_to_kill(&panes, &target.path, target.agent.as_deref());
            if let Err(e) = self.kill_windows(socket, &wins) {
                self.message = Some(e);
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
        // The footer grows to as many lines as the width requires (keybar wrapping).
        let footer = self.footer(frame.area().width);
        let chunks =
            Layout::vertical([Constraint::Min(1), Constraint::Length(footer.len() as u16)])
                .split(frame.area());

        // Split the body into list + preview when the preview is on and there's room.
        let preview_on = self.show_preview && self.preview_target_for_selection().is_some();
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
                "  no Claude Code agents or worktrees in view"
            } else {
                "  no agents match filter"
            };
            let empty = List::new([ListItem::new(Line::from(Span::raw(msg).dim()))]).block(block);
            frame.render_widget(empty, list_area);
        } else {
            let now = now_secs();
            let widths = self.row_widths(list_area.width);
            let items: Vec<ListItem> = self
                .rows
                .iter()
                .map(|row| match row {
                    Row::Header { label, count } => header_row(label, *count, &self.colors),
                    Row::Agent(i) => {
                        agent_row(&self.view[*i], now, &self.colors, self.all_sessions, widths)
                    }
                    Row::Worktree(i) => worktree_row(&self.idle_view[*i], &self.colors, widths),
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

        frame.render_widget(ratatui::widgets::Paragraph::new(footer), chunks[1]);
    }

    fn draw_preview(&self, frame: &mut Frame, area: ratatui::layout::Rect) {
        let Some(target) = self.preview_target_for_selection() else {
            return;
        };
        // The title carries the selection's full name (branch + differing folder), so a
        // name the list row had to truncate is always readable somewhere. Only the
        // preview pane's own width limits it.
        // A folder hint that doesn't fit beside the name moves to the bottom border.
        let name = if let Some(a) = self.selected_agent() {
            Some((Some(a.pane.window_index), agent::agent_name(a)))
        } else {
            self.selected_worktree()
                .map(|w| (None, agent::worktree_name(w)))
        };
        let room = (area.width as usize).saturating_sub(4); // borders + title padding
        let (top, bottom) = match name {
            Some((win, (label, hint))) => agent::preview_titles(win, &label, hint.as_deref(), room),
            None => ("preview".to_string(), None),
        };
        let mut block = Block::default()
            .borders(Borders::ALL)
            .title(format!(" {top} "))
            .title_style(Style::default().add_modifier(Modifier::DIM));
        if let Some(bottom) = bottom {
            block = block.title_bottom(format!(" {bottom} "));
        }
        // The text is fetched by the worker (`Overview::preview`); the UI never
        // shells out. While the snapshot's target lags the selection (just moved),
        // show a placeholder — the requested snapshot arrives within a tick.
        let raw = match &self.preview {
            Some((t, text)) if *t == target => text.as_str(),
            _ => "…",
        };
        // Show the tail (the most recent output/prompt) with its real colors:
        // capture-pane -e keeps the SGR sequences and ansi-to-tui turns them into
        // styled ratatui lines. Unparseable output falls back to plain text.
        let text = raw
            .into_text()
            .unwrap_or_else(|_| ratatui::text::Text::raw(raw));
        let rows = area.height.saturating_sub(2) as usize;
        let skip = text.lines.len().saturating_sub(rows);
        let tail: Vec<Line> = text.lines.into_iter().skip(skip).collect();
        let para = ratatui::widgets::Paragraph::new(tail).block(block);
        frame.render_widget(para, area);
    }

    /// Column widths for the list rows, sized from the widest place/name actually in
    /// view so every row lines up, then fitted to the list's inner width (see
    /// `agent::row_widths`): full branch names whenever they fit.
    fn row_widths(&self, width: u16) -> agent::RowWidths {
        let mut place_want = 1; // idle rows' "—"
        let mut name_want = 0;
        for row in &self.rows {
            let (label, hint) = match row {
                Row::Header { .. } => continue,
                Row::Agent(i) => {
                    let a = &self.view[*i];
                    let (session, suffix) = agent_place(a, self.all_sessions);
                    place_want = place_want.max(session.chars().count() + suffix.chars().count());
                    agent::agent_name(a)
                }
                Row::Worktree(i) => agent::worktree_name(&self.idle_view[*i]),
            };
            let hint_len = hint.map_or(0, |h| 3 + h.chars().count());
            name_want = name_want.max(label.chars().count() + hint_len);
        }
        agent::row_widths(
            (width as usize).saturating_sub(ROW_OVERHEAD),
            place_want,
            name_want,
        )
    }

    fn title(&self) -> String {
        // `scope_label` comes straight from the snapshot (repo name / "all sessions" /
        // session name); before the first snapshot it's empty.
        let scope = if self.scope_label.is_empty() {
            "…"
        } else {
            self.scope_label.as_str()
        };
        let needs = self
            .agents
            .iter()
            .filter(|a| a.effective_status == Status::NeedsInput)
            .count();
        let total = self.agents.len();
        if needs > 0 {
            format!(" Hydra · {scope} · {total} agents · ⚠ {needs} ")
        } else {
            format!(" Hydra · {scope} · {total} agents ")
        }
    }

    /// Footer lines for the given terminal width. Input/confirm modes and transient
    /// messages are a single line; the normal-mode keybar wraps onto extra lines
    /// when the popup is too narrow to fit every shortcut (nothing is clipped).
    fn footer(&self, width: u16) -> Vec<Line<'static>> {
        let line = match self.mode {
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
                Span::raw("name|pr-url[: prompt]  ⏎ create worktree+agent  Esc cancel").dim(),
            ]),
            Mode::Confirm => {
                let mut spans = if let Some((_, _, window)) = &self.pending_interrupt {
                    vec![
                        Span::styled(
                            format!(" interrupt win {window}?"),
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(" sends Esc, stopping its current turn").dim(),
                    ]
                } else if let Some(t) = &self.pending_close {
                    vec![
                        Span::styled(
                            format!(" close {}'s windows/session?", t.label),
                            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                        ),
                        Span::raw(format!(
                            " kills {} window{}{} · worktree is kept",
                            t.windows,
                            if t.windows == 1 { "" } else { "s" },
                            if t.agent.is_some() {
                                " incl. its agent"
                            } else {
                                ""
                            },
                        ))
                        .dim(),
                    ]
                } else {
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
                        // Removal is mode-agnostic: it kills every window rooted in the
                        // worktree, which in session mode takes the whole session with it.
                        spans.push(
                            Span::raw(if t.agent.is_some() {
                                " kills its agent and every window in it"
                            } else {
                                " kills every tmux window in it"
                            })
                            .dim(),
                        );
                        if t.dirty {
                            spans.push(Span::styled(
                                " ⚠ uncommitted changes (force)",
                                Style::default().fg(Color::Yellow),
                            ));
                        }
                    }
                    spans
                };
                spans.push(Span::raw("   "));
                spans.push(Span::raw("y confirm  n cancel").dim());
                Line::from(spans)
            }
            Mode::Normal => {
                // A transient action result takes over the footer until the next key.
                if let Some(msg) = &self.message {
                    return vec![Line::from(Span::styled(
                        format!(" {msg}"),
                        Style::default().fg(Color::Yellow),
                    ))];
                }
                return self.keybar_lines(width);
            }
        };
        vec![line]
    }

    /// The keybar packed into rows that fit `width`, colored from the theme
    /// (footer_key / footer_label), with the active-filter indicator appended.
    fn keybar_lines(&self, width: u16) -> Vec<Line<'static>> {
        let key = |k: &str| {
            Span::styled(
                k.to_string(),
                Style::default()
                    .fg(self.colors.footer_key)
                    .add_modifier(Modifier::BOLD),
            )
        };
        let label =
            |l: &str| Span::styled(l.to_string(), Style::default().fg(self.colors.footer_label));

        let rows = pack_keybar(KEYBAR, width as usize);
        let mut lines: Vec<Line> = rows
            .iter()
            .map(|row| {
                let mut spans = vec![Span::raw(" ")];
                for (i, (k, l)) in row.iter().enumerate() {
                    if i > 0 {
                        spans.push(Span::raw("  "));
                    }
                    spans.push(key(k));
                    spans.push(Span::raw(" "));
                    spans.push(label(l));
                }
                Line::from(spans)
            })
            .collect();

        if !self.filter.is_empty() {
            let text = format!("filter: {}", self.filter);
            let span = Span::styled(text.clone(), Style::default().fg(Color::Yellow));
            // Append to the last row when it fits, else give it its own line.
            let last_used = rows.last().map(|r| keybar_row_width(r)).unwrap_or(0);
            if last_used + 3 + text.chars().count() <= width as usize {
                if let Some(last) = lines.last_mut() {
                    last.push_span(Span::raw("   "));
                    last.push_span(span);
                }
            } else {
                lines.push(Line::from(vec![Span::raw(" "), span]));
            }
        }
        lines
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

/// Columns a list row spends outside its three variable cells: block borders (2),
/// indent + glyph (3), the age cell (5) and the two gutters after place and name (2+2).
const ROW_OVERHEAD: usize = 2 + 3 + 5 + 2 + 2;

/// An agent row's place cell as `(session_or_"win", suffix)`. In all-sessions view it's
/// `session:window`, with the `<repo>-` prefix dropped (the row already sits under its
/// repo header — display only, tmux targets keep the real name); otherwise `win N`.
fn agent_place(a: &Agent, show_session: bool) -> (String, String) {
    if show_session {
        let repo = a.worktree.as_ref().map(|w| w.repo_name.as_str());
        (
            agent::display_session(&a.pane.session_name, repo).to_string(),
            format!(":{}", a.pane.window_index),
        )
    } else {
        ("win".to_string(), format!(" {:>2}", a.pane.window_index))
    }
}

/// The name cell as spans: the label in `label_style`, then the dimmed ` · folder` hint,
/// fitted into and padded to `width` (+ the 2-col gutter) so the next column aligns.
fn name_spans(
    label: &str,
    hint: Option<&str>,
    width: usize,
    label_style: Style,
) -> Vec<Span<'static>> {
    let (label, hint) = agent::fit_name(label, hint, width);
    let used = label.chars().count() + hint.chars().count();
    let pad = " ".repeat(width.saturating_sub(used) + 2);
    vec![
        Span::styled(label, label_style),
        Span::raw(hint).dim(),
        Span::raw(pad),
    ]
}

/// An idle worktree with no agent: dimmed, with a "start ⏎" affordance and a badge for
/// how its HEAD stands against the default branch. `merged` is the strongest "safe to
/// remove" signal the list can give, so it's reserved for an actual merge — a branch with
/// no commits yet reads `no commits` instead (see `worktree::BranchState`).
fn worktree_row(
    w: &IdleWorktree,
    colors: &TuiColors,
    widths: agent::RowWidths,
) -> ListItem<'static> {
    let (label, hint) = agent::worktree_name(w);
    let mut spans = vec![
        Span::styled("  ○", Style::default().fg(colors.worktree_row)),
        Span::raw(format!("   — {:<pw$}  ", "—", pw = widths.place)),
    ];
    spans.extend(name_spans(
        &label,
        hint.as_deref(),
        widths.name,
        Style::default().fg(colors.branch).dim(),
    ));
    let badge = match w.state {
        Some(BranchState::Merged) => Some(("merged".to_string(), colors.working)),
        Some(BranchState::NoCommits) => Some(("no commits".to_string(), colors.unknown)),
        Some(BranchState::Diverged { ahead, behind }) => {
            let text = if behind > 0 {
                format!("↑{ahead} ↓{behind}")
            } else {
                format!("↑{ahead}")
            };
            Some((text, colors.dirty))
        }
        None => None,
    };
    const START: &str = "start ⏎";
    const BADGE_W: usize = 12;
    let start_w = START.chars().count();
    let start = |s: &'static str| Span::styled(s, Style::default().fg(colors.worktree_row));
    if widths.detail >= BADGE_W + start_w {
        // Pad every badge to one width so the "start ⏎" column stays aligned across rows.
        spans.push(match badge {
            Some((text, color)) => Span::styled(
                format!("{text:<BADGE_W$}"),
                Style::default().fg(color).dim(),
            ),
            None => Span::raw(" ".repeat(BADGE_W)),
        });
        spans.push(start(START));
    } else {
        // The name took the room (see `agent::row_widths`): keep the start affordance,
        // and the badge only when both still fit.
        if let Some((text, color)) = badge {
            if text.chars().count() + 1 + start_w <= widths.detail {
                spans.push(Span::styled(
                    format!("{text} "),
                    Style::default().fg(color).dim(),
                ));
            }
        }
        spans.push(start(if widths.detail >= start_w {
            START
        } else {
            "⏎"
        }));
    }
    ListItem::new(Line::from(spans))
}

fn agent_row(
    a: &Agent,
    now: u64,
    colors: &TuiColors,
    show_session: bool,
    widths: agent::RowWidths,
) -> ListItem<'static> {
    let (color, glyph) = match a.effective_status {
        Status::NeedsInput => (colors.needs_input, a.effective_status.glyph()),
        Status::Working => (colors.working, a.effective_status.glyph()),
        Status::Idle => (colors.idle, a.effective_status.glyph()),
        Status::Unknown => (colors.unknown, a.effective_status.glyph()),
    };

    let (label, hint) = agent::agent_name(a);
    let age = agent::format_age(now.saturating_sub(a.state.updated_at));
    // In all-sessions view each row says which session it lives in.
    let (session, suffix) = agent_place(a, show_session);
    let place = agent::fit_place(&session, &suffix, widths.place);
    let dirty = (a.dirty > 0).then(|| format!("Δ{} ", a.dirty));

    // Every variable cell is fitted to the row's actual width (List items are single
    // lines: overflow would run off-screen instead of wrapping).
    let budget = widths
        .detail
        .saturating_sub(dirty.as_deref().map_or(0, |d| d.chars().count()));
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
        Span::raw(format!("{place:<pw$}  ", pw = widths.place)),
    ];
    spans.extend(name_spans(
        &label,
        hint.as_deref(),
        widths.name,
        Style::default().fg(colors.branch),
    ));
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

/// Whether Hydra should open in all-sessions view. In session spawn mode agents live in
/// their own sessions, so all-sessions is the default scope; `s` still toggles at runtime.
fn starts_all_sessions(config: &crate::config::Config) -> bool {
    matches!(config.spawn_mode(), crate::config::SpawnMode::Session)
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

/// Whether `path` is the worktree Hydra itself is running in (its cwd) — tearing that
/// down would pull the directory (or the windows) out from under the popup.
fn is_hydras_cwd(path: &str) -> bool {
    std::env::current_dir().is_ok_and(|cwd| canon(path) == canon(&cwd.to_string_lossy()))
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
    agent::path_safe(name)
}

/// What a Spawn-mode line asked for: a branch to create, or a pull request to fetch
/// and then branch from.
#[derive(Debug, PartialEq, Eq)]
enum SpawnTarget<'a> {
    Name(&'a str),
    Pr(crate::pr::PrRef),
}

/// Split Spawn-mode input into its target and an optional initial prompt: everything
/// after the FIRST `:` is the task the new agent starts with (branch names can't
/// contain `:`, so the split is unambiguous). Both sides are trimmed; an empty
/// prompt means none.
///
/// A PR reference is matched BEFORE that split, because a pasted URL carries a `:` of
/// its own in `https://` — splitting first would spawn a branch called `https`.
fn parse_spawn_input(input: &str) -> (SpawnTarget<'_>, Option<&str>) {
    if let Some((pr, rest)) = crate::pr::parse_pr_ref(input) {
        let prompt = rest.trim().trim_start_matches(':').trim();
        return (SpawnTarget::Pr(pr), (!prompt.is_empty()).then_some(prompt));
    }
    match input.split_once(':') {
        Some((name, prompt)) => {
            let prompt = prompt.trim();
            (
                SpawnTarget::Name(name.trim()),
                (!prompt.is_empty()).then_some(prompt),
            )
        }
        None => (SpawnTarget::Name(input.trim()), None),
    }
}

/// The command a spawned agent runs: the configured command, plus the initial prompt
/// as a single-quoted shell argument. tmux hands the command string to a shell, so
/// the prompt is single-quoted with embedded quotes escaped (`'` → `'\''`) — the one
/// construct the POSIX shell never interprets further.
fn agent_command_with_prompt(base: &str, prompt: Option<&str>) -> String {
    match prompt {
        Some(p) => format!("{base} '{}'", p.replace('\'', r"'\''")),
        None => base.to_string(),
    }
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
            state: None,
        }
    }

    /// An App with the given data, rows built and selection restored — the state the
    /// TUI is in right after a fetch tick.
    fn app_with(agents: Vec<Agent>, idle: Vec<IdleWorktree>) -> App {
        let mut app = App {
            agents,
            idle,
            // Hermetic tmux: a fixed popup socket on which the client is attached to no
            // known session, no panes, and kills that are only recorded.
            current_socket_fn: Some(test_socket),
            client_session_fn: Some(|_| None),
            // Not inside any pane, whatever `TMUX_PANE` the test runner inherited.
            self_pane_fn: Some(|| None),
            list_panes_fn: Some(no_panes),
            kill_window_fn: Some(record_kill),
            ..App::default()
        };
        app.rebuild_rows();
        app
    }

    fn test_socket() -> Option<String> {
        Some("/sock".into())
    }

    thread_local! {
        /// Windows `record_kill` was asked to kill, in order: (socket, session, index).
        static KILLED: std::cell::RefCell<Vec<(String, String, u32)>> =
            const { std::cell::RefCell::new(Vec::new()) };
    }

    fn record_kill(socket: &str, session: &str, window: u32) -> std::io::Result<()> {
        KILLED.with(|k| k.borrow_mut().push((socket.into(), session.into(), window)));
        Ok(())
    }

    fn take_killed() -> Vec<(String, String, u32)> {
        KILLED.with(|k| std::mem::take(&mut *k.borrow_mut()))
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
    fn remove_target_for_an_idle_worktree_still_has_a_socket_to_tear_windows_down_on() {
        // Regression (orphan session): an idle-worktree row has no agent, and the removal
        // used to take its socket *from* that agent — so with none, no window was ever
        // killed and session mode was left with a session rooted in a deleted directory.
        let app = app_with(
            vec![],
            vec![idle_wt("/wt/feat", "feat", "/repo/main/.git", "main-repo")],
        );
        let target = app.remove_target().expect("an idle worktree is removable");
        assert_eq!(target.path, "/wt/feat");
        assert_eq!(target.base_cwd, "/repo/main");
        assert!(target.agent.is_none(), "no live agent on an idle worktree");
        assert_eq!(
            target.socket.as_deref(),
            Some("/sock"), // `app_with`'s popup-socket seam
            "falls back to the popup's own socket, where the leftover windows live"
        );
    }

    #[test]
    fn remove_confirm_warns_that_windows_die_even_without_an_agent() {
        let mut app = app_with(
            vec![],
            vec![idle_wt("/wt/feat", "feat", "/repo/main/.git", "main-repo")],
        );
        press(&mut app, KeyCode::Char('x'));
        assert!(app.mode == Mode::Confirm && app.pending_remove.is_some());
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("remove worktree feat?"), "prompt rendered");
        assert!(
            text.contains("kills every tmux window in it"),
            "the teardown is disclosed for an agent-less worktree too: {text}"
        );
    }

    /// Render `app` into a test buffer and return the flattened text.
    fn rendered(app: &mut App, w: u16, h: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect()
    }

    /// A session-mode session for `/wt/feat`: shell, diff extra window, agent.
    fn session_panes(_socket: &str) -> Vec<Pane> {
        let mk = |id: &str, win: u32| Pane {
            pane_id: id.into(),
            session_name: "repo-feat".into(),
            window_index: win,
            window_name: "w".into(),
            cwd: "/wt/feat".into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        };
        vec![mk("%10", 1), mk("%11", 2), mk("%12", 3)]
    }

    fn no_panes(_socket: &str) -> Vec<Pane> {
        Vec::new()
    }

    /// Just the test `agent("%1", ..)`'s own pane, live in `proj` (cwd diverged from its
    /// worktree, so only its pane id ties it there).
    fn agent_pane_only(_socket: &str) -> Vec<Pane> {
        vec![Pane {
            pane_id: "%1".into(),
            session_name: "proj".into(),
            window_index: 3,
            window_name: "claude".into(),
            cwd: "/repo".into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        }]
    }

    #[test]
    fn close_on_an_agent_row_enters_confirm_with_the_close_prompt_and_n_cancels() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                3,
                Some(("/repo/main/.git", "main-repo", "feat", "/wt/feat")),
            )],
            vec![],
        );
        app.list_panes_fn = Some(agent_pane_only);
        press(&mut app, KeyCode::Char('X'));
        assert!(app.mode == Mode::Confirm && app.pending_close.is_some());
        assert!(app.pending_remove.is_none(), "close is not a removal");
        let text = rendered(&mut app, 100, 24);
        assert!(
            text.contains("close feat's windows/session?"),
            "close prompt rendered: {text}"
        );
        assert!(text.contains("worktree is kept"), "{text}");
        press(&mut app, KeyCode::Char('n'));
        assert!(app.mode == Mode::Normal && app.pending_close.is_none());
    }

    #[test]
    fn close_refuses_when_hydra_runs_in_one_of_the_windows() {
        let mut app = app_with(
            vec![],
            vec![idle_wt("/wt/feat", "feat", "/repo/main/.git", "main-repo")],
        );
        app.list_panes_fn = Some(session_panes);
        app.self_pane_fn = Some(|| Some("%11".into()));
        press(&mut app, KeyCode::Char('X'));
        assert!(app.mode != Mode::Confirm && app.pending_close.is_none());
        let text = rendered(&mut app, 100, 24);
        assert!(
            text.contains("can't close the window Hydra is running in"),
            "{text}"
        );
    }

    #[test]
    fn close_on_an_idle_worktree_counts_its_session_windows() {
        let mut app = app_with(
            vec![],
            vec![idle_wt("/wt/feat", "feat", "/repo/main/.git", "main-repo")],
        );
        app.list_panes_fn = Some(session_panes);
        let t = app
            .close_target()
            .expect("idle worktree's session windows are closable");
        assert_eq!(t.windows, 3);
        assert_eq!(t.socket, "/sock", "the popup's socket");
        press(&mut app, KeyCode::Char('X'));
        assert!(app.mode == Mode::Confirm && app.pending_close.is_some());
        let text = rendered(&mut app, 100, 24);
        assert!(
            text.contains("kills 3 windows · worktree is kept"),
            "{text}"
        );
    }

    #[test]
    fn confirming_a_close_kills_the_windows_top_down_and_keeps_the_worktree() {
        take_killed();
        let mut app = app_with(
            vec![],
            vec![idle_wt("/wt/feat", "feat", "/repo/main/.git", "main-repo")],
        );
        app.list_panes_fn = Some(session_panes);
        press(&mut app, KeyCode::Char('X'));
        assert!(app.mode == Mode::Confirm);
        let action = press(&mut app, KeyCode::Char('y'));
        assert_eq!(action, Action::Refresh { invalidate: true });
        assert_eq!(
            take_killed(),
            vec![
                ("/sock".to_string(), "repo-feat".to_string(), 3),
                ("/sock".to_string(), "repo-feat".to_string(), 2),
                ("/sock".to_string(), "repo-feat".to_string(), 1),
            ]
        );
        assert_eq!(
            app.message.as_deref(),
            Some("✓ closed feat (3 windows) · worktree kept")
        );
        assert_eq!(app.selected_key.as_deref(), Some("/wt/feat"));
        assert!(app.mode == Mode::Normal && app.pending_close.is_none());
    }

    #[test]
    fn close_refuses_the_session_the_popup_is_attached_to() {
        take_killed();
        let mut app = app_with(
            vec![],
            vec![idle_wt("/wt/feat", "feat", "/repo/main/.git", "main-repo")],
        );
        app.list_panes_fn = Some(session_panes);
        app.client_session_fn = Some(|_| Some("repo-feat".into()));
        press(&mut app, KeyCode::Char('X'));
        assert!(app.mode == Mode::Normal && app.pending_close.is_none());
        assert!(
            app.message
                .as_deref()
                .is_some_and(|m| m.contains("attached")),
            "{:?}",
            app.message
        );
        assert!(take_killed().is_empty());
    }

    /// The main checkout plus a Claude Code `--worktree` nested inside it, each with a
    /// window in its own session.
    fn nested_panes(_socket: &str) -> Vec<Pane> {
        let mk = |id: &str, session: &str, cwd: &str| Pane {
            pane_id: id.into(),
            session_name: session.into(),
            window_index: 1,
            window_name: "w".into(),
            cwd: cwd.into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        };
        vec![
            // Main's dedicated session (session mode: `session_name(repo, main, path)`).
            mk("%1", "main-repo-main", "/repo/main"),
            mk("%2", "main-repo-x", "/repo/main/.claude/worktrees/x"),
        ]
    }

    #[test]
    fn closing_the_main_checkout_spares_a_worktree_nested_in_it() {
        take_killed();
        let mut app = app_with(
            vec![],
            vec![
                idle_wt("/repo/main", "main", "/repo/main/.git", "main-repo"),
                idle_wt(
                    "/repo/main/.claude/worktrees/x",
                    "x",
                    "/repo/main/.git",
                    "main-repo",
                ),
            ],
        );
        app.list_panes_fn = Some(nested_panes);
        let t = app.close_target().expect("main has a window");
        assert_eq!(t.windows, 1, "only main's own window");
        press(&mut app, KeyCode::Char('X'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(
            take_killed(),
            vec![("/sock".to_string(), "main-repo-main".to_string(), 1)]
        );
    }

    /// The user's own working session rooted at the main checkout (shell + editor), a
    /// window-mode agent in it, and main's dedicated session-mode session.
    fn main_checkout_panes(_socket: &str) -> Vec<Pane> {
        let mk = |id: &str, session: &str, win: u32, cwd: &str| Pane {
            pane_id: id.into(),
            session_name: session.into(),
            window_index: win,
            window_name: "w".into(),
            cwd: cwd.into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        };
        vec![
            mk("%1", "work", 1, "/repo/main"),     // shell
            mk("%2", "work", 2, "/repo/main/src"), // editor with unsaved state
            mk("%3", "work", 3, "/repo/main"),     // the agent
        ]
    }

    #[test]
    fn closing_an_agent_on_the_main_checkout_spares_the_users_editor() {
        take_killed();
        let mut a = agent(
            "%3",
            Status::Idle,
            3,
            Some(("/repo/main/.git", "main-repo", "main", "/repo/main")),
        );
        a.pane.session_name = "work".into();
        let mut app = app_with(vec![a], vec![]);
        app.list_panes_fn = Some(main_checkout_panes);
        let t = app.close_target().expect("the agent's window is closable");
        assert_eq!(t.windows, 1, "only the agent's own window");
        press(&mut app, KeyCode::Char('X'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(
            take_killed(),
            vec![("/sock".to_string(), "work".to_string(), 3)],
            "the shell and editor rooted in the repo stay"
        );
    }

    /// Before the confirm: shell 1, the agent (%1) in window 4, an editor in window 5.
    fn panes_before_shift(_socket: &str) -> Vec<Pane> {
        let mk = |id: &str, win: u32, cwd: &str| Pane {
            pane_id: id.into(),
            session_name: "proj".into(),
            window_index: win,
            window_name: "w".into(),
            cwd: cwd.into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        };
        vec![
            mk("%0", 1, "/home"),
            mk("%1", 4, "/repo"),
            mk("%2", 5, "/home/ed"),
        ]
    }

    /// After a lower window closed with `renumber-windows on`: agent 4 -> 3, editor 5 -> 4.
    fn panes_after_shift(_socket: &str) -> Vec<Pane> {
        panes_before_shift(_socket)
            .into_iter()
            .map(|mut p| {
                if p.window_index > 1 {
                    p.window_index -= 1;
                }
                p
            })
            .collect()
    }

    /// The agent's pane is gone by the confirm; the editor was renumbered into its slot.
    fn panes_agent_gone(_socket: &str) -> Vec<Pane> {
        let mut v = panes_after_shift(_socket);
        v.retain(|p| p.pane_id != "%1");
        v
    }

    #[test]
    fn confirming_a_close_after_windows_renumbered_kills_the_agents_real_window() {
        take_killed();
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                4,
                Some(("/repo/main/.git", "main-repo", "feat", "/wt/feat")),
            )],
            vec![],
        );
        app.list_panes_fn = Some(panes_before_shift);
        press(&mut app, KeyCode::Char('X'));
        assert!(app.mode == Mode::Confirm);
        // A lower window closes while the prompt is up.
        app.list_panes_fn = Some(panes_after_shift);
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(
            take_killed(),
            vec![("/sock".to_string(), "proj".to_string(), 3)],
            "the stale (proj, 4) now names the editor and must not be killed"
        );

        // And when the agent's pane is gone entirely, nothing else is killed in its place.
        app.list_panes_fn = Some(panes_before_shift);
        app.rebuild_rows();
        press(&mut app, KeyCode::Char('X'));
        assert!(app.mode == Mode::Confirm);
        app.list_panes_fn = Some(panes_agent_gone);
        press(&mut app, KeyCode::Char('y'));
        assert!(take_killed().is_empty());
        assert_eq!(app.message.as_deref(), Some("no open windows for feat"));
    }

    #[test]
    fn close_with_nothing_open_shows_a_message_instead_of_confirming() {
        let mut app = app_with(
            vec![],
            vec![idle_wt("/wt/feat", "feat", "/repo/main/.git", "main-repo")],
        );
        app.list_panes_fn = Some(no_panes);
        press(&mut app, KeyCode::Char('X'));
        assert!(app.mode == Mode::Normal && app.pending_close.is_none());
        assert_eq!(app.message.as_deref(), Some("no open windows for feat"));
    }

    #[test]
    fn close_allows_the_main_worktree_but_not_hydras_own_cwd() {
        let main = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/repo/main/.git", "main-repo", "main", "/repo/main")),
            )],
            vec![],
        );
        let mut main = main;
        main.list_panes_fn = Some(agent_pane_only);
        assert!(
            main.close_target().is_ok(),
            "closing main's windows is fine"
        );

        let cwd = std::env::current_dir().unwrap().display().to_string();
        let mut own = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/elsewhere/.git", "other", "b", cwd.as_str())),
            )],
            vec![],
        );
        own.list_panes_fn = Some(no_panes);
        assert_eq!(
            own.close_target().unwrap_err(),
            "can't close the worktree Hydra is running in"
        );
    }

    #[test]
    fn footer_shows_the_close_key() {
        let mut app = app_with(vec![], vec![]);
        let text = rendered(&mut app, 200, 10);
        assert!(text.contains("X close"), "{text}");
    }

    #[test]
    fn interrupt_gates_on_working_and_enters_confirm() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Working,
                4,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        press(&mut app, KeyCode::Char('I'));
        assert!(app.mode == Mode::Confirm && app.pending_interrupt.is_some());
        // Anything but y/Y cancels.
        press(&mut app, KeyCode::Char('n'));
        assert!(app.mode == Mode::Normal && app.pending_interrupt.is_none());
    }

    #[test]
    fn interrupt_refuses_agents_that_are_not_working() {
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                4,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        press(&mut app, KeyCode::Char('I'));
        assert!(app.mode == Mode::Normal);
        assert_eq!(app.message.as_deref(), Some("win 4 isn't working"));
    }

    #[test]
    fn interrupt_requires_a_fresh_working_state_file() {
        // The in-memory agent says Working, but no state file backs it up (the agent
        // already stopped) — confirming must refuse to send the Escape.
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Working,
                4,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        press(&mut app, KeyCode::Char('I'));
        press(&mut app, KeyCode::Char('y'));
        assert_eq!(app.message.as_deref(), Some("win 4 is no longer working"));
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
    fn preview_target_follows_the_selection() {
        use crate::fetcher::PreviewTarget;
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![idle_wt("/a/wt", "feat-x", "/a/.git", "alpha")],
        );
        // First selectable row is the agent → a pane capture target.
        assert_eq!(
            app.preview_target_for_selection(),
            Some(PreviewTarget::Pane {
                socket: "/sock".into(),
                pane_id: "%1".into(),
            })
        );
        // The idle worktree row → a git-preview target.
        app.move_by(1);
        assert_eq!(
            app.preview_target_for_selection(),
            Some(PreviewTarget::Worktree {
                path: "/a/wt".into()
            })
        );
    }

    #[test]
    fn worktree_preview_renders_from_the_snapshot() {
        use crate::fetcher::PreviewTarget;
        let mut app = app_with(vec![], vec![idle_wt("/a/wt", "feat-x", "/a/.git", "alpha")]);
        app.show_preview = true;
        app.preview = Some((
            PreviewTarget::Worktree {
                path: "/a/wt".into(),
            },
            "abc123 fix the api".into(),
        ));
        // Drawing does no tmux/git work — the text comes from the snapshot.
        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("abc123 fix the api"), "preview text rendered");
        assert!(text.contains("feat-x"), "worktree preview titled by branch");
    }

    /// Render the app into a `w`×`h` in-memory terminal; one String per screen row.
    fn render_lines(app: &mut App, w: u16, h: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let buf = terminal.backend().buffer().clone();
        (0..h)
            .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
            .collect()
    }

    /// The user's real layout: session-per-worktree names, long branches, and a folder
    /// that holds a different branch than its name says.
    fn long_name_app() -> App {
        let mut a = agent(
            "%1",
            Status::Working,
            3,
            Some((
                "/svc/.git",
                "cet-services",
                "CET-3200/feat/cet-3723-partner-service",
                "/w/tree/CET-3200-feat-cet-3723-partner-identity",
            )),
        );
        a.pane.session_name = "cet-services-CET-3200-feat-cet-3723-partner-service".into();
        let mut app = app_with(
            vec![a],
            vec![idle_wt(
                "/w/tree/CET-3200-feat-cet-3723-partner-service",
                "CET-3200/feat/cet-3723-partner-rpcs",
                "/svc/.git",
                "cet-services",
            )],
        );
        app.all_sessions = true;
        app.show_preview = false;
        app
    }

    #[test]
    fn long_branch_names_render_in_full_at_a_reasonable_width() {
        let mut app = long_name_app();
        let text = render_lines(&mut app, 160, 12).join("\n");
        assert!(
            text.contains("CET-3200/feat/cet-3723-partner-service"),
            "agent branch in full:\n{text}"
        );
        assert!(
            text.contains("CET-3200/feat/cet-3723-partner-rpcs"),
            "idle branch in full:\n{text}"
        );
        // The session drops its redundant repo prefix (the row is under that header)
        // and keeps its window index.
        assert!(
            text.contains(" CET-3200-feat-cet-3723-partner-service:3 "),
            "{text}"
        );
        assert!(
            !text.contains("cet-services-CET"),
            "repo prefix dropped:\n{text}"
        );
    }

    #[test]
    fn a_folder_named_for_another_branch_is_shown() {
        let mut app = long_name_app();
        let text = render_lines(&mut app, 160, 12).join("\n");
        assert!(
            text.contains("· CET-3200-feat-cet-3723-partner-identity"),
            "agent row names its folder:\n{text}"
        );
        // The idle worktree's folder also doesn't match its branch.
        assert!(
            text.contains("rpcs · CET-3200-feat-cet-3723-partner-service"),
            "idle row names its folder:\n{text}"
        );
    }

    #[test]
    fn rows_stay_aligned_and_inside_a_narrow_list() {
        let mut app = long_name_app();
        let lines = render_lines(&mut app, 70, 12);
        for l in &lines {
            assert_eq!(l.chars().count(), 70);
        }
        let text = lines.join("\n");
        // The branch survives (possibly truncated) and "start ⏎" still fits.
        assert!(text.contains("CET-3200/feat/"), "{text}");
        assert!(text.contains("start ⏎"), "{text}");
    }

    #[test]
    fn full_names_are_readable_at_the_users_popup_size() {
        // 159-col client, 70% popup ≈ 111 cols, preview on: the list is ~53 cols wide, so
        // the row can't hold the 38-char branch plus its folder. The preview title must.
        let mut app = long_name_app();
        app.show_preview = true; // the agent row is selected (its preview is "…")
        let text = render_lines(&mut app, 111, 40).join("\n");
        assert!(
            text.contains("CET-3200/feat/cet-3723-partner-service"),
            "full branch somewhere:\n{text}"
        );
        assert!(
            text.contains("CET-3200-feat-cet-3723-partner-identity"),
            "full folder somewhere:\n{text}"
        );
        // And the row itself keeps more of the name than the old fixed 23 columns.
        assert!(text.contains("CET-3200/feat/cet-3723-p"), "{text}");
    }

    #[test]
    fn preview_title_shows_the_full_name_of_the_selection() {
        let mut app = long_name_app();
        app.show_preview = true;
        app.move_by(1); // the idle worktree (no tmux capture needed)
        let text = render_lines(&mut app, 200, 12).join("\n");
        assert!(
            text.contains(
                "preview · CET-3200/feat/cet-3723-partner-rpcs · CET-3200-feat-cet-3723-partner-service"
            ),
            "{text}"
        );
    }

    #[test]
    fn filter_finds_an_agent_by_its_folder_name() {
        let mut app = long_name_app();
        press(&mut app, KeyCode::Char('/'));
        for c in "partner-identity".chars() {
            press(&mut app, KeyCode::Char(c));
        }
        press(&mut app, KeyCode::Enter);
        assert_eq!(app.view.len(), 1, "the agent in that folder matches");
        assert!(app.idle_view.is_empty(), "the other worktree doesn't");
        assert_eq!(selected_key_of(&app).as_deref(), Some("%1"));
    }

    #[test]
    fn worktree_rows_show_merged_or_ahead_behind_badges() {
        let mut merged = idle_wt("/a/wt1", "feat-merged", "/a/.git", "alpha");
        merged.state = Some(BranchState::Merged); // its commits landed → safe to remove
        let mut diverged = idle_wt("/a/wt2", "feat-live", "/a/.git", "alpha");
        diverged.state = Some(BranchState::Diverged {
            ahead: 2,
            behind: 1,
        });
        let mut unknown = idle_wt("/a/wt3", "feat-unknown", "/a/.git", "alpha");
        unknown.state = None; // no badge at all
        let mut app = app_with(vec![], vec![merged, diverged, unknown]);
        app.show_preview = false;

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("merged"), "merged badge rendered");
        assert!(text.contains("↑2 ↓1"), "ahead/behind badge rendered");
    }

    #[test]
    fn a_branch_with_no_commits_is_not_badged_as_merged() {
        // The dangerous regression: a just-spawned worktree has no commits of its own,
        // and used to render `merged` — telling you the branch you're about to work on
        // had already landed.
        let mut fresh = idle_wt("/a/wt1", "feat-fresh", "/a/.git", "alpha");
        fresh.state = Some(BranchState::NoCommits);
        let mut app = app_with(vec![], vec![fresh]);
        app.show_preview = false;

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(text.contains("no commits"), "honest badge rendered");
        assert!(!text.contains("merged"), "must NOT claim the branch merged");
    }

    #[test]
    fn detached_worktree_rows_are_named_by_their_directory() {
        // Several detached PR checkouts of one repo used to render as identical
        // "(detached)" rows — no way to tell which is which, including in the `x`
        // remove confirmation.
        let mut a = idle_wt("/work/tree/pr-1462", "x", "/a/.git", "alpha");
        a.branch = None;
        a.state = Some(BranchState::Diverged {
            ahead: 12,
            behind: 76,
        });
        let mut b = idle_wt("/work/tree/pr-1543", "x", "/a/.git", "alpha");
        b.branch = None;
        b.state = Some(BranchState::Diverged {
            ahead: 3,
            behind: 32,
        });
        let mut app = app_with(vec![], vec![a, b]);
        app.show_preview = false;

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("pr-1462 (detached)"),
            "first row identifiable"
        );
        assert!(
            text.contains("pr-1543 (detached)"),
            "second row identifiable"
        );
        assert!(
            text.contains("↑12 ↓76"),
            "detached worktrees get a badge too"
        );
        assert!(text.contains("↑3 ↓32"));
    }

    #[test]
    fn title_shows_the_scope_label_from_the_snapshot() {
        // Agent lives in session "proj", but the snapshot's scope is the repo — the
        // header must reflect the repo name, not the agent's session.
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        app.scope_label = "cet-services".into();
        app.show_preview = false;

        let mut terminal = Terminal::new(TestBackend::new(90, 24)).unwrap();
        terminal.draw(|f| app.draw(f)).unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|c| c.symbol())
            .collect();
        assert!(
            text.contains("cet-services"),
            "header shows the scope label"
        );
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
    fn picks_up_to_9_route_to_respond() {
        // Claude Code dialogs can offer more than three options; every digit must
        // reach `respond` (proved here by hitting its state-file gate, not `_ => {}`).
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::NeedsInput,
                4,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        press(&mut app, KeyCode::Char('9'));
        assert_eq!(
            app.message.as_deref(),
            Some("win 4 is no longer waiting for input")
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
    fn c_resumes_only_a_selected_idle_worktree() {
        // 'c' on an idle worktree starts it with the resume command.
        let mut app = app_with(vec![], vec![idle_wt("/a/wt", "feat-x", "/a/.git", "alpha")]);
        assert_eq!(
            press(&mut app, KeyCode::Char('c')),
            Action::Start { resume: true }
        );
        // On an agent row there's nothing to resume — 'c' is a no-op.
        let mut app = app_with(
            vec![agent(
                "%1",
                Status::Idle,
                1,
                Some(("/a/.git", "alpha", "f1", "/a1")),
            )],
            vec![],
        );
        assert_eq!(press(&mut app, KeyCode::Char('c')), Action::None);
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
    fn parse_spawn_input_splits_name_and_optional_prompt() {
        let name = |n| SpawnTarget::Name(n);
        assert_eq!(parse_spawn_input("feat-x"), (name("feat-x"), None));
        assert_eq!(
            parse_spawn_input("feat-x: fix the api tests"),
            (name("feat-x"), Some("fix the api tests"))
        );
        // Only the FIRST colon splits; the prompt may contain more.
        assert_eq!(
            parse_spawn_input("feat-x: update README: add usage"),
            (name("feat-x"), Some("update README: add usage"))
        );
        // Whitespace around both parts is trimmed; an empty prompt is no prompt.
        assert_eq!(parse_spawn_input("  feat-x :  "), (name("feat-x"), None));
    }

    #[test]
    fn parse_spawn_input_recognizes_a_pull_request() {
        let pr = |number| {
            SpawnTarget::Pr(crate::pr::PrRef {
                repo: Some(("guimochila".into(), "hydra".into())),
                number,
            })
        };
        // The whole point: the `:` in `https://` must not split the line.
        assert_eq!(
            parse_spawn_input("https://github.com/guimochila/hydra/pull/36"),
            (pr(36), None)
        );
        assert_eq!(
            parse_spawn_input("https://github.com/guimochila/hydra/pull/36: review it"),
            (pr(36), Some("review it"))
        );
        assert_eq!(
            parse_spawn_input("https://github.com/guimochila/hydra/pull/36/files review it"),
            (pr(36), Some("review it"))
        );
        assert_eq!(
            parse_spawn_input("#36: review it"),
            (
                SpawnTarget::Pr(crate::pr::PrRef {
                    repo: None,
                    number: 36
                }),
                Some("review it")
            )
        );
    }

    #[test]
    fn agent_command_with_prompt_quotes_for_the_shell() {
        assert_eq!(agent_command_with_prompt("claude", None), "claude");
        assert_eq!(
            agent_command_with_prompt("claude", Some("fix the api")),
            "claude 'fix the api'"
        );
        // Single quotes inside the prompt must survive shell quoting.
        assert_eq!(
            agent_command_with_prompt("claude", Some("don't break")),
            r#"claude 'don'\''t break'"#
        );
    }

    #[test]
    fn sanitize_makes_a_safe_path_segment() {
        assert_eq!(sanitize("feat/pagination api"), "feat-pagination-api");
        assert_eq!(sanitize("simple"), "simple");
    }

    #[test]
    fn session_name_is_repo_scoped_branch_then_basename_and_tmux_safe() {
        // Repo prefix disambiguates same-branch worktrees across different repos.
        assert_eq!(session_name("hydra", Some("main"), "/a/wt"), "hydra-main");
        assert_eq!(
            session_name("cet-services", Some("main"), "/b/wt"),
            "cet-services-main"
        );
        // Branch preferred over path; `/`, `.`, `:`, whitespace all become `-`.
        assert_eq!(
            session_name("repo", Some("feature/foo.bar"), "/x/wt"),
            "repo-feature-foo-bar"
        );
        assert_eq!(session_name("repo", Some("a b:c"), "/x"), "repo-a-b-c");
        // No branch -> final path segment, still repo-scoped.
        assert_eq!(session_name("repo", None, "/root/wt-a"), "repo-wt-a");
        assert_eq!(session_name("repo", None, "/root/wt-a/"), "repo-wt-a");
        // An unsafe char in the repo name is normalized too.
        assert_eq!(session_name("my.repo", Some("x"), "/p"), "my-repo-x");
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
        let lines = app.footer(200);
        assert_eq!(lines.len(), 1, "wide footer fits on one line");
        let spans = &lines[0].spans;
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
    fn keybar_wraps_on_narrow_widths_without_dropping_keys() {
        let app = App::default();
        let narrow = app.footer(40);
        assert!(narrow.len() > 1, "40 cols can't fit the whole keybar");
        // Every key and label still appears somewhere.
        let text: String = narrow
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.clone())
            .collect();
        for (k, l) in KEYBAR {
            assert!(text.contains(k), "key {k} present");
            assert!(text.contains(l), "label {l} present");
        }
        // No rendered row exceeds the width it was packed for.
        for line in &narrow {
            assert!(line.width() <= 40, "row '{line}' fits in 40 cols");
        }
    }

    #[test]
    fn pack_keybar_terminates_and_preserves_order_at_pathological_widths() {
        let rows = pack_keybar(KEYBAR, 1); // narrower than any single pair
        assert_eq!(rows.len(), KEYBAR.len(), "one pair per row");
        let flat: Vec<_> = rows.iter().flat_map(|r| r.iter()).collect();
        assert_eq!(flat.len(), KEYBAR.len());
        assert_eq!(*flat[0], KEYBAR[0]);
        assert_eq!(*flat[flat.len() - 1], KEYBAR[KEYBAR.len() - 1]);
    }

    #[test]
    fn active_filter_indicator_survives_wrapping() {
        let app = App {
            filter: "alpha".into(),
            ..App::default()
        };
        let lines = app.footer(40);
        let text: String = lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.clone())
            .collect();
        assert!(text.contains("filter: alpha"));
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

    #[test]
    fn session_spawn_mode_starts_in_all_sessions_view() {
        let win = crate::config::Config::parse("");
        let sess = crate::config::Config::parse("[agent]\nspawn_mode = \"session\"\n");
        assert!(!starts_all_sessions(&win));
        assert!(starts_all_sessions(&sess));
    }
}
