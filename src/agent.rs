//! Correlate reported agent state with live tmux panes to produce the display list.
//!
//! The state files say *what each agent is doing* and *where it claims to live*;
//! `tmux list-panes` says *whether that pane still exists and its current window*.
//! Joining on `pane_id` means a dead agent's leftover file matches nothing and simply
//! drops out — no ghost rows, no separate liveness tracking.

use crate::state::{AgentState, Status};
use crate::tmux::Pane;
use crate::worktree::{IdleWorktree, ProjectWorktrees, WorktreeInfo};

/// A working agent that is joined to a live pane and ready to display.
#[derive(Debug, Clone)]
pub struct Agent {
    pub state: AgentState,
    pub pane: Pane,
    /// Status after applying the staleness rule (see `join_and_sort`).
    pub effective_status: Status,
    pub worktree: Option<WorktreeInfo>,
    /// Count of uncommitted changes in the worktree (throttled; 0 if unknown/clean).
    pub dirty: usize,
}

/// A working agent whose `WORKING` status hasn't refreshed in this many seconds is
/// shown as `UNKNOWN` (likely crashed). Idle/NeedsInput agents can legitimately sit
/// for a long time, so staleness only applies to `WORKING`. This is also
/// `Timings::stale_after_secs`'s default; callers now go through `config::load()`, so
/// this constant is only reachable from tests (hence the `allow`).
#[allow(dead_code)]
pub const STALE_AFTER_SECS: u64 = 900;

/// Pure core: join `states` (already for one socket) against live `panes`, keep only
/// those in `session_name` (`None` = every session on the socket), apply staleness,
/// and sort (NeedsInput first, then by window index). Worktree is left unresolved
/// here so this stays IO-free and testable.
pub fn join_and_sort(
    states: Vec<AgentState>,
    panes: &[Pane],
    session_name: Option<&str>,
    now: u64,
    stale_after: u64,
) -> Vec<Agent> {
    let mut agents: Vec<Agent> = states
        .into_iter()
        .filter_map(|state| {
            let pane = panes.iter().find(|p| p.pane_id == state.pane_id)?.clone();
            if session_name.is_some_and(|name| pane.session_name != name) {
                return None;
            }
            let effective_status =
                effective_status(state.status, state.updated_at, now, stale_after);
            Some(Agent {
                state,
                pane,
                effective_status,
                worktree: None,
                dirty: 0,
            })
        })
        .collect();

    agents.sort_by(|a, b| {
        status_rank(a.effective_status)
            .cmp(&status_rank(b.effective_status))
            .then(a.pane.window_index.cmp(&b.pane.window_index))
    });
    agents
}

/// Grace period before a dead agent's leftover state file is deleted. Generous on
/// purpose: leftovers are invisible anyway (the join hides them), so GC is pure disk
/// hygiene and must never race a pane that is briefly absent.
pub const GC_GRACE_SECS: u64 = 3600;

/// State files whose pane no longer exists on this socket AND whose last update is
/// older than `grace_secs` — safe to delete. Returns their (socket, pane_id) keys.
/// `states` must already be filtered to the socket `panes` was listed from.
pub fn dead_states(
    states: &[AgentState],
    panes: &[Pane],
    now: u64,
    grace_secs: u64,
) -> Vec<(String, String)> {
    states
        .iter()
        .filter(|s| !panes.iter().any(|p| p.pane_id == s.pane_id))
        .filter(|s| now.saturating_sub(s.updated_at) > grace_secs)
        .map(|s| (s.socket.clone(), s.pane_id.clone()))
        .collect()
}

/// Candidate anchor directories for idle-worktree discovery: each agent's worktree
/// root in display order, then the popup's own cwd. Order matters — the caller
/// dedupes by repo identity taking the first anchor, so idle-only repos (reachable
/// just via the popup cwd) group after the repos that hold agents.
pub fn idle_anchors(agents: &[Agent], popup_cwd: Option<&str>) -> Vec<String> {
    let mut anchors: Vec<String> = agents
        .iter()
        .filter_map(|a| a.worktree.as_ref().map(|w| w.root.clone()))
        .collect();
    if let Some(cwd) = popup_cwd {
        anchors.push(cwd.to_string());
    }
    anchors
}

/// The per-row detail text: while the agent needs input, the live attention message
/// (why it's blocked — more actionable than the prompt that got it there); otherwise
/// the last task summary.
pub fn detail_text(a: &Agent) -> Option<String> {
    if a.effective_status == Status::NeedsInput {
        if let Some(attention) = &a.state.attention {
            return Some(one_line(attention));
        }
    }
    a.state.task_summary.as_deref().map(one_line)
}

/// Flatten to a single display line: newlines and runs of whitespace collapse to one
/// space each. Summaries can be multi-line (a tool-result blob, a notification payload
/// with embedded markup), and every consumer of this column renders one line — a
/// ratatui `Span` can't show a newline, and in `hydra ls` it would break the columns
/// of every row after it.
pub fn one_line(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Truncate to at most `max` chars (char-boundary safe), adding an ellipsis. Shared
/// by the hook (task summaries / attention) and the row renderer (width fitting).
pub fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Format an age in seconds compactly: `12s`, `4m`, `2h`, `3d`.
pub fn format_age(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m", secs / 60)
    } else if secs < 86_400 {
        format!("{}h", secs / 3600)
    } else {
        format!("{}d", secs / 86_400)
    }
}

/// Canonical worktree roots that have a live agent — the occupancy set for `idle_from`.
/// Deliberately spans EVERY session on the socket, not just the displayed (possibly
/// session-filtered) agents: in session spawn mode an agent lives in its own session, and
/// its worktree must not be offered as "idle" just because the current view is scoped to a
/// different session. Pure; `project.entries` are already canonical so we canonicalize here.
pub fn occupied_roots(agents: &[Agent]) -> std::collections::HashSet<String> {
    agents
        .iter()
        .filter_map(|a| a.worktree.as_ref().map(|w| canon(&w.root)))
        .collect()
}

/// Worktrees of the project that have no agent running in them. `occupied` is the set of
/// canonical roots from `occupied_roots`; `project.entries` paths are already canonical.
pub fn idle_from(
    occupied: &std::collections::HashSet<String>,
    project: &ProjectWorktrees,
) -> Vec<IdleWorktree> {
    project
        .entries
        .iter()
        .filter(|(path, _)| !occupied.contains(path))
        .map(|(path, branch)| IdleWorktree {
            path: path.clone(),
            branch: branch.clone(),
            repo_key: project.repo_key.clone(),
            repo_name: project.repo_name.clone(),
            state: None, // filled by the caller from its throttled cache
        })
        .collect()
}

/// The name to show for an idle worktree. Normally its branch; a detached worktree has
/// no branch, so fall back to its directory basename — the only identity it has, and the
/// thing that tells several detached PR checkouts of one repo apart (a bare "(detached)"
/// renders every one of them as the same unidentifiable row).
pub fn worktree_label(wt: &IdleWorktree) -> String {
    if let Some(branch) = &wt.branch {
        return branch.clone();
    }
    match basename(&wt.path) {
        Some(name) => format!("{name} (detached)"),
        None => "(detached)".to_string(),
    }
}

/// The worktree directory's name when it tells the user something the branch doesn't:
/// a folder whose name isn't just the branch with `/`/whitespace turned into `-` (the
/// way hydra names the folders it creates). A worktree checked out into a folder named
/// for a *different* branch is otherwise unrecognizable — the folder the user sees on
/// disk appears nowhere in the row. `None` for a detached worktree (its label already
/// is the folder) and for the repo's main checkout (the folder is the repo name, which
/// the group header already shows).
pub fn folder_hint(branch: Option<&str>, path: &str, repo_name: &str) -> Option<String> {
    let branch = branch?;
    let folder = basename(path)?;
    if folder == repo_name || folder == path_safe(branch) {
        return None;
    }
    Some(folder.to_string())
}

/// Replace `/`, whitespace, and any char in `also` with `-`. The single source of the
/// name-sanitizing rules: `ui::sanitize` (worktree folder leaf, `also = []`) and
/// `session_name` (`also = ['.', ':']`, tmux target separators) both use it, and so do
/// `folder_hint`/`display_session`, which must recognize the names those produce.
pub fn replace_unsafe(name: &str, also: &[char]) -> String {
    name.chars()
        .map(|c| {
            if c == '/' || c.is_whitespace() || also.contains(&c) {
                '-'
            } else {
                c
            }
        })
        .collect()
}

/// Branch → folder-name form, exactly how a spawned worktree's directory is named.
pub fn path_safe(name: &str) -> String {
    replace_unsafe(name, &[])
}

/// Chars a tmux session name can't carry (target separators), on top of `/`/whitespace.
const SESSION_UNSAFE: [char; 2] = ['.', ':'];

/// The `<repo>-` prefix of a session-per-worktree name (see `session_name`).
fn session_prefix(repo: &str) -> String {
    replace_unsafe(&format!("{repo}-"), &SESSION_UNSAFE)
}

/// A tmux-safe, **repo-scoped** session name for a worktree: `<repo>-<leaf>`, where `leaf`
/// is the branch if present, else the path's final segment. The `<repo>` prefix (the repo
/// directory name) disambiguates worktrees that share a branch name across different repos,
/// so `Enter`/`n` can't collide on (or switch to) the wrong repo's session. `/`, whitespace,
/// `.` and `:` all become `-` — tmux treats `.`/`:` as target separators (`session:window.pane`),
/// so they must not appear in a name we later target.
pub fn session_name(repo: &str, branch: Option<&str>, path: &str) -> String {
    let leaf = branch.map(str::to_string).unwrap_or_else(|| {
        std::path::Path::new(path)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.to_string())
    });
    format!(
        "{}{}",
        session_prefix(repo),
        replace_unsafe(&leaf, &SESSION_UNSAFE)
    )
}

/// The session name as shown under its repo header: a session-per-worktree name is
/// `<repo>-<branch>` (see `session_name`), and the `<repo>-` half only repeats the
/// group header while eating the row. Display only — tmux targets keep the real name.
/// Returned unchanged when the prefix doesn't match or would leave nothing.
pub fn display_session<'a>(session: &'a str, repo_name: Option<&str>) -> &'a str {
    let Some(repo) = repo_name else {
        return session;
    };
    match session.strip_prefix(&session_prefix(repo)) {
        Some(rest) if !rest.is_empty() => rest,
        _ => session,
    }
}

/// Minimum widths the list row's cells shrink to before the trailing text gives way.
pub const ROW_PLACE_MIN: usize = 8;
pub const ROW_NAME_MIN: usize = 16;
/// Width the trailing cell (task summary / badge + "start ⏎") keeps while the place cell
/// shrinks toward `ROW_PLACE_MIN`.
pub const ROW_DETAIL_MIN: usize = 20;
/// Hard floor for the trailing cell: below `ROW_DETAIL_MIN` it yields to the name (how
/// the user recognizes a worktree) until it reaches this, and only then does the name
/// shrink toward `ROW_NAME_MIN`.
pub const ROW_DETAIL_FLOOR: usize = 8;

/// Widths of a list row's three variable cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowWidths {
    pub place: usize,
    pub name: usize,
    pub detail: usize,
}

/// Split `avail` columns between the place, name and trailing cells. Place and name get
/// what they want (the widest value in the column) when it fits alongside a
/// `ROW_DETAIL_MIN` trailing cell. Otherwise, in order: place shrinks (toward
/// `ROW_PLACE_MIN`), then the trailing cell (toward `ROW_DETAIL_FLOOR`) — the name is how
/// the user recognizes a worktree, so it gives way last — then the name (toward
/// `ROW_NAME_MIN`). On a terminal too narrow even for that, the trailing cell goes first
/// and place/name are clamped into what's left. Always `place + name + detail == avail`.
pub fn row_widths(avail: usize, place_want: usize, name_want: usize) -> RowWidths {
    let mut place = place_want;
    let mut name = name_want;
    let mut over = (place + name + ROW_DETAIL_MIN).saturating_sub(avail);
    let cut = over.min(place.saturating_sub(ROW_PLACE_MIN));
    place -= cut;
    over -= cut;
    over = over.saturating_sub(ROW_DETAIL_MIN - ROW_DETAIL_FLOOR);
    let cut = over.min(name.saturating_sub(ROW_NAME_MIN));
    name -= cut;
    place = place.min(avail);
    name = name.min(avail - place);
    RowWidths {
        place,
        name,
        detail: avail - place - name,
    }
}

/// Fit a `session:window` place into `width`, truncating the session but never the
/// `:window` suffix (the part that tells two windows of one session apart).
pub fn fit_place(session: &str, suffix: &str, width: usize) -> String {
    let full = format!("{session}{suffix}");
    if full.chars().count() <= width {
        return full;
    }
    let room = width.saturating_sub(suffix.chars().count());
    if room < 2 {
        return truncate(&full, width);
    }
    format!("{}{suffix}", truncate(session, room))
}

/// Fit a name cell (`label`, plus an optional folder `hint` rendered as ` · hint`) into
/// `width`. The label has priority: the hint is truncated (or dropped when there's no
/// useful room for it) before the label loses a character. Returns `(label, hint_part)`
/// where `hint_part` already carries its ` · ` separator, so the caller can style the
/// halves apart and their combined length is `<= width`.
pub fn fit_name(label: &str, hint: Option<&str>, width: usize) -> (String, String) {
    let label_len = label.chars().count();
    if label_len >= width {
        return (truncate(label, width), String::new());
    }
    let Some(hint) = hint else {
        return (label.to_string(), String::new());
    };
    let room = width - label_len;
    // " · " plus at least a few chars of the hint, or it's just noise.
    if room < 3 + 4 {
        return (label.to_string(), String::new());
    }
    (
        label.to_string(),
        format!(" · {}", truncate(hint, room - 3)),
    )
}

/// The preview pane's titles for a selection named `label` (+ folder `hint`), fitted to
/// `room` columns: `(top, bottom)`. The top title is `prefix + name`; `win` (an agent's
/// window index) adds a `win N · ` to the prefix only while the full name still fits
/// beside it. When label + hint don't fit on one line, the hint moves to a bottom title
/// (`folder <hint>`) instead of being cut, so the selection's full branch AND folder are
/// readable even when the list row had to truncate them.
pub fn preview_titles(
    win: Option<u32>,
    label: &str,
    hint: Option<&str>,
    room: usize,
) -> (String, Option<String>) {
    let short = "preview · ".to_string();
    let long = win.map_or_else(|| short.clone(), |w| format!("preview · win {w} · "));
    let len = |s: &str| s.chars().count();
    let hint_len = hint.map_or(0, |h| 3 + len(h));
    for prefix in [&long, &short] {
        if len(prefix) + len(label) + hint_len <= room {
            let (l, h) = fit_name(label, hint, room - len(prefix));
            return (format!("{prefix}{l}{h}"), None);
        }
    }
    let bottom = hint.map(|h| truncate(&format!("folder {h}"), room));
    let prefix = if len(&long) + len(label) <= room {
        &long
    } else {
        &short
    };
    let top = if len(prefix) + len(label) <= room {
        format!("{prefix}{label}")
    } else {
        truncate(label, room) // the name beats the "preview" word
    };
    (top, bottom)
}

/// An agent row's name cell: its branch (the tmux window name outside a worktree), plus
/// the worktree folder when it differs from the branch (see `folder_hint`).
pub fn agent_name(a: &Agent) -> (String, Option<String>) {
    let label = a
        .worktree
        .as_ref()
        .and_then(|w| w.branch.clone())
        .unwrap_or_else(|| a.pane.window_name.clone());
    let hint = a
        .worktree
        .as_ref()
        .and_then(|w| folder_hint(w.branch.as_deref(), &w.root, &w.repo_name));
    (label, hint)
}

/// An idle worktree row's name cell: `worktree_label` plus the folder hint.
pub fn worktree_name(wt: &IdleWorktree) -> (String, Option<String>) {
    (
        worktree_label(wt),
        folder_hint(wt.branch.as_deref(), &wt.path, &wt.repo_name),
    )
}

/// Last non-empty path segment, or `None` for a path with none (`"/"`, `""`).
pub fn basename(path: &str) -> Option<&str> {
    path.trim_end_matches('/')
        .rsplit('/')
        .find(|s| !s.is_empty())
}

/// Filter predicate for an idle worktree (branch / repo / path — the path covers the
/// folder name, which can differ from the branch).
pub fn worktree_matches_filter(wt: &IdleWorktree, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    let branch = wt.branch.as_deref().unwrap_or("");
    [branch, wt.repo_name.as_str(), wt.path.as_str()]
        .iter()
        .any(|field| field.to_lowercase().contains(&q))
}

fn canon(path: &str) -> String {
    std::fs::canonicalize(path)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string())
}

/// Which subset of the socket's agents the display shows. `Repo` compares each
/// agent's resolved `worktree.repo_key`, so agents must already have their worktree
/// resolved before filtering by it; `Session` and `All` need no worktree.
pub enum Scope<'a> {
    /// Only agents in this tmux session — the fallback when the popup's cwd isn't in
    /// a repo (so there's no repo identity to scope by).
    Session(&'a str),
    /// Agents whose worktree shares this repo identity, across every session on the
    /// socket. The default view when the popup opens inside a repo/worktree.
    Repo(&'a str),
    /// Every agent on the socket, regardless of session or repo.
    All,
}

/// Pick the display scope from the `s`-toggle state and the popup's context. The
/// default (toggle off) is repo-scoped when the popup's cwd resolves to a repo, so a
/// worktree sees the whole repo's agents across sessions; it falls back to the current
/// session only when there's no repo identity to scope by. The toggle on means the
/// whole socket.
pub fn choose_scope<'a>(
    all_sessions: bool,
    popup_repo_key: Option<&'a str>,
    session: &'a str,
) -> Scope<'a> {
    match (all_sessions, popup_repo_key) {
        (true, _) => Scope::All,
        (false, Some(key)) => Scope::Repo(key),
        (false, None) => Scope::Session(session),
    }
}

/// Whether `agent` belongs in the given display `scope`. A worktree-less agent never
/// matches a `Repo` scope (no `repo_key` to compare).
pub fn matches_scope(agent: &Agent, scope: &Scope) -> bool {
    match scope {
        Scope::Session(name) => agent.pane.session_name == *name,
        Scope::Repo(key) => agent.worktree.as_ref().is_some_and(|w| w.repo_key == *key),
        Scope::All => true,
    }
}

/// Case-insensitive substring match against branch, repo, task summary, window name,
/// session name and the worktree's folder name (which can differ from the branch).
/// An empty query matches everything.
pub fn matches_filter(agent: &Agent, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }
    let q = query.to_lowercase();
    let branch = agent
        .worktree
        .as_ref()
        .and_then(|w| w.branch.as_deref())
        .unwrap_or("");
    let repo = agent
        .worktree
        .as_ref()
        .map(|w| w.repo_name.as_str())
        .unwrap_or("");
    let summary = agent.state.task_summary.as_deref().unwrap_or("");
    let window = agent.pane.window_name.as_str();
    let session = agent.pane.session_name.as_str();
    let folder = agent
        .worktree
        .as_ref()
        .and_then(|w| basename(&w.root))
        .unwrap_or("");
    [branch, repo, summary, window, session, folder]
        .iter()
        .any(|field| field.to_lowercase().contains(&q))
}

fn effective_status(status: Status, updated_at: u64, now: u64, stale_after: u64) -> Status {
    if status == Status::Working && now.saturating_sub(updated_at) > stale_after {
        Status::Unknown
    } else {
        status
    }
}

fn status_rank(s: Status) -> u8 {
    match s {
        Status::NeedsInput => 0,
        Status::Working => 1,
        Status::Idle => 2,
        Status::Unknown => 3,
    }
}

/// Every `(session, window_index)` whose pane cwd is rooted in `path` — equal to `path`
/// or under it on a path boundary, so `/r/wt-a` never matches `/r/wt-ab`. Deduplicated,
/// so a window with several panes is returned once. Used by worktree removal to tear
/// down all windows a worktree occupies: in session spawn mode that is the shell + agent
/// windows (killing them empties and destroys the dedicated session); in window mode it
/// is just the single agent window.
pub fn windows_under_path(panes: &[Pane], path: &str) -> Vec<(String, u32)> {
    windows_under_path_except(panes, path, &[])
}

/// Whether `cwd` is `root` or under it on a path boundary.
fn is_under(cwd: &str, root: &str) -> bool {
    cwd == root
        || cwd
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/') || root.ends_with('/'))
}

/// `windows_under_path`, minus panes that belong to a *more specific* worktree nested
/// inside `path` (any of `others` strictly under it — `<repo>/.worktrees/x`, Claude Code's
/// `<repo>/.claude/worktrees/x`): by longest-prefix match those panes are that worktree's,
/// not `path`'s.
fn windows_under_path_except(panes: &[Pane], path: &str, others: &[String]) -> Vec<(String, u32)> {
    let nested: Vec<&str> = others
        .iter()
        .map(|o| o.trim_end_matches('/'))
        .filter(|o| *o != path.trim_end_matches('/') && is_under(o, path))
        .collect();
    let mut out: Vec<(String, u32)> = Vec::new();
    for p in panes {
        if is_under(&p.cwd, path) && !nested.iter().any(|n| is_under(&p.cwd, n)) {
            let key = (p.session_name.clone(), p.window_index);
            if !out.contains(&key) {
                out.push(key);
            }
        }
    }
    out
}

/// The live (session, window) of the pane `pane_id`, resolved against `panes` — the
/// listing taken right before a kill. A window *index* is not a stable id: with
/// `renumber-windows on` it shifts whenever a lower window closes, so a (session, index)
/// captured when the user pressed a key can name an unrelated window by the time they
/// confirm. The pane id is stable; `None` when the pane is gone.
pub fn own_window(panes: &[Pane], pane_id: Option<&str>) -> Option<(String, u32)> {
    let id = pane_id?;
    panes
        .iter()
        .find(|p| p.pane_id == id)
        .map(|p| (p.session_name.clone(), p.window_index))
}

/// Push `own` onto `wins` unless already there, then sort top window first (with
/// `renumber-windows on`, killing a low index renumbers the higher windows down and
/// invalidates the indices still held, so the top window always goes first).
fn with_own_top_first(
    mut wins: Vec<(String, u32)>,
    own: Option<(String, u32)>,
) -> Vec<(String, u32)> {
    if let Some(own) = own {
        if !wins.contains(&own) {
            wins.push(own);
        }
    }
    wins.sort_by_key(|w| std::cmp::Reverse(w.1));
    wins
}

/// The full set of windows a worktree removal must tear down, in the order to kill them.
///
/// `windows_under_path` plus the window of `own_pane` (a live agent's pane, which must die
/// even if its cwd has diverged from the worktree — we never `git worktree remove` from
/// under a running agent). The agent's window is looked up by pane id in `panes` (see
/// `own_window`), so a stale index can never pick an unrelated window; a pane that is
/// already gone adds nothing. A row without a live agent still occupies windows (an idle
/// worktree in session mode keeps its shell and agent windows), and skipping the teardown
/// for those is what orphans a session on a directory git is about to delete.
///
/// Sorted by descending window index (see `with_own_top_first`).
pub fn windows_to_kill(panes: &[Pane], path: &str, own_pane: Option<&str>) -> Vec<(String, u32)> {
    with_own_top_first(windows_under_path(panes, path), own_window(panes, own_pane))
}

/// Why closing a worktree's windows (`X`, close without removing) was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum CloseRefusal {
    /// No tmux window is rooted in the worktree — there is nothing to close.
    NothingOpen,
    /// One of the windows hosts Hydra's own pane; closing it would kill Hydra itself.
    HostsSelf,
    /// The windows are every window of the session the popup's client is attached to:
    /// killing them destroys that session, which detaches the client and takes the popup
    /// (Hydra) down with it.
    WholeClientSession,
}

impl CloseRefusal {
    /// The user-facing message for this refusal; `label` names the worktree.
    pub fn message(&self, label: &str) -> String {
        match self {
            CloseRefusal::NothingOpen => format!("no open windows for {label}"),
            CloseRefusal::HostsSelf => "can't close the window Hydra is running in".into(),
            CloseRefusal::WholeClientSession => {
                format!(
                    "can't close {label}: it's the session you're attached to — switch away first"
                )
            }
        }
    }
}

/// Live tmux context a close is planned against, besides the pane listing.
#[derive(Debug, Default, Clone, Copy)]
pub struct CloseGuard<'a> {
    /// Other known worktree roots: panes under one nested inside the closed path belong
    /// to it and are left alone (see `windows_under_path_except`).
    pub other_roots: &'a [String],
    /// Hydra's own pane id (`$TMUX_PANE`), when it runs in a pane rather than a popup.
    pub self_pane: Option<&'a str>,
    /// The session the popup's client is attached to (`tmux::current_session`) — the
    /// guard that holds for a `display-popup`, which has no pane of its own.
    pub client_session: Option<&'a str>,
}

/// Which windows count as a worktree's for a close (`X`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseScope<'a> {
    /// Every window with a pane rooted in the worktree (minus nested worktrees) — the
    /// same set as a removal. Right for a linked worktree: nothing but its own windows
    /// lives there.
    Rooted,
    /// Only the windows Hydra owns for the worktree: the named dedicated session (session
    /// mode's `session_name`, when it exists) plus the agent's own window. For the repo's
    /// main checkout, where the user's own editor/shell windows usually sit too — a close
    /// must not take those down just because they're `cd`'d into the repo.
    Owned { session: &'a str },
}

/// The windows a close (`X`) tears down, top window first; the worktree itself is kept.
///
/// `scope` picks the base set (see `CloseScope`); the window of `own_pane` (a live
/// agent's pane, resolved against `panes` by id — never a stale index) is always added.
/// For `Rooted`, windows of worktrees nested inside the path (`guard.other_roots`) are
/// excluded. Refused when a window to close holds Hydra's own pane (`guard.self_pane`),
/// or when the set covers every window of the client's session (`guard.client_session`)
/// — that would destroy the session the popup is open on. An empty set is
/// `NothingOpen`, so the UI can say so instead of asking to confirm a no-op.
pub fn windows_to_close(
    panes: &[Pane],
    path: &str,
    own_pane: Option<&str>,
    scope: CloseScope,
    guard: CloseGuard,
) -> Result<Vec<(String, u32)>, CloseRefusal> {
    let base = match scope {
        CloseScope::Rooted => windows_under_path_except(panes, path, guard.other_roots),
        CloseScope::Owned { session } => {
            let mut out: Vec<(String, u32)> = Vec::new();
            for p in panes.iter().filter(|p| p.session_name == session) {
                let key = (p.session_name.clone(), p.window_index);
                if !out.contains(&key) {
                    out.push(key);
                }
            }
            out
        }
    };
    let wins = with_own_top_first(base, own_window(panes, own_pane));
    if wins.is_empty() {
        return Err(CloseRefusal::NothingOpen);
    }
    if let Some(me) = guard
        .self_pane
        .and_then(|id| panes.iter().find(|p| p.pane_id == id))
    {
        if wins.contains(&(me.session_name.clone(), me.window_index)) {
            return Err(CloseRefusal::HostsSelf);
        }
    }
    if let Some(client) = guard.client_session {
        let mut session_windows: Vec<u32> = panes
            .iter()
            .filter(|p| p.session_name == client)
            .map(|p| p.window_index)
            .collect();
        session_windows.sort_unstable();
        session_windows.dedup();
        let covered = session_windows
            .iter()
            .all(|w| wins.contains(&(client.to_string(), *w)));
        if !session_windows.is_empty() && covered {
            return Err(CloseRefusal::WholeClientSession);
        }
    }
    Ok(wins)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(pane_id: &str, status: Status, updated_at: u64) -> AgentState {
        AgentState {
            socket: "/sock".into(),
            session_id: "1".into(),
            pane_id: pane_id.into(),
            cwd: "/repo".into(),
            status,
            event: "x".into(),
            task_summary: None,
            attention: None,
            updated_at,
        }
    }

    fn pane(pane_id: &str, session: &str, window_index: u32) -> Pane {
        Pane {
            pane_id: pane_id.into(),
            session_name: session.into(),
            window_index,
            window_name: "claude".into(),
            cwd: "/repo".into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        }
    }

    fn agent_with(
        pane_id: &str,
        status: Status,
        window: u32,
        repo: Option<(&str, &str, Option<&str>)>,
    ) -> Agent {
        Agent {
            state: state(pane_id, status, 100),
            pane: pane(pane_id, "proj", window),
            effective_status: status,
            worktree: repo.map(|(key, name, branch)| WorktreeInfo {
                root: "/r".into(),
                repo_key: key.into(),
                repo_name: name.into(),
                branch: branch.map(String::from),
            }),
            dirty: 0,
        }
    }

    fn idle_wt(path: &str, branch: Option<&str>) -> IdleWorktree {
        IdleWorktree {
            path: path.into(),
            branch: branch.map(str::to_string),
            repo_key: "/k".into(),
            repo_name: "proj".into(),
            state: None,
        }
    }

    #[test]
    fn one_line_collapses_newlines_and_whitespace_runs() {
        assert_eq!(
            one_line("<task-notification>\n<task-id>abc</task-id>\n<tool-use>"),
            "<task-notification> <task-id>abc</task-id> <tool-use>"
        );
        assert_eq!(one_line("  padded   text \t x "), "padded text x");
        assert_eq!(one_line(""), "");
    }

    #[test]
    fn detail_text_is_always_a_single_line() {
        // A multi-line task summary would otherwise wrap and shear the columns of
        // every row printed after it.
        let mut a = agent_with("%1", Status::Idle, 0, None);
        a.state.task_summary = Some("first line\nsecond line".into());
        assert_eq!(detail_text(&a).as_deref(), Some("first line second line"));

        let mut b = agent_with("%2", Status::NeedsInput, 0, None);
        b.state.attention = Some("needs permission\nto run Bash".into());
        assert_eq!(
            detail_text(&b).as_deref(),
            Some("needs permission to run Bash")
        );
    }

    #[test]
    fn worktree_label_uses_the_branch_when_there_is_one() {
        assert_eq!(
            worktree_label(&idle_wt("/wt/feat-x", Some("feat/x"))),
            "feat/x"
        );
    }

    #[test]
    fn worktree_label_names_a_detached_worktree_by_its_directory() {
        // Several detached PR checkouts of one repo must not all collapse to the same
        // row — the basename is what tells them apart (and `x` remove confirms on it).
        assert_eq!(
            worktree_label(&idle_wt("/work/tree/pr-1462", None)),
            "pr-1462 (detached)"
        );
        assert_eq!(
            worktree_label(&idle_wt("/work/tree/pr-1543/", None)),
            "pr-1543 (detached)"
        );
    }

    #[test]
    fn worktree_label_falls_back_when_there_is_no_basename() {
        assert_eq!(worktree_label(&idle_wt("/", None)), "(detached)");
        assert_eq!(worktree_label(&idle_wt("", None)), "(detached)");
    }

    #[test]
    fn idle_from_excludes_occupied_worktrees() {
        // /wt/a is occupied; /wt/b and /repo/main are idle.
        let occupied = std::collections::HashSet::from([canon("/wt/a")]);
        let project = ProjectWorktrees {
            repo_key: "/k".into(),
            repo_name: "proj".into(),
            default_branch: "main".into(),
            entries: vec![
                ("/repo/main".into(), Some("main".into())),
                ("/wt/a".into(), Some("a".into())),
                ("/wt/b".into(), Some("b".into())),
            ],
        };
        let idle = idle_from(&occupied, &project);
        let paths: Vec<&str> = idle.iter().map(|w| w.path.as_str()).collect();
        assert_eq!(paths, vec!["/repo/main", "/wt/b"]);
    }

    #[test]
    fn occupied_roots_spans_every_session_and_canonicalizes() {
        // Two agents in DIFFERENT sessions, each in its own worktree. Occupancy must not
        // be session-scoped: both roots count, so neither shows as idle in any view.
        let mut a = agent_with("%1", Status::Working, 1, Some(("/k", "proj", Some("a"))));
        a.pane.session_name = "sess-a".into();
        a.worktree.as_mut().unwrap().root = "/wt/a".into();
        let mut b = agent_with("%2", Status::Idle, 1, Some(("/k", "proj", Some("b"))));
        b.pane.session_name = "sess-b".into();
        b.worktree.as_mut().unwrap().root = "/wt/b".into();

        let roots = occupied_roots(&[a, b]);
        assert!(roots.contains(&canon("/wt/a")));
        assert!(roots.contains(&canon("/wt/b")));
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn dead_states_only_flags_long_gone_panes() {
        let now = 10_000;
        let states = vec![
            state("%1", Status::Idle, 0),           // pane alive → kept
            state("%2", Status::Working, 9_990),    // pane gone but recent → kept
            state("%3", Status::NeedsInput, 1_000), // pane gone + old → dead
        ];
        let panes = vec![pane("%1", "proj", 1)];
        let dead = dead_states(&states, &panes, now, GC_GRACE_SECS);
        assert_eq!(dead, vec![("/sock".to_string(), "%3".to_string())]);
    }

    #[test]
    fn idle_anchors_lists_agent_roots_first_then_popup_cwd() {
        let a1 = agent_with("%1", Status::Working, 1, Some(("/a/.git", "alpha", None)));
        let a2 = agent_with("%2", Status::Idle, 2, None); // no worktree → no anchor
        assert_eq!(
            idle_anchors(&[a1, a2], Some("/popup/repo")),
            vec!["/r".to_string(), "/popup/repo".to_string()]
        );
        assert!(idle_anchors(&[], None).is_empty());
    }

    #[test]
    fn detail_text_prefers_attention_only_while_needing_input() {
        let mut a = agent_with("%1", Status::NeedsInput, 1, None);
        a.state.task_summary = Some("build the api".into());
        a.state.attention = Some("needs permission to run Bash".into());
        assert_eq!(
            detail_text(&a),
            Some("needs permission to run Bash".to_string())
        );
        // Back to working: the attention reason no longer applies.
        a.effective_status = Status::Working;
        assert_eq!(detail_text(&a), Some("build the api".to_string()));
    }

    #[test]
    fn truncate_is_char_safe_and_adds_an_ellipsis() {
        assert_eq!(truncate("hi", 60), "hi");
        let t = truncate(&"a".repeat(100), 10);
        assert_eq!(t.chars().count(), 10);
        assert!(t.ends_with('…'));
        // Multibyte input must not panic and must land on a char boundary.
        let t = truncate("héllo wörld ☃ quite long indeed", 5);
        assert_eq!(t.chars().count(), 5);
    }

    #[test]
    fn format_age_uses_compact_units() {
        assert_eq!(format_age(5), "5s");
        assert_eq!(format_age(90), "1m");
        assert_eq!(format_age(3600), "1h");
        assert_eq!(format_age(90_000), "1d");
    }

    #[test]
    fn choose_scope_defaults_to_repo_then_falls_back_to_session() {
        // Popup in a repo, toggle off → repo-scoped (across sessions).
        assert!(matches!(
            choose_scope(false, Some("/k"), "proj"),
            Scope::Repo("/k")
        ));
        // Popup NOT in a repo, toggle off → session-scoped fallback.
        assert!(matches!(
            choose_scope(false, None, "proj"),
            Scope::Session("proj")
        ));
        // Toggle on → all sessions, regardless of repo context.
        assert!(matches!(choose_scope(true, Some("/k"), "proj"), Scope::All));
        assert!(matches!(choose_scope(true, None, "proj"), Scope::All));
    }

    #[test]
    fn matches_scope_filters_by_session_repo_or_all() {
        // agent_with puts the pane in session "proj"; repo_key comes from the worktree.
        let a = agent_with("%1", Status::Idle, 1, Some(("/k1", "alpha", None)));
        let b = agent_with("%2", Status::Idle, 1, None); // no worktree

        // Session scope keys off the tmux session name.
        assert!(matches_scope(&a, &Scope::Session("proj")));
        assert!(!matches_scope(&a, &Scope::Session("other")));

        // Repo scope keys off the resolved worktree's repo_key, across sessions.
        assert!(matches_scope(&a, &Scope::Repo("/k1")));
        assert!(!matches_scope(&a, &Scope::Repo("/k2")));
        // A worktree-less agent can never match a repo scope.
        assert!(!matches_scope(&b, &Scope::Repo("/k1")));

        // All keeps everything, worktree or not.
        assert!(matches_scope(&a, &Scope::All));
        assert!(matches_scope(&b, &Scope::All));
    }

    #[test]
    fn filter_matches_branch_repo_and_summary_case_insensitively() {
        let mut a = agent_with(
            "%1",
            Status::Idle,
            1,
            Some(("/a/.git", "alpha", Some("feat/pagination"))),
        );
        a.state.task_summary = Some("refactor cursors".into());
        assert!(matches_filter(&a, ""));
        assert!(matches_filter(&a, "PAGIN"));
        assert!(matches_filter(&a, "alpha"));
        assert!(matches_filter(&a, "cursor"));
        assert!(!matches_filter(&a, "nonexistent"));
    }

    #[test]
    fn filter_matches_the_worktree_folder_and_session_name() {
        let mut a = agent_with(
            "%1",
            Status::Idle,
            1,
            Some(("/a/.git", "svc", Some("CET-1/feat/partner-service"))),
        );
        a.worktree.as_mut().unwrap().root = "/tree/CET-1-feat-partner-identity".into();
        a.pane.session_name = "svc-CET-1-feat-partner-service".into();
        assert!(matches_filter(&a, "IDENTITY"), "folder basename matches");
        assert!(matches_filter(&a, "svc-cet-1"), "session name matches");
        assert!(
            !matches_filter(&a, "tree/"),
            "only the folder name, not the path"
        );
    }

    #[test]
    fn folder_hint_only_when_the_folder_isnt_the_branch() {
        // Folder named for another branch: worth showing.
        assert_eq!(
            folder_hint(
                Some("CET-3200/feat/cet-3723-partner-service"),
                "/w/tree/CET-3200-feat-cet-3723-partner-identity",
                "svc"
            ),
            Some("CET-3200-feat-cet-3723-partner-identity".into())
        );
        // Folder is the sanitized branch (how hydra names them): redundant.
        assert_eq!(
            folder_hint(Some("feat/x y"), "/w/tree/feat-x-y/", "svc"),
            None
        );
        // Main checkout: the folder is the repo name, already in the group header.
        assert_eq!(folder_hint(Some("main"), "/code/svc", "svc"), None);
        // Detached: the label already is the folder.
        assert_eq!(folder_hint(None, "/w/tree/pr-1", "svc"), None);
    }

    #[test]
    fn display_session_drops_the_repo_prefix_only() {
        assert_eq!(
            display_session("cet-services-CET-3200-feat-x", Some("cet-services")),
            "CET-3200-feat-x"
        );
        // Dots in the repo name were made tmux-safe in the session name.
        assert_eq!(display_session("my-app-feat", Some("my.app")), "feat");
        assert_eq!(display_session("main", Some("cet-services")), "main");
        assert_eq!(display_session("svc-", Some("svc")), "svc-");
        assert_eq!(display_session("svc-x", None), "svc-x");
    }

    #[test]
    fn row_widths_give_names_full_width_when_there_is_room() {
        let w = row_widths(140, 30, 80);
        assert_eq!(
            w,
            RowWidths {
                place: 30,
                name: 80,
                detail: 30
            }
        );
    }

    #[test]
    fn row_widths_shrink_place_then_detail_before_name() {
        // 10 short: all taken from the place cell.
        let w = row_widths(120, 30, 80);
        assert_eq!((w.place, w.name, w.detail), (20, 80, ROW_DETAIL_MIN));
        // Place bottoms out at its minimum, then the trailing cell yields down to its
        // floor before the name loses a column.
        let w = row_widths(60, 30, 80);
        assert_eq!(w.place, ROW_PLACE_MIN);
        assert_eq!(w.detail, ROW_DETAIL_FLOOR);
        assert_eq!(w.name, 60 - ROW_PLACE_MIN - ROW_DETAIL_FLOOR);
        // A 38-char branch fits once the place is at its minimum, at the cost of detail.
        let w = row_widths(60, 40, 38);
        assert_eq!((w.place, w.name, w.detail), (ROW_PLACE_MIN, 38, 14));
        // Short wants are never padded up to the minimums.
        let w = row_widths(100, 6, 10);
        assert_eq!((w.place, w.name, w.detail), (6, 10, 84));
    }

    #[test]
    fn row_widths_never_exceed_the_available_width() {
        for avail in 0..80 {
            let w = row_widths(avail, 40, 90);
            assert_eq!(w.place + w.name + w.detail, avail, "avail {avail}");
        }
    }

    #[test]
    fn fit_place_keeps_the_window_suffix() {
        assert_eq!(fit_place("feat-x", ":3", 20), "feat-x:3");
        assert_eq!(fit_place("CET-3200-feat-partner", ":3", 10), "CET-320…:3");
        assert!(fit_place("CET-3200-feat-partner", ":3", 10).chars().count() <= 10);
    }

    #[test]
    fn fit_name_truncates_the_hint_before_the_label() {
        let full = fit_name("feat/a", Some("folder-b"), 40);
        assert_eq!(full, ("feat/a".into(), " · folder-b".into()));
        let short = fit_name("feat/a", Some("folder-bbbbbbbb"), 14);
        assert_eq!(short.0, "feat/a");
        assert_eq!(short.0.chars().count() + short.1.chars().count(), 14);
        // No useful room for the hint: dropped, label intact.
        assert_eq!(
            fit_name("feat/a", Some("folder-b"), 9),
            ("feat/a".into(), String::new())
        );
        // Label too long: truncated, no hint.
        assert_eq!(
            fit_name("feat/abcdef", Some("x"), 6),
            ("feat/…".into(), String::new())
        );
    }

    #[test]
    fn joins_only_panes_that_still_exist() {
        let states = vec![
            state("%1", Status::Idle, 100),
            state("%2", Status::Working, 100), // no matching live pane
        ];
        let panes = vec![pane("%1", "proj", 1)];
        let agents = join_and_sort(states, &panes, Some("proj"), 100, STALE_AFTER_SECS);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].state.pane_id, "%1");
    }

    #[test]
    fn filters_out_other_sessions() {
        let states = vec![
            state("%1", Status::Idle, 100),
            state("%2", Status::Idle, 100),
        ];
        let panes = vec![pane("%1", "proj", 1), pane("%2", "other", 1)];
        let agents = join_and_sort(states, &panes, Some("proj"), 100, STALE_AFTER_SECS);
        assert_eq!(agents.len(), 1);
        assert_eq!(agents[0].pane.session_name, "proj");
    }

    #[test]
    fn no_session_filter_keeps_every_session() {
        let states = vec![
            state("%1", Status::Idle, 100),
            state("%2", Status::Idle, 100),
        ];
        let panes = vec![pane("%1", "proj", 1), pane("%2", "other", 1)];
        let agents = join_and_sort(states, &panes, None, 100, STALE_AFTER_SECS);
        assert_eq!(agents.len(), 2);
    }

    #[test]
    fn sorts_needs_input_first_then_by_window() {
        let states = vec![
            state("%1", Status::Idle, 100),
            state("%2", Status::NeedsInput, 100),
            state("%3", Status::Working, 100),
            state("%4", Status::Working, 100),
        ];
        let panes = vec![
            pane("%1", "proj", 5),
            pane("%2", "proj", 4),
            pane("%3", "proj", 3),
            pane("%4", "proj", 1),
        ];
        let agents = join_and_sort(states, &panes, Some("proj"), 100, STALE_AFTER_SECS);
        let order: Vec<&str> = agents.iter().map(|a| a.state.pane_id.as_str()).collect();
        // NeedsInput(%2), then Working by window index (%4 win1, %3 win3), then Idle(%1).
        assert_eq!(order, vec!["%2", "%4", "%3", "%1"]);
    }

    #[test]
    fn stale_working_agent_becomes_unknown() {
        let states = vec![state("%1", Status::Working, 0)];
        let panes = vec![pane("%1", "proj", 1)];
        let agents = join_and_sort(states, &panes, Some("proj"), 10_000, STALE_AFTER_SECS);
        assert_eq!(agents[0].effective_status, Status::Unknown);
    }

    #[test]
    fn old_idle_agent_stays_idle() {
        // Idle agents can sit indefinitely; staleness must not touch them.
        let states = vec![state("%1", Status::Idle, 0)];
        let panes = vec![pane("%1", "proj", 1)];
        let agents = join_and_sort(states, &panes, Some("proj"), 10_000, STALE_AFTER_SECS);
        assert_eq!(agents[0].effective_status, Status::Idle);
    }

    #[test]
    fn windows_under_path_matches_rooted_windows_dedups_and_respects_boundary() {
        let mk = |id: &str, session: &str, win: u32, cwd: &str| Pane {
            pane_id: id.into(),
            session_name: session.into(),
            window_index: win,
            window_name: "w".into(),
            cwd: cwd.into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        };
        let panes = vec![
            mk("%1", "wt-a", 1, "/root/wt-a"),     // shell window (exact match)
            mk("%2", "wt-a", 2, "/root/wt-a"),     // agent window
            mk("%3", "wt-a", 2, "/root/wt-a/src"), // 2nd pane, same window -> dedup
            mk("%4", "other", 5, "/root/wt-ab"),   // boundary: must NOT match
            mk("%5", "other", 6, "/elsewhere"),    // unrelated
        ];
        let got = windows_under_path(&panes, "/root/wt-a");
        assert_eq!(got, vec![("wt-a".to_string(), 1), ("wt-a".to_string(), 2)]);
    }

    #[test]
    fn windows_under_path_empty_when_nothing_matches() {
        let panes: Vec<Pane> = Vec::new();
        assert!(windows_under_path(&panes, "/root/wt-a").is_empty());
    }

    fn kill_pane(id: &str, session: &str, win: u32, cwd: &str) -> Pane {
        Pane {
            pane_id: id.into(),
            session_name: session.into(),
            window_index: win,
            window_name: "w".into(),
            cwd: cwd.into(),
            window_active: false,
            pane_tty: "/dev/ttys000".into(),
        }
    }

    #[test]
    fn windows_to_kill_tears_down_a_worktree_with_no_live_agent() {
        // The orphan-session bug: an idle worktree row (no agent state file) still owns a
        // session-mode session's shell + agent windows. They must all be killed.
        let panes = vec![
            kill_pane("%1", "repo-feat", 1, "/root/wt-a"),
            kill_pane("%2", "repo-feat", 2, "/root/wt-a"),
            kill_pane("%3", "other", 1, "/elsewhere"),
        ];
        assert_eq!(
            windows_to_kill(&panes, "/root/wt-a", None),
            vec![("repo-feat".to_string(), 2), ("repo-feat".to_string(), 1)],
            "highest window index first"
        );
    }

    #[test]
    fn windows_to_kill_adds_the_agents_own_window_when_its_cwd_diverged() {
        let panes = vec![
            kill_pane("%1", "repo-feat", 1, "/root/wt-a"),
            // Agent's pane reports a different cwd (symlink divergence), so it isn't
            // matched by path — it still has to die before git removes the worktree.
            kill_pane("%7", "repo-feat", 7, "/private/root/wt-a"),
        ];
        let got = windows_to_kill(&panes, "/root/wt-a", Some("%7"));
        assert_eq!(
            got,
            vec![("repo-feat".to_string(), 7), ("repo-feat".to_string(), 1)]
        );
        // The agent's pane already gone: nothing extra is killed.
        assert_eq!(
            windows_to_kill(&panes[..1], "/root/wt-a", Some("%7")),
            vec![("repo-feat".to_string(), 1)]
        );
    }

    #[test]
    fn windows_to_kill_does_not_duplicate_the_agents_own_window() {
        let panes = vec![
            kill_pane("%1", "repo-feat", 2, "/root/wt-a"),
            kill_pane("%2", "repo-feat", 2, "/root/wt-a/src"),
        ];
        assert_eq!(
            windows_to_kill(&panes, "/root/wt-a", Some("%1")),
            vec![("repo-feat".to_string(), 2)]
        );
    }

    #[test]
    fn windows_to_close_returns_every_rooted_window_top_first() {
        let panes = vec![
            kill_pane("%1", "repo-feat", 1, "/root/wt-a"), // shell
            kill_pane("%2", "repo-feat", 2, "/root/wt-a"), // extra window (diff)
            kill_pane("%3", "repo-feat", 3, "/root/wt-a"), // agent
            kill_pane("%4", "other", 1, "/elsewhere"),
        ];
        assert_eq!(
            windows_to_close(
                &panes,
                "/root/wt-a",
                None,
                CloseScope::Rooted,
                CloseGuard::default()
            ),
            Ok(vec![
                ("repo-feat".to_string(), 3),
                ("repo-feat".to_string(), 2),
                ("repo-feat".to_string(), 1),
            ])
        );
    }

    #[test]
    fn windows_to_close_reports_nothing_open() {
        let panes = vec![kill_pane("%1", "other", 1, "/elsewhere")];
        assert_eq!(
            windows_to_close(
                &panes,
                "/root/wt-a",
                None,
                CloseScope::Rooted,
                CloseGuard::default()
            ),
            Err(CloseRefusal::NothingOpen)
        );
    }

    #[test]
    fn windows_to_close_includes_a_live_agents_own_window() {
        let panes = vec![kill_pane("%4", "repo-feat", 4, "/somewhere/else")];
        let got = windows_to_close(
            &panes,
            "/root/wt-a",
            Some("%4"),
            CloseScope::Rooted,
            CloseGuard::default(),
        );
        assert_eq!(got, Ok(vec![("repo-feat".to_string(), 4)]));
    }

    #[test]
    fn windows_to_close_refuses_to_kill_hydras_own_window() {
        let panes = vec![
            kill_pane("%1", "repo-feat", 1, "/root/wt-a"),
            // Hydra itself runs in a pane of window 2 (cwd elsewhere, same window).
            kill_pane("%2", "repo-feat", 2, "/root/wt-a"),
            kill_pane("%9", "repo-feat", 2, "/tmp"),
        ];
        let me = CloseGuard {
            self_pane: Some("%9"),
            ..CloseGuard::default()
        };
        assert_eq!(
            windows_to_close(&panes, "/root/wt-a", None, CloseScope::Rooted, me),
            Err(CloseRefusal::HostsSelf)
        );
        // Hydra's pane in an unrelated window doesn't block the close.
        let panes2 = vec![
            kill_pane("%1", "repo-feat", 1, "/root/wt-a"),
            kill_pane("%9", "home", 1, "/tmp"),
        ];
        assert!(windows_to_close(&panes2, "/root/wt-a", None, CloseScope::Rooted, me).is_ok());
    }

    #[test]
    fn windows_to_close_on_the_main_checkout_spares_nested_worktrees() {
        let panes = vec![
            kill_pane("%1", "main", 1, "/repo"),
            kill_pane("%2", "main", 2, "/repo/src"),
            kill_pane("%3", "repo-x", 1, "/repo/.claude/worktrees/x"),
            kill_pane("%4", "repo-y", 1, "/repo/.worktrees/y/src"),
            kill_pane("%5", "other", 1, "/repo-other"),
        ];
        let others = vec![
            "/repo".to_string(), // the closed path itself is not "nested"
            "/repo/.claude/worktrees/x".to_string(),
            "/repo/.worktrees/y".to_string(),
        ];
        let guard = CloseGuard {
            other_roots: &others,
            ..CloseGuard::default()
        };
        assert_eq!(
            windows_to_close(&panes, "/repo", None, CloseScope::Rooted, guard),
            Ok(vec![("main".to_string(), 2), ("main".to_string(), 1)])
        );
        // Closing the nested worktree itself still finds its window.
        assert_eq!(
            windows_to_close(
                &panes,
                "/repo/.claude/worktrees/x",
                None,
                CloseScope::Rooted,
                guard
            ),
            Ok(vec![("repo-x".to_string(), 1)])
        );
    }

    #[test]
    fn windows_to_close_refuses_to_destroy_the_client_session() {
        let panes = vec![
            kill_pane("%1", "repo-feat", 1, "/root/wt-a"),
            kill_pane("%2", "repo-feat", 2, "/root/wt-a"),
            kill_pane("%3", "home", 1, "/tmp"),
        ];
        let attached = |s| CloseGuard {
            client_session: Some(s),
            ..CloseGuard::default()
        };
        // The popup is open on the worktree's own session: every window would go.
        assert_eq!(
            windows_to_close(
                &panes,
                "/root/wt-a",
                None,
                CloseScope::Rooted,
                attached("repo-feat")
            ),
            Err(CloseRefusal::WholeClientSession)
        );
        // Attached elsewhere: fine.
        assert!(windows_to_close(
            &panes,
            "/root/wt-a",
            None,
            CloseScope::Rooted,
            attached("home")
        )
        .is_ok());
        // Attached to a session that keeps another window: fine too.
        let mut more = panes.clone();
        more.push(kill_pane("%4", "repo-feat", 3, "/elsewhere"));
        assert!(windows_to_close(
            &more,
            "/root/wt-a",
            None,
            CloseScope::Rooted,
            attached("repo-feat")
        )
        .is_ok());
    }

    #[test]
    fn windows_to_close_resolves_the_agents_window_from_the_fresh_listing() {
        // When X was pressed the agent sat in window 4 and an editor in 5. Window 2
        // closed during the prompt and `renumber-windows` shifted them to 3 and 4: the
        // stale (work, 4) would now name the editor. Planning by pane id kills the
        // agent's real window, never a stale index.
        let panes = vec![
            kill_pane("%1", "work", 1, "/home"),
            kill_pane("%5", "work", 3, "/elsewhere"), // the agent, renumbered 4 -> 3
            kill_pane("%6", "work", 4, "/home/editor"), // the editor, renumbered 5 -> 4
        ];
        assert_eq!(
            windows_to_close(
                &panes,
                "/root/wt-a",
                Some("%5"),
                CloseScope::Rooted,
                CloseGuard::default()
            ),
            Ok(vec![("work".to_string(), 3)])
        );
        // The agent's pane is gone entirely: nothing to kill, not some other window.
        let gone = vec![panes[0].clone(), panes[2].clone()];
        assert_eq!(
            windows_to_close(
                &gone,
                "/root/wt-a",
                Some("%5"),
                CloseScope::Rooted,
                CloseGuard::default()
            ),
            Err(CloseRefusal::NothingOpen)
        );
    }

    #[test]
    fn owned_close_on_the_main_checkout_spares_the_users_own_windows() {
        let panes = vec![
            // The user's working session, rooted at the main checkout.
            kill_pane("%1", "work", 1, "/repo"),
            kill_pane("%2", "work", 2, "/repo/src"), // editor
            kill_pane("%3", "work", 3, "/repo"),     // the agent (window mode)
            kill_pane("%4", "home", 1, "/tmp"),
        ];
        let scope = CloseScope::Owned {
            session: "repo-main",
        };
        assert_eq!(
            windows_to_close(&panes, "/repo", Some("%3"), scope, CloseGuard::default()),
            Ok(vec![("work".to_string(), 3)]),
            "only the agent's window; the editor and shell stay"
        );
        // No agent and no dedicated session: nothing of Hydra's is open there.
        assert_eq!(
            windows_to_close(&panes, "/repo", None, scope, CloseGuard::default()),
            Err(CloseRefusal::NothingOpen)
        );
        // Session mode: the dedicated session's windows all go, whatever their cwd.
        let mut with_session = panes.clone();
        with_session.push(kill_pane("%5", "repo-main", 1, "/repo"));
        with_session.push(kill_pane("%6", "repo-main", 2, "/repo/sub"));
        assert_eq!(
            windows_to_close(&with_session, "/repo", None, scope, CloseGuard::default()),
            Ok(vec![
                ("repo-main".to_string(), 2),
                ("repo-main".to_string(), 1)
            ])
        );
    }

    #[test]
    fn close_refusal_messages() {
        assert_eq!(
            CloseRefusal::NothingOpen.message("feat"),
            "no open windows for feat"
        );
        assert!(CloseRefusal::WholeClientSession
            .message("feat")
            .contains("attached"));
    }

    #[test]
    fn preview_titles_keep_the_full_name_and_move_the_folder_down() {
        let label = "CET-3200/feat/cet-3723-partner-service";
        let folder = "CET-3200-feat-cet-3723-partner-identity";
        let hint = Some(folder);
        // Wide: everything on one line.
        let (top, bottom) = preview_titles(Some(3), label, hint, 200);
        assert_eq!(top, format!("preview · win 3 · {label} · {}", folder));
        assert_eq!(bottom, None);
        // The user's preview width (~54 columns of room): `win N` dropped, full label on
        // top, the folder on the bottom border.
        let (top, bottom) = preview_titles(Some(3), label, hint, 54);
        assert_eq!(top, format!("preview · {label}"));
        assert_eq!(bottom, Some(format!("folder {}", folder)));
        // Too narrow even for the label: the label wins over the prefix.
        let (top, _) = preview_titles(Some(3), label, None, 20);
        assert_eq!(top.chars().count(), 20);
        assert!(top.starts_with("CET-3200/"));
    }
}
