//! Unified cancellation support for pesto.
//!
//! Provides a single signal listener that flips a shared [`AtomicBool`] on
//! Ctrl-C or SIGTERM.  Library code should **never** install its own signal
//! handler; instead it accepts an `Arc<AtomicBool>` and polls it at safe
//! boundaries.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Spawn a background task that listens for Ctrl-C (and SIGTERM on Unix) and
/// sets `flag` to `true` when either fires.
///
/// Call this **once** per binary invocation / per run, then pass `flag` to
/// every long-running phase (posting, check, repost, etc.).
pub fn spawn_listener(flag: Arc<AtomicBool>) {
    tokio::spawn(async move {
        let ctrl_c = async {
            tokio::signal::ctrl_c().await.ok();
        };
        #[cfg(unix)]
        let sigterm = async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler")
                .recv()
                .await;
        };
        #[cfg(not(unix))]
        let sigterm = std::future::pending::<()>();

        tokio::select! {
            _ = ctrl_c => {},
            _ = sigterm => {},
        }
        flag.store(true, Ordering::Relaxed);
    });
}
