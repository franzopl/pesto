//! Posting pipeline: cancel forwarding, worker spawn and producer join.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tracing::{error, info};

use crate::nntp::pool::ConnectionSlot;
use crate::progress::ProgressEvent;

use super::producer::producer;
use super::shared::Shared;
use super::task::{PostTask, TaskDispatcher};
use super::worker::{encode_worker, worker};
use super::{encode_concurrency, post_pregenerated_release, ready_queue_depth, FileMeta};
/// Forward external cancel/pause flags into `Shared` until the run ends,
/// emitting the matching progress events.
pub(super) fn spawn_cancel_watcher(
    shared: &Arc<Shared>,
    external_cancel: Option<Arc<AtomicBool>>,
    external_pause: Option<Arc<AtomicBool>>,
) -> tokio::task::JoinHandle<()> {
    let shared = shared.clone();
    tokio::spawn(async move {
        if external_cancel.is_none() && external_pause.is_none() {
            std::future::pending::<()>().await;
        }
        loop {
            if let Some(ref flag) = external_cancel {
                if flag.load(Ordering::Relaxed) {
                    shared.cancelled.store(true, Ordering::Relaxed);
                    shared.emit(ProgressEvent::Interrupted);
                    return;
                }
            }
            if let Some(ref flag) = external_pause {
                let want = flag.load(Ordering::Relaxed);
                if shared.paused.swap(want, Ordering::Relaxed) != want {
                    shared.emit(if want {
                        ProgressEvent::Paused
                    } else {
                        ProgressEvent::Resumed
                    });
                }
            }
            tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
        }
    })
}

/// The spawned posting pipeline: one task per POST worker, the encode pool and
/// the producer-facing dispatcher.
pub(super) struct Pipeline {
    pub(super) handles: Vec<tokio::task::JoinHandle<ConnectionSlot>>,
    pub(super) encode_handles: Vec<tokio::task::JoinHandle<()>>,
    pub(super) tx_opt: Option<TaskDispatcher<super::task::PostTask>>,
}

/// Spawn the POST workers and yEnc encode workers, returning their join
/// handles and the dispatcher the producer feeds.
pub(super) fn start_pipeline(
    shared: &Arc<Shared>,
    worker_count: usize,
    post_slots: &mut Vec<ConnectionSlot>,
) -> Pipeline {
    let mut handles = Vec::with_capacity(worker_count);
    let mut encode_handles = Vec::new();
    let tx_opt = if worker_count > 0 && !post_slots.is_empty() {
        let spawn_n = worker_count.min(post_slots.len());
        let ready_n = ready_queue_depth(spawn_n);
        let post_depth = (ready_n / spawn_n).max(2);
        let mut post_senders = Vec::with_capacity(spawn_n);
        let mut post_receivers = Vec::with_capacity(spawn_n);
        for _ in 0..spawn_n {
            let (tx, rx) = tokio::sync::mpsc::channel(post_depth);
            post_senders.push(tx);
            post_receivers.push(rx);
        }
        let post_disp = Arc::new(TaskDispatcher::new(post_senders));
        let spawned: Vec<_> = post_slots.drain(..spawn_n).collect();
        for (idx, (slot, rx)) in spawned.into_iter().zip(post_receivers).enumerate() {
            handles.push(tokio::spawn(worker(shared.clone(), rx, idx, slot)));
        }

        let n_enc = encode_concurrency(parmesan::performance_core_count(), worker_count);
        let enc_depth = (ready_n / n_enc).max(2);
        let mut enc_senders = Vec::with_capacity(n_enc);
        let mut enc_receivers = Vec::with_capacity(n_enc);
        for _ in 0..n_enc {
            let (tx, rx) = tokio::sync::mpsc::channel(enc_depth);
            enc_senders.push(tx);
            enc_receivers.push(rx);
        }
        info!(
            encode_workers = n_enc,
            ready_queue = ready_n,
            "article encode pool"
        );
        for rx in enc_receivers {
            let shared = shared.clone();
            let post_disp = post_disp.clone();
            encode_handles.push(tokio::spawn(async move {
                encode_worker(shared, rx, post_disp).await;
            }));
        }
        Some(TaskDispatcher::new(enc_senders))
    } else {
        None
    };
    Pipeline {
        handles,
        encode_handles,
        tx_opt,
    }
}

/// Run the producer (or the pre-generated-release poster in
/// `--par2-before-upload` mode), join the encode and POST workers, and return
/// the final force-abort/failure state plus the POST slots recovered from the
/// joined workers.
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_pipeline(
    shared: &Arc<Shared>,
    metas: Vec<Arc<FileMeta>>,
    tx_opt: Option<TaskDispatcher<PostTask>>,
    will_defer: bool,
    par2_dir: PathBuf,
    recovery_count: usize,
    total_conns: usize,
    mut failure_reason: Option<String>,
    mut handles: Vec<tokio::task::JoinHandle<ConnectionSlot>>,
    mut encode_handles: Vec<tokio::task::JoinHandle<()>>,
    mut post_slots: Vec<ConnectionSlot>,
) -> (bool, Option<String>, Vec<ConnectionSlot>) {
    // The second signal (or the first signal's deadline) is deliberately
    // handled here rather than inside every NNTP read/write: aborting these
    // tasks drops their `TcpStream`s immediately, while preserving the
    // single final resume-state persistence path below.
    let mut force_abort = crate::cancel::abort_requested();
    if force_abort {
        shared.cancelled.store(true, Ordering::Release);
        shared.emit(ProgressEvent::Aborted);
    }

    // Producer (or, when PAR2 was already generated above, the
    // already-generated-files poster) runs in this thread. Skipped entirely
    // if the generation phase above already failed — nothing valid to post.
    if failure_reason.is_none() && !force_abort {
        let producer_shared = shared.clone();
        let producer_result: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<()>> + Send>,
        > = if will_defer {
            Box::pin(async move {
                match tx_opt.as_ref() {
                    Some(tx) => {
                        post_pregenerated_release(
                            &metas,
                            &par2_dir,
                            recovery_count,
                            tx,
                            &producer_shared,
                        )
                        .await
                    }
                    None => Ok(()),
                }
                // `tx_opt` is owned by this future, so it closes here
                // even when a force-abort cancels the future.
            })
        } else {
            Box::pin(producer(metas, tx_opt, producer_shared, total_conns))
        };
        let result = tokio::select! {
            result = producer_result => Some(result),
            _ = crate::cancel::aborted() => {
                force_abort = true;
                None
            }
        };
        if force_abort {
            shared.cancelled.store(true, Ordering::Release);
            shared.emit(ProgressEvent::Aborted);
        } else if let Some(Err(e)) = result {
            let description = format!("producer error: {e:#}");
            // `Failed` alone only reaches `--output-format json` consumers; log
            // it too so the reason survives in the session log file even when
            // the human-readable renderer (which only shows it via `Failed`,
            // see `ui::terminal`) is what's on screen.
            error!(error = %e, "producer error");
            shared.cancelled.store(true, Ordering::Relaxed);
            shared.emit(ProgressEvent::Failed {
                description: description.clone(),
            });
            failure_reason = Some(description);
        }
    }

    if !force_abort {
        while let Some(mut handle) = encode_handles.pop() {
            tokio::select! {
                _ = &mut handle => {},
                _ = crate::cancel::aborted() => {
                    handle.abort();
                    force_abort = true;
                    shared.cancelled.store(true, Ordering::Release);
                    shared.emit(ProgressEvent::Aborted);
                    break;
                }
            }
        }
    }
    if force_abort {
        for handle in encode_handles {
            handle.abort();
        }
        for handle in handles {
            handle.abort();
        }
        shared.cancelled.store(true, Ordering::Release);
    } else {
        while let Some(mut handle) = handles.pop() {
            tokio::select! {
                result = &mut handle => {
                    if let Ok(slot) = result {
                        post_slots.push(slot);
                    }
                }
                _ = crate::cancel::aborted() => {
                    handle.abort();
                    force_abort = true;
                    break;
                }
            }
        }
        if force_abort {
            for handle in handles {
                handle.abort();
            }
            shared.cancelled.store(true, Ordering::Release);
            shared.emit(ProgressEvent::Aborted);
        }
    }

    (force_abort, failure_reason, post_slots)
}
