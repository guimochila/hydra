# Hydra

A tmux popup overseer for [Claude Code](https://claude.com/claude-code) agents.

Hydra shows every Claude Code agent running in your current tmux session — across all
windows — with a live status (working / needs input / done), the window it's in, and
its git worktree. Summon it from any window with a keybinding, navigate with vim keys,
and press Enter to jump to an agent. You can also approve/deny a pending prompt or send
a message to an agent without leaving the popup.

```
┌─ Hydra · cet-services · 4 agents · ⚠ 1 ──────────────┐
│ ▸ cet-services (3)                                   │
│   ⚠ win  4  feat/pagination-list-apis  fix cursor…   │
│   ● win  2  feat/auth-tokens           add refresh…  │
│   ○ win  5  chore/deps                                │
│ ▸ no worktree (1)                                    │
│   ○ win  6  scratch                                  │
│ j/k move  ⏎ jump  a ✓  d ✗  i send  / filter  q quit │
└──────────────────────────────────────────────────────┘
```

## How it works

Hydra never scrapes terminal output. Instead:

1. **Claude Code hooks push state.** `hydra install` registers hooks that run
   `hydra hook <event>` on each lifecycle event. Each agent self-reports its tmux
   socket, pane, cwd, and status into a per-pane JSON file in a runtime dir — keyed by
   `$TMUX_PANE`.
2. **tmux says where things are.** The popup joins those state files against
   `tmux list-panes` by pane id. A dead pane's leftover file matches nothing and
   simply disappears — no ghosts.
3. **Git says which worktree.** Each agent's cwd resolves to a branch/repo, and agents
   are grouped under their repo.

Status comes from the hook events: `UserPromptSubmit`/`PreToolUse` → working,
`Notification` → needs input, `Stop` → idle, `SessionEnd` → gone.

## Install

Requires Rust and tmux.

```sh
cargo build --release
./target/release/hydra install      # adds Claude Code hooks + a tmux popup binding
tmux source-file ~/.tmux.conf
```

`install` is non-destructive and reversible:

- It merges its hooks into `~/.claude/settings.json` alongside any you already have
  (a backup is written first).
- It appends a popup binding and a status-line indicator to `~/.tmux.conf` inside a
  marked block, using `set -ga status-right` so your existing status line is preserved.

Remove everything with `hydra uninstall`.

## Usage

Open the popup with **`prefix` + `a`** (tmux prefix, then `a`).

| Key | Action |
|-----|--------|
| `j` / `k`, arrows | move |
| `gg` / `G` | first / last |
| `Tab` | jump selection to the next agent needing input |
| `Enter` | jump to the agent's window — or, on an idle worktree, start `claude` there |
| `a` | approve a pending prompt (accept the highlighted default) |
| `d` | deny a pending prompt (Escape) |
| `i` | send a message to the agent |
| `n` | spawn a new agent: worktree + tmux window running `claude` |
| `p` | toggle the preview pane |
| `/` | filter (branch / repo / summary / window) |
| `r` | refresh |
| `q` / `Esc` | quit (Esc clears an active filter first) |

`a`/`d` only act when the selected agent is actually waiting for input.

Each row shows the agent's status glyph, how long it's been in that state (`4m`),
its window number, branch, an uncommitted-change count (`Δ3`), and its last prompt.
The preview pane (right) shows a live snapshot of the selected agent's screen.

The list also includes the project's **existing worktrees that have no agent yet**,
shown dimmed under their repo. Press `Enter` on one to start `claude` in it — so you
can pick up a worktree you created earlier without leaving Hydra. `git worktree list`
is the source, so worktrees are found wherever they live.

### Notifications

When an agent transitions into "needs input", Hydra fires a desktop notification
(macOS) so you don't have to watch the popup. Set `HYDRA_ALERTS=0` to disable.

### Spawning agents

`n` creates a git worktree on a new branch off the repo's default branch, then opens a
tmux window running `claude` in it. Worktrees go under `~/work/tree/<name>` by default;
override with `HYDRA_WORKTREE_ROOT`. Spawning uses an existing agent to locate the repo
and session, so open at least one agent first.

## Commands

```
hydra                    Open the popup TUI
hydra ls                 Print the agent list (headless; for debugging)
hydra status <sock> <s>  Print the status-line indicator for a session
hydra hook <event>       Record a Claude Code lifecycle event (used by hooks)
hydra install            Install hooks + tmux popup keybinding + status indicator
hydra uninstall          Remove everything Hydra installed
```
