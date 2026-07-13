# CLAUDE.md

Guidance for working on Hydra, a Rust tmux popup that oversees Claude Code agents.
See `README.md` for the user-facing description.

## Build / test / lint

```sh
cargo build              # or: cargo build --release
cargo test               # all logic is unit-tested; keep it green
cargo clippy --all-targets
cargo fmt                # run before committing; CI-clean = fmt --check passes
```

The data/logic layers are pure and unit-tested; the UI is thin rendering over them.
The TUI itself is tested with `ratatui::backend::TestBackend` (see `ui.rs` tests):
build an `App` from synthetic agents/worktrees, drive `handle_key`, render into an
in-memory buffer and assert on its content — new UI behavior should come with such a
test. For real end-to-end checks, use `hydra ls` and `hydra status` (headless), or
drive the popup by hand in tmux.

## Architecture

Data flows one way: hooks **write** per-pane state files; the TUI **reads** them and
joins against live tmux. See `README.md` "How it works" for the high-level picture.

Module map (`src/`):

- `main.rs` — CLI dispatch (`hydra`, `ls`, `status`, `hook`, `install`, `uninstall`,
  `version`) and `current_overview()`, the shared "resolve socket+session → agents +
  idle worktrees" helper. Resolves every agent's worktree once (serving both occupancy
  and the display filter), then picks a display `Scope` via `agent::choose_scope`:
  repo-scoped by default (this repo's agents across sessions, keyed on the popup cwd's
  `repo_key`), session-scoped when the popup cwd isn't in a repo, or `all_sessions`
  (whole socket) when the `s` toggle is on. Returns a `scope_label` for the header so
  the UI does no git/tmux work. Idle worktrees for every repo in view; GC's dead state
  files as a side effect.
- `state.rs` — the on-disk contract: `Status`, the event→status state machine
  (`outcome_for_event`), `$TMUX` parsing, and atomic read/write/GC of state files.
  **The only shared contract between the hook writer and the TUI reader.**
- `config.rs` — the optional TOML config contract (`~/.config/hydra/config.toml`, or
  `$HYDRA_CONFIG`). `Config` = section structs, each with a `Default` matching a built-in
  constant, so a missing/partial/unparseable file yields today's exact behavior. Loaded at
  the entry points (`ui`/`status`/`hook`/`install`) and threaded as plain values into the
  pure core — no globals. Env vars (`HYDRA_WORKTREE_ROOT`, `HYDRA_ALERTS`) are folded into
  `Config` at load, so use sites never re-check env. `agent.spawn_mode` (`"window"` |
  `"session"`) is runtime-read like everything outside `[popup]`: `"session"` spawns each
  worktree into its own two-window session (shell + agent) instead of a window in the
  current session, and starts the popup in all-sessions view so those sessions are
  visible.
- `hook.rs` — `hydra hook <event>`. Deliberately dumb and fast (runs on every event):
  reads hook JSON from stdin + `$TMUX`/`$TMUX_PANE`, writes/removes one state file
  (including `attention`, the Notification's "why input is needed" message). No tmux
  or git calls here.
- `fetcher.rs` — the background fetch worker. Owns the `Caches`, re-runs
  `current_overview` on its own `refresh_ms` tick, streams `Overview` snapshots to
  the TUI over mpsc. Requests coalesce (`wait_for_request`): at most one fetch is
  ever in flight, so a slow `git status` can't queue work or block input.
- `tmux.rs` — all `tmux` CLI wrappers, **parameterized by socket path** so nested
  servers work. `list_panes`, `current_socket/session`, `jump_to`, `send_key`,
  `send_text`. Shells out (no libtmux).
- `worktree.rs` — cwd → branch/repo via `git`, cached by cwd (`WorktreeCache`). Also
  `DirtyCache` (throttled uncommitted-change counts), `WorktreeListCache` +
  `list_worktrees` (throttled `git worktree list` for idle-worktree discovery), the
  `Caches` bundle (with `invalidate()` to force a re-read after a mutation), and
  `default_branch`/`create_worktree`/`remove_worktree`/`is_dirty` for spawn+remove. Repo identity
  (`repo_key`) is the canonicalized common git dir (`abs_common_dir`) so main and
  linked worktrees share one key.
- `agent.rs` — the join. `join_and_sort` (pure: state ⋈ live panes, optional session
  filter, staleness, sort). Display scoping is pure and tested: `Scope`
  (Session/Repo/All), `choose_scope` (toggle + popup repo_key → scope) and
  `matches_scope` (per-agent predicate; a worktree-less agent never matches `Repo`).
  Also `idle_from` (project worktrees − occupied),
  `matches_filter`/`worktree_matches_filter`, `format_age`.
- `ui.rs` — the ratatui popup: `Mode` (Normal/Filter/Send/Spawn/Confirm), vim keys, a
  unified repo-grouped list of both running agents (age/dirty/attention) and idle
  worktrees (`Enter` starts `claude`), `a`/`d`/`1`-`3` prompt replies (NEEDS_INPUT
  gated + state-file re-checked at send time), `x` to remove a worktree (confirm,
  kills every window rooted in the worktree via `agent::windows_under_path` — mode-
  agnostic, so it destroys the whole session in session mode — forces on dirty, keeps
  branch), `s` scope toggle (repo-scoped ⟷ all-sessions), and a colored `capture-pane -e`
  preview (memoized per selection+snapshot). The header scope label comes from the
  snapshot's `scope_label` (no git/tmux on the UI thread). `spawn_mode` (`n`/`Enter`)
  branches between `new_window` and `new_session`+`switch_client`; session mode starts the
  popup in all-sessions view. Data
  arrives as snapshots from `fetcher.rs`; the UI thread polls input at 50 ms and
  never does git/tmux fetch work itself. Selection is tracked by a stable key (agent
  pane id or worktree path). UI behavior is tested via `TestBackend`.
- `status.rs` — `hydra status <socket> <session>`, the daemon-free status-line
  indicator (tmux polls it from `status-right`).
- `alert.rs` — best-effort desktop notification on the transition into NEEDS_INPUT
  (fired from `hook.rs`); fire-and-forget, `HYDRA_ALERTS=0` disables. Shown via the
  cross-platform `notify-rust` crate, but that call *blocks*, so `spawn_notify` runs it
  out-of-process: it launches `hydra notify <title> <body>` (an internal subcommand in
  `main.rs`) detached and returns instantly, keeping the hook cheap.
- `install.rs` — merges hooks into `~/.claude/settings.json` and a marked block into
  `~/.tmux.conf`; both idempotent and reversible.

## Conventions & invariants — don't break these

- **Socket everywhere.** Never call `tmux` without a socket. Every agent self-reports
  its socket via `$TMUX`; always target *that* socket. This is what makes nesting work.
- **The join is authority on liveness.** Don't add separate "is this agent alive?"
  tracking — if a pane isn't in `list_panes`, the agent is gone. Period.
- **`send-keys` must always have an explicit, real pane id.** A defaulted/empty target
  sends keystrokes to the *active* pane (learned the hard way: it typed into a live
  Claude session). `approve`/`deny` are additionally gated on `Status::NeedsInput`.
- **Keep `hook.rs` cheap.** It runs on every Claude Code event. No tmux/git subprocess
  calls, no blocking work.
- **Keep logic pure and out of `ui.rs`.** New behavior goes in `agent.rs`/`state.rs`
  with a unit test; `ui.rs` should stay thin rendering + input.
- **`install`/`uninstall` stay non-destructive.** Hooks merge alongside existing ones
  (identified by a command containing both `hydra` and `hook`); the tmux block is
  marker-delimited; status-right uses `set -ga` (append). Uninstall must fully reverse.
- **Config is read at two points.** Everything is runtime-read (live on rebuild) EXCEPT
  the `[popup]` key/size, which `install` bakes into `~/.tmux.conf` — changing those needs
  a re-`install`. `Caches::invalidate()` must preserve configured TTLs (don't reset via
  `Default`). The starter config is write-if-absent only; `uninstall` never deletes it.
- Run `cargo fmt` before committing.

## Status state machine

`UserPromptSubmit`/`PreToolUse`/`PostToolUse`/`SessionStart` → `WORKING`;
`Notification` → `NEEDS_INPUT`; `Stop` → `IDLE`; `SessionEnd` → removed.
`SubagentStop` also maps to `WORKING` (the *parent* agent is still processing — never
`IDLE`, which would flicker). Staleness downgrades only `WORKING` (not
idle/needs-input, which can legitimately sit) to `UNKNOWN`. Leftover files from
crashed agents (no `SessionEnd`) are GC'd by `current_overview` via
`agent::dead_states`: pane gone from `list_panes` AND older than `GC_GRACE_SECS`.

## Roadmap notes

Phases 1–4 plus the extra features (attention alerts, spawn `n`, triage age/`Δ`/`Tab`,
preview pane) are done. `git status` for dirty counts is throttled via `DirtyCache`
(`DIRTY_TTL_SECS`) so it doesn't run on every fetch tick — keep it that way. Not yet
verified: the cross-socket jump against a real nested tmux (matching logic unit-tested
via `match_pane_by_tty`). The command *forms* for send-keys, spawn (worktree +
new-window), status, and the cross-session jump (`switch-client`, incl. from inside a
popup) are verified against live tmux.

`spawn_mode` (window|session, default `"window"`) is done: `"session"` gives each
worktree a dedicated two-window session (shell + agent), `Enter` reuses an existing
session and lands on the agent window, and removal stays mode-agnostic by killing
every window rooted in the worktree (`agent::windows_under_path`) rather than
special-casing sessions.
