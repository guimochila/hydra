//! The popup TUI: a live, vim-navigated, repo-grouped list of the session's agents.
//!
//! Refreshes on a 250 ms tick (re-reads state files + `tmux list-panes`), which is
//! real-time enough for a popup and avoids a filesystem-watch dependency. Enter jumps
//! to the selected agent's window and exits so the `-E` popup closes on the agent.
//!
//! Rows are either a repo header or an agent; navigation skips headers, and selection
//! is tracked by pane id so it sticks to the same agent as the list reorders.

use crate::agent::{self, Agent};
use crate::state::Status;
use crate::tmux;
use crate::worktree::Caches;
use std::time::{SystemTime, UNIX_EPOCH};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind};
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
    let mut terminal = ratatui::init();
    let mut app = App {
        show_preview: true,
        ..App::default()
    };
    let result = app.run(&mut terminal);
    ratatui::restore();
    result
}

enum Action {
    None,
    Quit,
    Jump,
}

/// A quick reply to a pending prompt.
enum Response {
    /// Accept the highlighted default (send Enter).
    Approve,
    /// Reject / cancel the prompt (send Escape).
    Deny,
}

/// A rendered line: a repo header, or an agent at `view` index.
enum Row {
    Header { label: String, count: usize },
    Agent(usize),
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
}

#[derive(Default)]
struct App {
    caches: Caches,
    /// Whether the preview pane is shown.
    show_preview: bool,
    /// All agents this tick (status-sorted), before filtering.
    agents: Vec<Agent>,
    /// Agents passing the current filter — what `Row::Agent` indexes into.
    view: Vec<Agent>,
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
    /// Pane id of the selected agent, so selection survives reordering/rebuilds.
    selected_pane: Option<String>,
    /// Transient status line (e.g. "✓ approved win 4"); cleared on the next keypress.
    message: Option<String>,
}

impl App {
    fn run<B: ratatui::backend::Backend>(
        &mut self,
        terminal: &mut Terminal<B>,
    ) -> std::io::Result<()> {
        loop {
            self.fetch();
            self.rebuild_rows();
            terminal.draw(|f| self.draw(f))?;

            if event::poll(Duration::from_millis(250))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind != KeyEventKind::Press {
                        continue;
                    }
                    match self.handle_key(key.code) {
                        Action::Quit => break,
                        Action::Jump => {
                            self.jump()?;
                            break;
                        }
                        Action::None => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Re-read agents from disk + tmux (the expensive step). Filtering is separate.
    fn fetch(&mut self) {
        self.agents = crate::current_agents(&mut self.caches);
    }

    /// Rebuild the filtered view and the header/agent rows from `self.agents`, then
    /// restore the selection onto the same agent (by pane id) where possible.
    fn rebuild_rows(&mut self) {
        self.view = self
            .agents
            .iter()
            .filter(|a| agent::matches_filter(a, &self.filter))
            .cloned()
            .collect();

        self.rows.clear();
        for group in agent::group_by_repo(&self.view) {
            self.rows.push(Row::Header {
                label: group.label,
                count: group.indices.len(),
            });
            for idx in group.indices {
                self.rows.push(Row::Agent(idx));
            }
        }

        self.select_pane(self.selected_pane.clone());
    }

    /// Point the selection at the agent with `pane`, else the first agent row.
    fn select_pane(&mut self, pane: Option<String>) {
        let target = pane.and_then(|p| {
            self.rows.iter().position(|r| match r {
                Row::Agent(i) => self
                    .view
                    .get(*i)
                    .map(|a| a.pane.pane_id == p)
                    .unwrap_or(false),
                Row::Header { .. } => false,
            })
        });
        let row = target.or_else(|| self.first_agent_row());
        self.list_state.select(row);
        self.selected_pane = self.selected_agent().map(|a| a.pane.pane_id.clone());
    }

    fn first_agent_row(&self) -> Option<usize> {
        self.rows.iter().position(|r| matches!(r, Row::Agent(_)))
    }

    fn last_agent_row(&self) -> Option<usize> {
        self.rows.iter().rposition(|r| matches!(r, Row::Agent(_)))
    }

    fn selected_agent(&self) -> Option<&Agent> {
        match self.list_state.selected().and_then(|r| self.rows.get(r)) {
            Some(Row::Agent(i)) => self.view.get(*i),
            _ => None,
        }
    }

    fn handle_key(&mut self, code: KeyCode) -> Action {
        match self.mode {
            Mode::Filter => {
                self.handle_filter_key(code);
                Action::None
            }
            Mode::Send => {
                self.handle_send_key(code);
                Action::None
            }
            Mode::Spawn => {
                self.handle_spawn_key(code);
                Action::None
            }
            Mode::Normal => self.handle_normal_key(code),
        }
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
            KeyCode::Char('G') => self.set_selected_row(self.last_agent_row()),
            KeyCode::Char('g') => {
                if g_was_pending {
                    self.set_selected_row(self.first_agent_row());
                } else {
                    self.pending_g = true;
                }
            }
            KeyCode::Char('r') => {
                self.fetch();
                self.rebuild_rows();
            }
            KeyCode::Char('p') => self.show_preview = !self.show_preview,
            KeyCode::Char('n') => {
                self.spawn_input.clear();
                self.mode = Mode::Spawn;
            }
            KeyCode::Tab => self.select_next_needs_input(),
            // Interaction (Phase 3): approve/deny a pending prompt, or compose a message.
            KeyCode::Char('a') => self.respond(Response::Approve),
            KeyCode::Char('d') => self.respond(Response::Deny),
            KeyCode::Char('i') => {
                if self.selected_agent().is_some() {
                    self.send_input.clear();
                    self.mode = Mode::Send;
                }
            }
            KeyCode::Enter if self.selected_agent().is_some() => return Action::Jump,
            _ => {}
        }
        Action::None
    }

    fn handle_filter_key(&mut self, code: KeyCode) {
        match code {
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

    fn handle_send_key(&mut self, code: KeyCode) {
        match code {
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

    fn handle_spawn_key(&mut self, code: KeyCode) {
        match code {
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
    /// existing agent (selected, else first) to infer the repo, socket and session —
    /// so spawning needs at least one agent already present to anchor the project.
    fn spawn_agent(&mut self, name: &str) {
        let Some((socket, session, cwd)) = self.spawn_context() else {
            self.message = Some("spawn needs an existing agent to anchor the repo".into());
            return;
        };
        let path = worktree_root().join(sanitize(name));
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
            "claude",
        ) {
            Ok(()) => Some(format!("✓ spawned {name}")),
            Err(e) => Some(format!("window failed: {e}")),
        };
    }

    /// Socket/session/cwd of the agent used to anchor a spawn (selected, else first).
    fn spawn_context(&self) -> Option<(String, String, String)> {
        let a = self.selected_agent().or_else(|| self.view.first())?;
        Some((
            a.state.socket.clone(),
            a.pane.session_name.clone(),
            a.pane.cwd.clone(),
        ))
    }

    /// Approve (accept the highlighted default) or deny (Escape) a pending prompt on the
    /// selected agent. Only acts when that agent is actually waiting for input, so a
    /// stray keypress can't submit an Enter to a busy or idle agent.
    fn respond(&mut self, response: Response) {
        let Some((socket, pane, window, status)) = self.selected_target() else {
            return;
        };
        if status != Status::NeedsInput {
            self.message = Some(format!("win {window} isn't waiting for input"));
            return;
        }
        let (key, verb) = match response {
            Response::Approve => ("Enter", "approved"),
            Response::Deny => ("Escape", "denied"),
        };
        self.message = Some(match tmux::send_key(&socket, &pane, key) {
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

    /// Move selection by `delta` agent rows (skipping headers).
    fn move_by(&mut self, delta: isize) {
        let agent_rows: Vec<usize> = self
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| matches!(r, Row::Agent(_)))
            .map(|(i, _)| i)
            .collect();
        if agent_rows.is_empty() {
            return;
        }
        let cur_row = self.list_state.selected().unwrap_or(agent_rows[0]);
        let cur_pos = agent_rows.iter().position(|&r| r == cur_row).unwrap_or(0) as isize;
        let next_pos = (cur_pos + delta).clamp(0, agent_rows.len() as isize - 1) as usize;
        self.set_selected_row(Some(agent_rows[next_pos]));
    }

    fn set_selected_row(&mut self, row: Option<usize>) {
        self.list_state.select(row);
        self.selected_pane = self.selected_agent().map(|a| a.pane.pane_id.clone());
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
                Row::Header { .. } => false,
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
            let msg = if self.filter.is_empty() {
                "  no Claude Code agents in this session"
            } else {
                "  no agents match filter"
            };
            let empty = List::new([ListItem::new(Line::from(Span::raw(msg).dim()))]).block(block);
            frame.render_widget(empty, list_area);
        } else {
            let now = now_secs();
            let items: Vec<ListItem> = self
                .rows
                .iter()
                .map(|row| match row {
                    Row::Header { label, count } => header_row(label, *count),
                    Row::Agent(i) => agent_row(&self.view[*i], now),
                })
                .collect();
            let list = List::new(items).block(block).highlight_style(
                Style::default()
                    .bg(Color::Rgb(50, 50, 60))
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
        // Show the tail of the agent's visible screen (the most recent output/prompt).
        let content = tmux::capture_pane(&a.state.socket, &a.pane.pane_id);
        let rows = area.height.saturating_sub(2) as usize;
        let tail: Vec<Line> = content
            .lines()
            .rev()
            .take(rows)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|l| Line::from(Span::raw(l.to_string())))
            .collect();
        let para = ratatui::widgets::Paragraph::new(tail).block(block);
        frame.render_widget(para, area);
    }

    fn title(&self) -> String {
        let session = self
            .agents
            .first()
            .map(|a| a.pane.session_name.clone())
            .or_else(|| tmux::current_socket().and_then(|s| tmux::current_session(&s)))
            .unwrap_or_else(|| "?".into());
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
            Mode::Normal => {
                // A transient action result takes over the footer until the next key.
                if let Some(msg) = &self.message {
                    return Line::from(Span::styled(
                        format!(" {msg}"),
                        Style::default().fg(Color::Yellow),
                    ));
                }
                let mut spans = vec![
                    Span::raw(" j/k ").dim(),
                    Span::raw("move  "),
                    Span::raw("⏎ ").dim(),
                    Span::raw("jump  "),
                    Span::raw("a ").dim(),
                    Span::raw("✓  "),
                    Span::raw("d ").dim(),
                    Span::raw("✗  "),
                    Span::raw("i ").dim(),
                    Span::raw("send  "),
                    Span::raw("n ").dim(),
                    Span::raw("new  "),
                    Span::raw("⇥ ").dim(),
                    Span::raw("next⚠  "),
                    Span::raw("/ ").dim(),
                    Span::raw("filter  "),
                    Span::raw("p ").dim(),
                    Span::raw("preview  "),
                    Span::raw("q ").dim(),
                    Span::raw("quit"),
                ];
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

fn header_row(label: &str, count: usize) -> ListItem<'static> {
    let line = Line::from(vec![
        Span::styled(
            format!("▸ {label}"),
            Style::default()
                .fg(Color::Blue)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(format!(" ({count})")).dim(),
    ]);
    ListItem::new(line)
}

fn agent_row(a: &Agent, now: u64) -> ListItem<'static> {
    let (color, glyph) = match a.effective_status {
        Status::NeedsInput => (Color::Yellow, a.effective_status.glyph()),
        Status::Working => (Color::Green, a.effective_status.glyph()),
        Status::Idle => (Color::Gray, a.effective_status.glyph()),
        Status::Unknown => (Color::DarkGray, a.effective_status.glyph()),
    };

    let branch = a
        .worktree
        .as_ref()
        .and_then(|w| w.branch.clone())
        .unwrap_or_else(|| a.pane.window_name.clone());

    let age = agent::format_age(now.saturating_sub(a.state.updated_at));
    let summary = a.state.task_summary.clone().unwrap_or_default();

    let mut spans = vec![
        Span::raw("  "),
        Span::styled(
            glyph.to_string(),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {age:>3} "), Style::default().fg(color)),
        Span::raw(format!("win {:>2}  ", a.pane.window_index)),
        Span::styled(format!("{branch:<24}"), Style::default().fg(Color::Cyan)),
    ];
    if a.dirty > 0 {
        spans.push(Span::styled(
            format!("Δ{} ", a.dirty),
            Style::default().fg(Color::Magenta),
        ));
    }
    spans.push(Span::raw(summary).dim());
    ListItem::new(Line::from(spans))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Directory new worktrees are created under. `$HYDRA_WORKTREE_ROOT` overrides the
/// default of `~/work/tree`.
fn worktree_root() -> std::path::PathBuf {
    if let Ok(root) = std::env::var("HYDRA_WORKTREE_ROOT") {
        if !root.is_empty() {
            return std::path::PathBuf::from(root);
        }
    }
    dirs::home_dir()
        .unwrap_or_default()
        .join("work")
        .join("tree")
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

    #[test]
    fn sanitize_makes_a_safe_path_segment() {
        assert_eq!(sanitize("feat/pagination api"), "feat-pagination-api");
        assert_eq!(sanitize("simple"), "simple");
    }
}
