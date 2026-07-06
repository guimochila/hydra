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

There is no test harness for the interactive TUI (ratatui needs a real terminal). The
data/logic layers are pure and unit-tested; the UI is thin rendering over them. To
verify behavior end-to-end, use `hydra ls` and `hydra status` (headless), or drive the
popup by hand in tmux.

## Architecture

Data flows one way: hooks **write** per-pane state files; the TUI **reads** them and
joins against live tmux. See `README.md` "How it works" for the high-level picture.

Module map (`src/`):

- `main.rs` — CLI dispatch (`hydra`, `ls`, `status`, `hook`, `install`, `uninstall`)
  and `current_agents()`, the shared "resolve socket+session → agents" helper.
- `state.rs` — the on-disk contract: `Status`, the event→status state machine
  (`outcome_for_event`), `$TMUX` parsing, and atomic read/write/GC of state files.
  **The only shared contract between the hook writer and the TUI reader.**
- `hook.rs` — `hydra hook <event>`. Deliberately dumb and fast (runs on every event):
  reads hook JSON from stdin + `$TMUX`/`$TMUX_PANE`, writes/removes one state file. No
  tmux or git calls here.
- `tmux.rs` — all `tmux` CLI wrappers, **parameterized by socket path** so nested
  servers work. `list_panes`, `current_socket/session`, `jump_to`, `send_key`,
  `send_text`. Shells out (no libtmux).
- `worktree.rs` — cwd → branch/repo via `git`, cached by cwd (`WorktreeCache`).
- `agent.rs` — the join. `join_and_sort` (pure: state ⋈ live panes, filter to session,
  staleness, sort) and `collect` (adds worktree). Also `group_by_repo`, `matches_filter`.
- `ui.rs` — the ratatui popup: `Mode` (Normal/Filter/Send), vim keys, repo-grouped
  rows, 250 ms refresh tick.
- `status.rs` — `hydra status <socket> <session>`, the daemon-free status-line
  indicator (tmux polls it from `status-right`).
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
- Run `cargo fmt` before committing.

## Status state machine

`UserPromptSubmit`/`PreToolUse`/`SessionStart` → `WORKING`; `Notification` →
`NEEDS_INPUT`; `Stop` → `IDLE`; `SessionEnd` → removed. Staleness downgrades only
`WORKING` (not idle/needs-input, which can legitimately sit) to `UNKNOWN`.

## Roadmap notes

Phases 1–4 are done. Not yet verified: the interactive popup driven by a real human
keypress, and the cross-socket jump against a real nested tmux (its matching logic is
unit-tested via `match_pane_by_tty`).
