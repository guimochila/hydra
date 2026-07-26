//! Background fetch worker: owns the `Caches`, re-runs `current_overview()` on a
//! self-paced tick, and streams `Overview` snapshots to the TUI over a channel — so
//! keyboard input never blocks on tmux/git subprocesses (a cold `git status` in a
//! big repo can take seconds).
//!
//! One worker thread, two mpsc channels, no shared state: the caches move into the
//! thread and snapshots are owned values. Requests are coalesced (see
//! `wait_for_request`), so at most one fetch is ever in flight and a slow fetch can
//! never queue up work behind it. The thread is detached on purpose: this is a
//! popup whose process exits on quit, and dropping the `Fetcher` disconnects both
//! channels, which stops the loop at its next send/receive anyway.

use crate::worktree::Caches;
use crate::Overview;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// What the preview pane should show — captured by the worker so the UI thread never
/// shells out to tmux or git itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreviewTarget {
    /// A running agent: `capture-pane` of its live screen.
    Pane { socket: String, pane_id: String },
    /// An idle worktree: recent git log + uncommitted files.
    Worktree { path: String },
}

/// A UI-initiated refresh. `invalidate` drops the caches first (used after a
/// mutation like a worktree removal); `all_sessions` and `preview` carry the UI's
/// current scope toggle and preview selection so the worker always fetches what the
/// UI wants to show.
pub struct Request {
    pub invalidate: bool,
    pub all_sessions: bool,
    pub preview: Option<PreviewTarget>,
}

/// UI-side handle. Dropping it disconnects both channels, which stops the worker.
pub struct Fetcher {
    req_tx: Sender<Request>,
    /// Overview snapshots, oldest first — the UI drains and keeps the newest.
    pub snap_rx: Receiver<Overview>,
}

impl Fetcher {
    /// Ask for an immediate fetch. Send errors are ignored: the worker being gone
    /// means the app is already exiting.
    pub fn request_refresh(
        &self,
        invalidate: bool,
        all_sessions: bool,
        preview: Option<PreviewTarget>,
    ) {
        let _ = self.req_tx.send(Request {
            invalidate,
            all_sessions,
            preview,
        });
    }
}

/// What woke the worker.
#[derive(Debug, PartialEq, Eq)]
enum Wake {
    /// The tick elapsed with no request — do the periodic refetch.
    Tick,
    /// One or more requests arrived, coalesced into a single wake.
    Refresh {
        invalidate: bool,
        all_sessions: bool,
        preview: Option<PreviewTarget>,
    },
    /// The UI dropped its handle — exit.
    Disconnected,
}

/// Block up to `tick` for a request, then drain everything else queued, OR-ing the
/// `invalidate` flags and keeping the latest `all_sessions`/`preview`. This
/// coalescing is the backpressure property: if a fetch took 3 s, any number of `r`
/// presses and ticks that piled up meanwhile fold into exactly one follow-up fetch.
fn wait_for_request(rx: &Receiver<Request>, tick: Duration) -> Wake {
    let first = match rx.recv_timeout(tick) {
        Ok(r) => r,
        Err(RecvTimeoutError::Timeout) => return Wake::Tick,
        Err(RecvTimeoutError::Disconnected) => return Wake::Disconnected,
    };
    let mut invalidate = first.invalidate;
    let mut all_sessions = first.all_sessions;
    let mut preview = first.preview;
    while let Ok(r) = rx.try_recv() {
        invalidate |= r.invalidate;
        all_sessions = r.all_sessions;
        preview = r.preview;
    }
    Wake::Refresh {
        invalidate,
        all_sessions,
        preview,
    }
}

/// How long a worktree's git preview stays fresh. A pane capture is one cheap tmux
/// call and is redone every fetch; `git log` + `git status` are not, so they are
/// memoized briefly instead of running on every 250 ms tick.
const WORKTREE_PREVIEW_TTL_SECS: u64 = 2;

/// The worker's preview memo: what was last fetched, when, and its text.
struct PreviewMemo {
    target: PreviewTarget,
    fetched_at: u64,
    text: String,
}

/// Produce the preview text for `target`, reusing `memo` when it is still fresh
/// (worktree previews only — pane captures are cheap enough to redo every fetch).
fn fetch_preview(target: &PreviewTarget, memo: &mut Option<PreviewMemo>, now: u64) -> String {
    if let Some(m) = memo.as_ref() {
        let fresh = matches!(target, PreviewTarget::Worktree { .. })
            && m.target == *target
            && now.saturating_sub(m.fetched_at) < WORKTREE_PREVIEW_TTL_SECS;
        if fresh {
            return m.text.clone();
        }
    }
    let text = match target {
        PreviewTarget::Pane { socket, pane_id } => crate::tmux::capture_pane(socket, pane_id),
        PreviewTarget::Worktree { path } => crate::worktree::preview(path),
    };
    *memo = Some(PreviewMemo {
        target: target.clone(),
        fetched_at: now,
        text: text.clone(),
    });
    text
}

/// Spawn the detached worker thread. Takes ownership of `caches`; config values are
/// threaded in as plain values, per the project convention. Fetches immediately so
/// the first snapshot lands as soon as possible after the popup opens.
pub fn spawn(
    mut caches: Caches,
    refresh_ms: u64,
    stale_after_secs: u64,
    initial_all_sessions: bool,
) -> Fetcher {
    let (req_tx, req_rx) = mpsc::channel::<Request>();
    let (snap_tx, snap_rx) = mpsc::channel::<Overview>();
    let tick = Duration::from_millis(refresh_ms.max(1));
    let _ = std::thread::Builder::new()
        .name("hydra-fetch".into())
        .spawn(move || {
            let mut all_sessions = initial_all_sessions;
            let mut preview: Option<PreviewTarget> = None;
            let mut memo: Option<PreviewMemo> = None;
            loop {
                let mut overview =
                    crate::current_overview(&mut caches, stale_after_secs, all_sessions);
                if let Some(target) = &preview {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0);
                    overview.preview =
                        Some((target.clone(), fetch_preview(target, &mut memo, now)));
                }
                if snap_tx.send(overview).is_err() {
                    return; // UI dropped the receiver — app is exiting
                }
                match wait_for_request(&req_rx, tick) {
                    Wake::Tick => {}
                    Wake::Refresh {
                        invalidate,
                        all_sessions: scope,
                        preview: target,
                    } => {
                        if invalidate {
                            caches.invalidate(); // TTL-preserving (see worktree.rs)
                        }
                        all_sessions = scope;
                        preview = target;
                    }
                    Wake::Disconnected => return,
                }
            }
        });
    Fetcher { req_tx, snap_rx }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A Receiver works synchronously, so `wait_for_request` is testable without
    // spawning the worker thread.

    #[test]
    fn coalesces_queued_requests_oring_invalidate() {
        let (tx, rx) = mpsc::channel::<Request>();
        tx.send(Request {
            invalidate: false,
            all_sessions: false,
            preview: None,
        })
        .unwrap();
        tx.send(Request {
            invalidate: true,
            all_sessions: false,
            preview: Some(PreviewTarget::Pane {
                socket: "/s".into(),
                pane_id: "%1".into(),
            }),
        })
        .unwrap();
        tx.send(Request {
            invalidate: false,
            all_sessions: true, // latest scope wins…
            preview: Some(PreviewTarget::Worktree {
                path: "/wt".into(), // …and so does the latest preview target
            }),
        })
        .unwrap();
        assert_eq!(
            wait_for_request(&rx, Duration::from_millis(10)),
            Wake::Refresh {
                invalidate: true,
                all_sessions: true,
                preview: Some(PreviewTarget::Worktree { path: "/wt".into() }),
            }
        );
        // Everything was drained — the next wait times out into a Tick.
        assert_eq!(wait_for_request(&rx, Duration::from_millis(1)), Wake::Tick);
    }

    #[test]
    fn times_out_into_a_tick_when_no_requests_arrive() {
        let (_tx, rx) = mpsc::channel::<Request>();
        assert_eq!(wait_for_request(&rx, Duration::from_millis(1)), Wake::Tick);
    }

    #[test]
    fn disconnects_when_the_ui_handle_is_dropped() {
        let (tx, rx) = mpsc::channel::<Request>();
        drop(tx);
        assert_eq!(
            wait_for_request(&rx, Duration::from_millis(1)),
            Wake::Disconnected
        );
    }
}
