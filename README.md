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
  The indicator sits on the right of the bar; when an agent needs input it becomes a
  `⚠ N NEEDS INPUT` block. Colours live in `src/status.rs` (matched to a theme palette).

Remove everything with `hydra uninstall`.

## Configuration

Hydra reads an optional TOML config from `~/.config/hydra/config.toml` (override the path
with `$HYDRA_CONFIG`). Without a config, Hydra uses its built-in defaults — nothing to set
up. `hydra install` writes a commented starter file with every default if none exists.

Most settings are read at runtime, so a rebuild isn't needed to pick them up. The one
exception is `[popup]` (key/size), which is baked into `~/.tmux.conf` by `install` — after
changing it, re-run `hydra install` and `tmux source-file ~/.tmux.conf`.

Precedence: built-in defaults → config file → environment variables
(`HYDRA_WORKTREE_ROOT`, `HYDRA_ALERTS`) win on top.

```toml
[timings]
stale_after_secs       = 900   # WORKING agent silent this long → UNKNOWN
refresh_ms             = 250   # popup refresh tick
dirty_ttl_secs         = 3     # throttle for git-status dirty counts
worktree_list_ttl_secs = 5     # throttle for git worktree list

[agent]
command       = "claude"       # launched by `n` (spawn) and Enter (start in worktree)
worktree_root = "~/work/tree"  # where spawned worktrees go

[popup]                        # re-run `hydra install` after changing
key    = "a"
width  = "70%"
height = "60%"

[theme.tui]                    # a color name ("green") or "#rrggbb"
highlight_bg = "#32323c"
working      = "green"
needs_input  = "yellow"
idle         = "gray"
unknown      = "darkgray"

[theme.status]                 # status-bar palette
label    = "#b35b79"
working  = "#5e857a"
idle     = "#d9a594"
alert_fg = "#f2ecbc"
alert_bg = "#d7474b"
unknown  = "#b35b79"

[alerts]
enabled = true                 # HYDRA_ALERTS=0 also disables
```

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
| `x` | remove the selected worktree (confirm with `y`) |
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

### Removing worktrees

`x` removes the selected worktree when the work is done, after a `y/N` confirm. If the
worktree has a running agent, its tmux window is killed first. Uncommitted changes are
surfaced in the prompt and require confirming a forced removal. The **branch is kept**
(`git worktree remove` only) and the main/current worktree can't be removed.

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
