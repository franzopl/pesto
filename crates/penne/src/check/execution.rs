use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use pesto::config::ServerEntry;
use tokio::task::JoinSet;

use super::plan::{ItemResolution, WorkItem};
use super::{CheckConfig, CheckMethod, CheckProgress, CheckProgressSender};
use crate::client::DownloadClient;
use crate::config::ServerTier;

fn emit(progress: &Option<CheckProgressSender>, present: bool) {
    if let Some(tx) = progress {
        let _ = tx.send(CheckProgress { present });
    }
}

/// Drain `pending` against every member of `tier`, each contributing its
/// own `connections` concurrent workers to one shared queue — see
/// [`crate::config::ServerTier`]. Returns `(found, leftover, bytes_used)`:
/// `found` is everything this tier confirmed present; `leftover` is
/// everything nobody in it did (missing, or a STAT attempt that exhausted
/// its retries), for the next tier in priority order to try; `bytes_used`
/// is every byte sent or received across every connection this tier's
/// workers opened.
///
/// `progress` is threaded down into each worker so a "present" event fires
/// the instant *that item* resolves — not batched up and only emitted once
/// this whole function returns. Every worker (across every member server)
/// pulls from one shared queue and only stops once it's empty, so without
/// per-item emission every worker's task finishes within the same instant
/// at the very end of the pass regardless of queue size, making the
/// progress bar sit still for the entire check and then jump straight to
/// 100%.
///
/// `is_last_tier` matters for the same reason: a "missing" verdict is only
/// final once every configured tier has had a turn, but for the *last*
/// one, that's true the instant each item resolves — so on the last tier,
/// a "missing" event fires per-item too, instead of every leftover item
/// silently piling up for a single batch emitted after this whole function
/// (and therefore the whole multi-tier check) returns. On a
/// single-tier setup (the common case) every pass is the last pass, so
/// this is the difference between a release with many missing articles
/// updating the bar throughout the check or not moving until the very end.
pub(super) async fn drain_one_tier(
    tier: &ServerTier,
    pending: Vec<WorkItem>,
    config: &CheckConfig,
    progress: &Option<CheckProgressSender>,
    is_last_tier: bool,
    stop: Arc<AtomicBool>,
    item_tx: Option<tokio::sync::mpsc::UnboundedSender<ItemResolution>>,
) -> (Vec<WorkItem>, Vec<WorkItem>, u64) {
    let queue = Arc::new(Mutex::new(VecDeque::from(pending)));

    // The *tier-wide* worker count, not any one member's own — worker_loop's
    // fair-share batching (see its own doc comment) needs to know how many
    // workers are actually pulling from the shared queue in total, or a
    // pooled tier's later members would starve exactly the way un-pooled
    // servers used to before that fix.
    let worker_count: usize = tier.members.iter().map(|s| s.connections.max(1)).sum();

    let method = config.method;
    let retries = config.retries;
    let pipeline_depth = config.pipeline_depth.clamp(1, 256);

    let mut workers = JoinSet::new();
    let breaker = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    for server in &tier.members {
        for _ in 0..server.connections.max(1) {
            workers.spawn(worker_loop(
                queue.clone(),
                server.clone(),
                method,
                retries,
                pipeline_depth,
                progress.clone(),
                is_last_tier,
                config.fail_fast,
                stop.clone(),
                worker_count,
                breaker.clone(),
                item_tx.clone(),
            ));
        }
    }

    let mut found = Vec::new();
    let mut leftover = Vec::new();
    let mut bytes_used = 0u64;
    while let Some(result) = workers.join_next().await {
        if let Ok((f, l, b)) = result {
            found.extend(f);
            leftover.extend(l);
            bytes_used += b;
        }
    }

    // Any items left in the shared queue were abandoned by workers that exited
    // early (e.g., due to fatal connection errors) — never a definitive
    // 430/423/420, so these are unreachable unless an earlier tier already
    // confirmed them missing (`WorkItem::confirmed_missing`).
    let mut q = queue.lock().unwrap();
    while let Some(item) = q.pop_front() {
        if stop.load(Ordering::Relaxed) {
            leftover.push(item);
            continue;
        }
        if is_last_tier {
            emit(progress, false);
            if let Some(tx) = &item_tx {
                let resolution = if item.confirmed_missing {
                    ItemResolution::Missing(item.clone())
                } else {
                    ItemResolution::Unreachable(item.clone())
                };
                let _ = tx.send(resolution);
            }
        }
        leftover.push(item);
    }

    (found, leftover, bytes_used)
}

/// One worker's whole pass over `queue`: pop a batch, pipeline-`STAT` it
/// against `server` in one round trip (retrying the *whole batch* per
/// [`stat_batch_with_retry`] on a connection/transport error), repeat until
/// the queue is empty. Keeps one connection open for the entire pass.
///
/// Each pop takes at most `pipeline_depth` items, *and* never more
/// than a `worker_count`-th of whatever's left in the queue right then —
/// without that second cap, a worker that wins the lock first could grab
/// the entire remaining queue in one batch whenever it's no bigger than
/// `pipeline_depth` (always eventually true, since every queue drains
/// to nothing), leaving every other worker with nothing to do and
/// defeating `server.connections` concurrency right when it matters most:
/// finishing the tail of a check together instead of one connection
/// mopping it up alone.
///
/// Emits a "present" [`CheckProgress`] event as soon as each item resolves
/// present. A "missing" event fires per-item too, but only when
/// `is_last_server` — otherwise a segment this server doesn't have might
/// still turn up on the next one, so it's not a final answer yet.
/// Dispatches to [`stat_worker_loop`] (the existing, pipelined-batch
/// path — untouched) for [`CheckMethod::Stat`], or [`single_item_worker_
/// loop`] for [`CheckMethod::Head`]/[`CheckMethod::Body`] — see the module
/// doc comment for why the latter two deliberately aren't pipelined.
#[allow(clippy::too_many_arguments)]
async fn worker_loop(
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    server: ServerEntry,
    method: CheckMethod,
    retries: u32,
    pipeline_depth: usize,
    progress: Option<CheckProgressSender>,
    is_last_server: bool,
    fail_fast: bool,
    stop: Arc<AtomicBool>,
    worker_count: usize,
    breaker: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    item_tx: Option<tokio::sync::mpsc::UnboundedSender<ItemResolution>>,
) -> (Vec<WorkItem>, Vec<WorkItem>, u64) {
    match method {
        CheckMethod::Stat => {
            stat_worker_loop(
                queue,
                server,
                retries,
                pipeline_depth,
                progress,
                is_last_server,
                fail_fast,
                stop,
                worker_count,
                breaker,
                item_tx,
            )
            .await
        }
        CheckMethod::Head | CheckMethod::Body => {
            single_item_worker_loop(
                queue,
                server,
                method,
                retries,
                progress,
                is_last_server,
                fail_fast,
                stop,
                breaker,
                item_tx,
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn stat_worker_loop(
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    server: ServerEntry,
    retries: u32,
    pipeline_depth: usize,
    progress: Option<CheckProgressSender>,
    is_last_server: bool,
    fail_fast: bool,
    stop: Arc<AtomicBool>,
    worker_count: usize,
    breaker: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    item_tx: Option<tokio::sync::mpsc::UnboundedSender<ItemResolution>>,
) -> (Vec<WorkItem>, Vec<WorkItem>, u64) {
    let mut client: Option<DownloadClient> = None;
    let mut found = Vec::new();
    let mut leftover = Vec::new();
    let mut bytes_used = 0u64;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if breaker.load(std::sync::atomic::Ordering::Relaxed) >= 5 {
            break;
        }
        let batch: Vec<WorkItem> = {
            let mut q = queue.lock().expect("queue mutex poisoned");
            let fair_share = q.len().div_ceil(worker_count.max(1));
            let n = pipeline_depth.min(q.len()).min(fair_share);
            q.drain(..n).collect()
        };
        if batch.is_empty() {
            break;
        }

        match stat_batch_with_retry(&mut client, &server, &batch, retries, &mut bytes_used).await {
            Ok(results) => {
                breaker.store(0, std::sync::atomic::Ordering::Relaxed);
                for (mut item, present) in batch.into_iter().zip(results) {
                    if present {
                        emit(&progress, true);
                        if let Some(tx) = &item_tx {
                            let _ = tx.send(ItemResolution::Found(item.clone()));
                        }
                        found.push(item);
                    } else {
                        // A real `430`/`423`/`420` — a definitive answer,
                        // never downgraded by a later tier's connection
                        // failure (see `WorkItem::confirmed_missing`).
                        item.confirmed_missing = true;
                        if is_last_server {
                            emit(&progress, false);
                            if let Some(tx) = &item_tx {
                                let _ = tx.send(ItemResolution::Missing(item.clone()));
                            }
                            if fail_fast {
                                stop.store(true, Ordering::Relaxed);
                            }
                        }
                        leftover.push(item);
                    }
                }
            }
            Err(_) => {
                // The whole batch failed to connect/transfer — no server
                // actually answered, so this is unreachable, not missing,
                // unless an earlier tier already confirmed it.
                breaker.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if is_last_server {
                    for item in &batch {
                        emit(&progress, false);
                        if let Some(tx) = &item_tx {
                            let resolution = if item.confirmed_missing {
                                ItemResolution::Missing(item.clone())
                            } else {
                                ItemResolution::Unreachable(item.clone())
                            };
                            let _ = tx.send(resolution);
                        }
                    }
                }
                leftover.extend(batch);
            }
        }
    }

    if let Some(c) = client {
        bytes_used += c.bytes_written() + c.bytes_read();
        c.quit().await;
    }

    (found, leftover, bytes_used)
}

/// One worker's whole pass over `queue` for [`CheckMethod::Head`]/
/// [`CheckMethod::Body`]: pop a single item, fetch it (retrying per
/// [`single_item_with_retry`]), repeat until the queue is empty. Keeps one
/// connection open for the entire pass, mirroring [`crate::download::
/// worker_loop`]'s shape — deliberately simpler than [`stat_worker_loop`]'s
/// batch pipelining, which pays off far less once a real (if small, for
/// `Head`) payload is involved. Emits progress the same way
/// `stat_worker_loop` does: "present" the instant an item resolves,
/// "missing" per-item only when `is_last_server`.
#[allow(clippy::too_many_arguments)]
async fn single_item_worker_loop(
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    server: ServerEntry,
    method: CheckMethod,
    retries: u32,
    progress: Option<CheckProgressSender>,
    is_last_server: bool,
    fail_fast: bool,
    stop: Arc<AtomicBool>,
    breaker: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    item_tx: Option<tokio::sync::mpsc::UnboundedSender<ItemResolution>>,
) -> (Vec<WorkItem>, Vec<WorkItem>, u64) {
    let mut client: Option<DownloadClient> = None;
    let mut found = Vec::new();
    let mut leftover = Vec::new();
    let mut bytes_used = 0u64;

    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        if breaker.load(std::sync::atomic::Ordering::Relaxed) >= 5 {
            break;
        }
        let item = {
            let mut q = queue.lock().expect("queue mutex poisoned");
            q.pop_front()
        };
        let Some(mut item) = item else { break };

        let present = single_item_with_retry(
            &mut client,
            &server,
            method,
            &item.message_id,
            retries,
            &mut bytes_used,
        )
        .await;

        match present {
            Ok(true) => {
                breaker.store(0, std::sync::atomic::Ordering::Relaxed);
                emit(&progress, true);
                if let Some(tx) = &item_tx {
                    let _ = tx.send(ItemResolution::Found(item.clone()));
                }
                found.push(item);
            }
            Ok(false) => {
                // A real `423`/`430` — definitive, never downgraded by a
                // later tier's connection failure.
                breaker.store(0, std::sync::atomic::Ordering::Relaxed);
                item.confirmed_missing = true;
                if is_last_server {
                    emit(&progress, false);
                    if let Some(tx) = &item_tx {
                        let _ = tx.send(ItemResolution::Missing(item.clone()));
                    }
                    if fail_fast {
                        stop.store(true, Ordering::Relaxed);
                    }
                }
                leftover.push(item);
            }
            Err(_) => {
                // Connection/transport failure after exhausting retries —
                // nobody actually answered, so unreachable, not missing,
                // unless an earlier tier already confirmed it.
                breaker.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if is_last_server {
                    emit(&progress, false);
                    if let Some(tx) = &item_tx {
                        let resolution = if item.confirmed_missing {
                            ItemResolution::Missing(item.clone())
                        } else {
                            ItemResolution::Unreachable(item.clone())
                        };
                        let _ = tx.send(resolution);
                    }
                }
                leftover.push(item);
            }
        }
    }

    if let Some(c) = client {
        bytes_used += c.bytes_written() + c.bytes_read();
        c.quit().await;
    }

    (found, leftover, bytes_used)
}

/// Fetch one item (via `HEAD` or `BODY`, per `method`) against `server`
/// over `client` (connected lazily on first use), retrying up to `retries`
/// times on a connection/transport error — mirrors [`crate::download::
/// fetch_with_retry`]'s shape exactly, just returning presence instead of
/// the decoded bytes. `Ok(None)`-equivalent (`430`, not present) is never
/// retried — that's a definitive answer, not a transient failure.
///
/// Bytes are added to `bytes_used` only when a connection is discarded
/// (on error) — the final tally happens when the worker's own connection
/// is dropped at the end of its pass, exactly like [`stat_batch_with_
/// retry`]'s accounting, so nothing is double-counted.
async fn single_item_with_retry(
    client: &mut Option<DownloadClient>,
    server: &ServerEntry,
    method: CheckMethod,
    message_id: &str,
    retries: u32,
    bytes_used: &mut u64,
) -> Result<bool> {
    let mut last_err = None;

    for attempt in 0..=retries {
        if attempt > 0 {
            let base = server.retry_delay.max(1) as f64;
            let backoff = base * (2.0_f64.powi(attempt as i32 - 1));
            tokio::time::sleep(Duration::from_secs_f64(backoff.min(60.0))).await;
        }

        if client.is_none() {
            match DownloadClient::connect(server).await {
                Ok(c) => *client = Some(c),
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            }
        }

        let c = client.as_mut().expect("just connected above");
        let result = match method {
            CheckMethod::Head => c.head(message_id).await,
            CheckMethod::Body => c.body(message_id).await,
            CheckMethod::Stat => unreachable!("Stat uses stat_worker_loop, not this function"),
        };
        match result {
            Ok(present) => return Ok(present.is_some()),
            Err(e) => {
                // Connection likely dead; drop it so the next attempt
                // reconnects instead of reusing it.
                *bytes_used += c.bytes_written() + c.bytes_read();
                *client = None;
                last_err = Some(e);
            }
        }
    }

    Err(last_err.expect("loop always runs at least once and only exits early on Ok"))
}

/// Pipeline-`STAT` every item in `batch` against `server` over `client`
/// (connected lazily on first use) in one round trip, retrying the *whole
/// batch* up to `retries` times on a connection/transport error. Returns
/// one `bool` per item, in the same order as `batch`.
///
/// A batch either succeeds completely or fails completely: once a read
/// fails partway through, the connection is desynced (subsequent bytes on
/// the wire no longer line up with the remaining expected responses), so
/// there's no way to trust any response after that point even if earlier
/// ones in the same batch looked fine. Retrying the whole (small, capped at
/// the configured pipeline depth) batch on a fresh connection is simpler
/// and safer than trying to salvage a partial one — mirrors
/// [`crate::download::fetch_with_retry`]'s per-item retry shape, just at
/// batch granularity.
///
/// Every byte a connection transferred is added to `bytes_used` right
/// before that connection is dropped (on a transport error) — not just
/// once at the very end — so a reconnect mid-retry never loses the bytes
/// the abandoned connection already spent.
async fn stat_batch_with_retry(
    client: &mut Option<DownloadClient>,
    server: &ServerEntry,
    batch: &[WorkItem],
    retries: u32,
    bytes_used: &mut u64,
) -> Result<Vec<bool>> {
    let mut last_err = None;

    for attempt in 0..=retries {
        if attempt > 0 {
            let base = server.retry_delay.max(1) as f64;
            let backoff = base * (2.0_f64.powi(attempt as i32 - 1));
            tokio::time::sleep(Duration::from_secs_f64(backoff.min(60.0))).await;
        }

        if client.is_none() {
            match DownloadClient::connect(server).await {
                Ok(c) => *client = Some(c),
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            }
        }

        let c = client.as_mut().expect("just connected above");
        match stat_batch_once(c, batch).await {
            Ok(results) => return Ok(results),
            Err(e) => {
                *bytes_used += c.bytes_written() + c.bytes_read();
                *client = None;
                last_err = Some(e);
            }
        }
    }

    Err(last_err.expect("loop always runs at least once and only exits early on Ok"))
}

/// One pipelined round trip: enqueue every item's `STAT`, flush once, then
/// read back one response per item in order. Fails atomically — see
/// [`stat_batch_with_retry`]'s doc comment for why a partial batch is never
/// returned.
async fn stat_batch_once(client: &mut DownloadClient, batch: &[WorkItem]) -> Result<Vec<bool>> {
    for item in batch {
        client.enqueue_stat(&item.message_id).await?;
    }
    client.flush_pipeline().await?;

    let mut results = Vec::with_capacity(batch.len());
    for _ in batch {
        results.push(client.read_stat_response().await?);
    }
    Ok(results)
}
