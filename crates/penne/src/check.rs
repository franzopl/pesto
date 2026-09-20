//! Completeness check: verifies every segment a `.nzb` lists is still
//! present on at least one configured server, via [`CheckMethod::Stat`]
//! (`STAT`, the default — cheapest, but only as trustworthy as the
//! server's index), [`CheckMethod::Head`] (`HEAD` — still cheap, but reads
//! from the same article storage `BODY` does, catching an index that's
//! drifted out of sync), or [`CheckMethod::Body`] (a full real fetch,
//! discarded — maximum certainty, real bandwidth cost). None of the three
//! ever decode, write anything to disk, or touch the resume cache.
//!
//! Mirrors [`crate::download::download_queue`]'s shape — per-server
//! priority order, up to `server.connections` workers per server, retry
//! with backoff on a transient error — but is deliberately its own,
//! simpler implementation rather than a generalisation of it: there is no
//! body to decode, no bytes to cache for resume, and the per-item result is
//! a plain "present or not" instead of a decoded article. Forcing both into
//! one function would trade a small amount of duplication for a
//! meaningfully more complicated one. `Stat` additionally pipelines several
//! requests per round trip (see [`DEFAULT_STAT_PIPELINE_DEPTH`]); `Head`/`Body`
//! deliberately don't — pipelining trades complexity for hiding
//! round-trip latency, which matters enormously for `STAT`'s
//! near-zero-payload round trips but far less once real payload (however
//! small) is involved.
//!
//! A full-release check is far cheaper over the wire than actually
//! downloading it (except, honestly, in `Body` mode) — the point of this
//! module is to answer "is this NZB still fully grabbable" before
//! committing to a real download. [`CheckOutcome::bytes_used`] makes that
//! cost visible instead of just asserted: every byte a check actually sent
//! or received is tracked on [`pesto::nntp::Connection`] itself
//! (`bytes_written`/`bytes_read`) and summed up here, so the terminal
//! report can show, say, "12.3 KiB to check a 4 GiB release" for `Stat`, or
//! the release's real size for `Body`.

use std::collections::HashMap;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;

use crate::config::ServerTier;
use crate::queue::DownloadQueue;

mod execution;
mod model;
mod plan;
mod reporting;

use execution::drain_one_tier;
pub use model::{
    channel, CheckConfig, CheckMethod, CheckOutcome, CheckProgress, CheckProgressReceiver,
    CheckProgressSender, FileCheck, MissingSegment, UnreachableSegment,
    DEFAULT_STAT_PIPELINE_DEPTH,
};
use plan::ItemResolution;

/// Check every segment in `queue` against `tiers`, tried in priority order;
/// within each tier, every member server's own `connections` workers run
/// concurrently, pooled together — see [`crate::config::ServerTier`]. A
/// segment confirmed missing (`430`) from every configured server lands in
/// [`CheckOutcome::missing`]; everything else counts as present.
///
/// `progress`, when given, gets one [`CheckProgress`] event per segment as
/// its fate is *finally* decided — a "present" event fires as soon as any
/// server confirms it; a "missing" event only fires once every configured
/// tier has been tried and none had it, mirroring
/// [`crate::download::download_queue`]'s own emit points exactly (never
/// emit "missing" for a segment a backup tier might still have).
pub async fn check_queue(
    queue: &DownloadQueue,
    tiers: &[ServerTier],
    config: &CheckConfig,
    progress: Option<CheckProgressSender>,
) -> Result<CheckOutcome> {
    let mut outcomes =
        check_nzbs(std::slice::from_ref(queue), tiers, config, progress, None).await?;
    Ok(outcomes.pop().unwrap())
}

/// Check multiple NZB queues in a single run, returning one
/// [`CheckOutcome`] per queue. This uses a single set of connections to
/// process all queues as one massive batch, avoiding the overhead of
/// tearing down and re-establishing TCP/TLS connections between NZBs.
pub async fn check_nzbs(
    queues: &[DownloadQueue],
    tiers: &[ServerTier],
    config: &CheckConfig,
    progress: Option<CheckProgressSender>,
    outcome_tx: Option<tokio::sync::mpsc::UnboundedSender<(usize, CheckOutcome)>>,
) -> Result<Vec<CheckOutcome>> {
    anyhow::ensure!(!tiers.is_empty(), "no servers configured");
    let start = Instant::now();

    let plan::CheckPlan {
        totals,
        total_items_per_q,
        mut pending,
    } = plan::build(queues);
    let stop = Arc::new(AtomicBool::new(false));

    let (item_tx, item_rx) = tokio::sync::mpsc::unbounded_channel::<ItemResolution>();
    let item_tx_opt = if outcome_tx.is_some() {
        Some(item_tx)
    } else {
        None
    };

    let tracker_task = reporting::spawn_tracker(
        outcome_tx,
        queues,
        &totals,
        &total_items_per_q,
        start,
        stop.clone(),
        item_rx,
    );

    let mut present: Vec<HashMap<String, u32>> = vec![HashMap::new(); queues.len()];
    let mut bytes_used = 0u64;

    let last_tier_idx = tiers.len() - 1;
    for (idx, tier) in tiers.iter().enumerate() {
        if pending.is_empty() {
            break;
        }
        let is_last_tier = idx == last_tier_idx;
        let (found, leftover, bytes) = drain_one_tier(
            tier,
            pending,
            config,
            &progress,
            is_last_tier,
            stop.clone(),
            item_tx_opt.clone(),
        )
        .await;
        bytes_used += bytes;
        for item in found {
            *present[item.queue_idx].entry(item.file_name).or_insert(0) += 1;
        }
        pending = leftover;
    }

    drop(item_tx_opt);
    if let Some(task) = tracker_task {
        task.await.ok();
    }
    Ok(reporting::build_outcomes(reporting::OutcomeInputs {
        queues,
        totals: &totals,
        total_items_per_q: &total_items_per_q,
        present,
        pending,
        bytes_used,
        start,
        stopped_early: stop.load(std::sync::atomic::Ordering::Relaxed),
    }))
}
