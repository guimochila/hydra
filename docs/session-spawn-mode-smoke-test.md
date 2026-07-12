# Session spawn mode — manual smoke test

`spawn_mode = "session"` behavior shells out to a live tmux server and drives the
interactive popup, so it isn't covered by the unit tests. Run this checklist by hand
after building, whenever the spawn/teardown paths change. Each step notes which
behavior it validates.

## Setup

```toml
# ~/.config/hydra/config.toml  (or $HYDRA_CONFIG)
[agent]
spawn_mode = "session"
```

```sh
cargo build --release
# ensure the popup runs THIS binary (the installed path), then open it: prefix + a
```

Keep a second pane handy to observe tmux state:

```sh
tmux ls                          # sessions
tmux list-windows -t <session>   # windows within a session
```

Have **two different git repos** available (`repoA`, `repoB`) to test repo-scoping.

---

## A. `n` creates a dedicated 2-window session, stays in the popup

1. Open the popup from inside `repoA`. Press `n`, type a branch name (e.g. `smoke-1`), Enter.
2. Footer shows `✓ spawned smoke-1 in session repoA-smoke-1`; **popup stays open**.
3. Observer pane: `tmux ls` shows session `repoA-smoke-1`; `tmux list-windows -t repoA-smoke-1`
   shows **window 1 = `shell`, window 2 = the agent** (running `claude`).
4. Spawn a second one (`n` → `smoke-2`) to confirm you can queue several without the popup closing.

_Covers: session-mode `n`, two-window layout, repo-scoped name._

## B. `Enter` starts an idle worktree and lands on the Claude window

1. Select an **idle worktree** row, press `Enter`.
2. Popup **closes** and you land in that worktree's session **on window 2 (Claude)** — not the shell.
3. `tmux display-message -p '#S #W'` confirms you're on the agent window.

_Covers: session-mode `Enter`, landing on window 2._

## C. `Enter` on an existing session reuses it (no duplicate)

1. Re-open the popup, select the **same** worktree/agent you just started, press `Enter` again.
2. You're switched back to that existing session — **no** "duplicate session" error, no second
   session in `tmux ls`.
3. You land on the Claude window (window 2), not the shell.

_Covers: `session_exists` idempotency + landing-on-Claude after reuse._

## D. Repo-scoping: same branch name in two repos doesn't collide

1. Spawn `n` → branch `main` (or any shared name) from **repoA**.
2. Spawn `n` → same branch name from **repoB**.
3. `tmux ls` shows **two** sessions: `repoA-main` and `repoB-main` (not one shared session,
   no "already exists" after creating a worktree).
4. `Enter` on each → you land in the **correct repo's** session.

_Covers: repo-scoped session names._

## E. All-sessions default + idle occupancy

1. Fresh popup in session mode → it opens in **all-sessions view** (rows show `session:win`),
   so the agents you spawned are visible.
2. With a session-mode agent running in its **own** session, find its worktree in the list.
3. It shows as a **running agent**, **not** an idle "start ⏎" row.
4. Press `s` to toggle to **single-session** view.
5. That worktree **still does not** appear as an idle row (even though its agent lives in
   another session).

_Covers: all-session idle occupancy + all-sessions default in session mode._

## F. `x` tears down the whole session — including under `renumber-windows on`

1. **First set the risky option** so you test the fixed path:
   ```sh
   tmux set-option -g renumber-windows on
   ```
2. Select a session-mode agent (or its worktree), press `x`, confirm `y`.
3. **Both** windows die and the whole session disappears from `tmux ls` (not just the Claude window).
4. The worktree is removed (`git worktree list` no longer shows it), branch kept.
5. No `kill window failed` footer error.
6. (Optional) Try `x` on a **dirty** worktree → it should force-remove and still tear the session down.

_Covers: mode-agnostic teardown + the highest-window-index-first fix (renumber-windows)._

## G. Jump to a running session-mode agent (cross-session)

1. With several agents across sessions, select one in a **different** session than the popup's,
   press `Enter`.
2. You're switched to that agent's session and its Claude window.

_Covers: cross-session `switch-client` jump._

## H. Window-mode regression check

1. Set `spawn_mode = "window"` (or remove it), reopen the popup (no rebuild needed — runtime-read).
2. `n` and `Enter` create a **window in the current session** (no new session in `tmux ls`);
   `Enter` lands you on that window; `x` kills just that window and the surrounding session /
   other windows survive.
3. Popup opens **session-scoped** (not all-sessions) by default.

_Covers: no behavior change for existing users._

---

## Cleanup

- `tmux set-option -g renumber-windows off` if you changed it and don't normally run it.
- Remove any leftover `smoke-*` worktrees/sessions.

## Not reproducible on demand

`new_session`'s rollback (kill the shell-only session if the agent window can't be created)
only triggers when `new-window` fails after `new-session` succeeds — not forceable from the
CLI. It's wired and the `kill_session` helper is verified; no action needed unless you ever
see a stray `<repo>-<branch>` session with only a `shell` window after a spawn error.
