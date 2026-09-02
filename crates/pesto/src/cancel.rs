//! Unified cancellation support for pesto.
//!
//! Provides a single signal listener that flips a shared [`AtomicBool`] on
//! Ctrl-C or SIGTERM. The first signal is graceful; a second signal (or the
//! graceful-shutdown deadline) requests an immediate I/O abort. Library code
//! should **never** install its own signal handler; instead it accepts an
//! `Arc<AtomicBool>` and polls it at safe boundaries.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::sync::OnceLock;
use tokio::sync::Notify;

/// How long a graceful signal may wait for in-flight network I/O before the
/// listener escalates to an abort. Kept deliberately short: NNTP commands may
/// otherwise be waiting on the configured (normally 120 second) timeout.
pub const GRACEFUL_SHUTDOWN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(10);

struct AbortState {
    requested: AtomicBool,
    notify: Notify,
}

static ABORT_STATE: OnceLock<Arc<AbortState>> = OnceLock::new();

fn abort_state() -> Arc<AbortState> {
    ABORT_STATE
        .get_or_init(|| {
            Arc::new(AbortState {
                requested: AtomicBool::new(false),
                notify: Notify::new(),
            })
        })
        .clone()
}

fn request_abort(state: &AbortState) {
    state.requested.store(true, Ordering::Release);
    state.notify.notify_waiters();
}

/// Returns whether a signal listener has escalated graceful cancellation to
/// an immediate abort for this process.
pub fn abort_requested() -> bool {
    abort_state().requested.load(Ordering::Acquire)
}

/// Wait until a second signal, or the graceful-shutdown deadline, requests an
/// immediate abort. This is cancellation-safe and also handles an abort that
/// happened before the caller started waiting.
pub async fn aborted() {
    let state = abort_state();
    while !state.requested.load(Ordering::Acquire) {
        state.notify.notified().await;
    }
}

/// Spawn a background task that listens for Ctrl-C (and SIGTERM on Unix) and
/// sets `flag` to `true` when either fires. A second signal, or a timeout
/// after the first, wakes [`aborted`] so callers can drop in-flight sockets.
///
/// Call this **once** per binary invocation / per run, then pass `flag` to
/// every long-running phase (posting, check, repost, etc.).
pub fn spawn_listener(flag: Arc<AtomicBool>) {
    tokio::spawn(async move {
        #[cfg(unix)]
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("SIGINT handler");
        #[cfg(not(unix))]
        let sigint = async {
            tokio::signal::ctrl_c().await.ok();
        };

        #[cfg(unix)]
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("SIGTERM handler");

        #[cfg(unix)]
        tokio::select! {
            _ = sigint.recv() => {},
            _ = sigterm.recv() => {},
        }
        #[cfg(not(unix))]
        sigint.await;

        flag.store(true, Ordering::Release);

        let state = abort_state();
        #[cfg(unix)]
        tokio::select! {
            _ = sigint.recv() => request_abort(&state),
            _ = sigterm.recv() => request_abort(&state),
            _ = tokio::time::sleep(GRACEFUL_SHUTDOWN_DEADLINE) => request_abort(&state),
        }
        #[cfg(not(unix))]
        tokio::select! {
            _ = tokio::signal::ctrl_c() => request_abort(&state),
            _ = tokio::time::sleep(GRACEFUL_SHUTDOWN_DEADLINE) => request_abort(&state),
        }
    });
}
