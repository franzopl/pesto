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

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::ValueEnum;
use pesto::config::ServerEntry;
use tokio::task::JoinSet;

use crate::client::DownloadClient;
use crate::config::ServerTier;
use crate::queue::DownloadQueue;

/// Which NNTP command an availability check uses to decide "present or
/// not" for each segment — a three-way trade between wire cost and how
/// trustworthy the answer is.
///
/// `Stat` and `Head` never transfer an article body; `Body` does (and
/// discards it, never decoding or writing anything — see
/// [`check_queue`]'s doc comment). Lives here rather than in the CLI
/// binary, mirroring [`crate::config::ProcessingMode`]'s existing
/// precedent of a `clap::ValueEnum` in the library so both the flag and a
/// future library caller share one definition.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[value(rename_all = "lowercase")]
pub enum CheckMethod {
    /// `STAT` (RFC 3977 §6.2.4): a bare existence check against the
    /// server's index. Cheapest by far, but the index can drift out of
    /// sync with what the server can actually deliver — see `Head`.
    #[default]
    Stat,
    /// `HEAD` (RFC 3977 §6.2.2): fetches just the header block. Still
    /// cheap (a few hundred bytes, not the full article), and on most
    /// servers reads from the same underlying article storage `BODY`
    /// does, catching a provider whose `STAT` index says "present" for an
    /// article its real storage doesn't have. Not guaranteed, though: some
    /// providers apparently serve `HEAD` from a more complete path than
    /// `BODY` — `Body` is the only method that's ever fully certain.
    Head,
    /// `BODY` (RFC 3977 §6.2.3): a full, real article fetch, discarded
    /// immediately. Maximum certainty, real bandwidth cost — the same as
    /// an actual download would pay for the same segment.
    Body,
}

impl fmt::Display for CheckMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CheckMethod::Stat => "STAT",
            CheckMethod::Head => "HEAD",
            CheckMethod::Body => "BODY",
        })
    }
}

/// Default pipeline depth for `STAT` commands per connection. Each `STAT`
/// is ~80 bytes on the wire, so 128 in-flight STATs buffer ~10 KB — trivial.
/// This hides significantly more RTT latency than nzbCheck's depth of 64,
/// roughly halving check wall-time on high-latency links.
pub const DEFAULT_STAT_PIPELINE_DEPTH: usize = 128;

/// Configuration for a `STAT`/`HEAD`/`BODY` availability check run.
///
/// Designed for both CLI and library callers (`sugo`, `upapasta`, or any
/// code that embeds `penne` as a library and wants to check article
/// availability programmatically).
#[derive(Debug, Clone)]
pub struct CheckConfig {
    /// Which NNTP command to use for the existence check.
    pub method: CheckMethod,
    /// How many `STAT` commands to pipeline per connection per round trip.
    /// Only used when `method` is [`CheckMethod::Stat`]; ignored for
    /// `Head`/`Body`. Clamped to `1..=256` internally.
    pub pipeline_depth: usize,
    /// Retry attempts per server before failing over to the next tier.
    pub retries: u32,
}

impl Default for CheckConfig {
    fn default() -> Self {
        Self {
            method: CheckMethod::default(),
            pipeline_depth: DEFAULT_STAT_PIPELINE_DEPTH,
            retries: 3,
        }
    }
}

impl CheckConfig {
    /// Build a [`CheckConfig`] from individual values, matching the old
    /// `check_queue(queue, tiers, method, retries, progress)` call pattern.
    /// Provided for ergonomic migration of callers that used the previous
    /// four-argument form.
    pub fn new(method: CheckMethod, retries: u32) -> Self {
        Self {
            method,
            retries,
            ..Default::default()
        }
    }
}

/// One segment's `STAT` just resolved, for a live progress bar —
/// deliberately its own small type rather than reusing
/// [`crate::progress::ProgressEvent`]: that enum's variants
/// (`SegmentDownloaded`, `FileAssembled`, ...) describe fetching and
/// writing bytes, none of which a `STAT`-only check ever does.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CheckProgress {
    pub present: bool,
}

pub type CheckProgressSender = tokio::sync::mpsc::UnboundedSender<CheckProgress>;
pub type CheckProgressReceiver = tokio::sync::mpsc::UnboundedReceiver<CheckProgress>;

/// Create a fresh check-progress channel.
pub fn channel() -> (CheckProgressSender, CheckProgressReceiver) {
    tokio::sync::mpsc::unbounded_channel()
}

/// A segment no configured server confirmed via `STAT`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MissingSegment {
    pub file_name: String,
    pub part: u32,
    pub message_id: String,
}

/// One file's completeness: how many of its segments a server confirmed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileCheck {
    pub name: String,
    pub total_segments: u32,
    pub present_segments: u32,
}

impl FileCheck {
    pub fn is_complete(&self) -> bool {
        self.total_segments > 0 && self.present_segments >= self.total_segments
    }
}

/// Result of checking a [`DownloadQueue`] against a set of servers.
#[derive(Debug)]
pub struct CheckOutcome {
    /// One entry per file, in `.nzb` queue order.
    pub files: Vec<FileCheck>,
    /// Segments no configured server confirmed present.
    pub missing: Vec<MissingSegment>,
    /// Total bytes actually sent/received over the wire to perform this
    /// check (every `STAT <id>` command and its response, across every
    /// connection opened) — the whole point of `STAT` over a real fetch is
    /// that this stays tiny even for a release with thousands of segments.
    pub bytes_used: u64,
    /// Wall-clock duration of the check (from entering `check_queue` to
    /// the last response being read, not including NZB parsing or queue
    /// construction).
    pub elapsed: Duration,
    /// Total number of articles checked (segments in the queue).
    pub total_checked: u32,
    /// Articles confirmed present on at least one configured server.
    pub total_present: u32,
}

impl CheckOutcome {
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }

    /// Articles checked per second, based on wall-clock elapsed time.
    /// Returns `0.0` when elapsed is zero (instantaneous or empty queue).
    pub fn articles_per_second(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            self.total_checked as f64 / secs
        } else {
            0.0
        }
    }

    /// Number of missing segments — convenience alias for
    /// `self.missing.len()`.
    pub fn missing_count(&self) -> usize {
        self.missing.len()
    }
}

/// Provide a sensible default for tests and callers that build a
/// `CheckOutcome` directly (the `elapsed`/`total_*` fields default to
/// zero, which is correct for an "empty queue checked" outcome).
impl Default for CheckOutcome {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            missing: Vec::new(),
            bytes_used: 0,
            elapsed: Duration::ZERO,
            total_checked: 0,
            total_present: 0,
        }
    }
}


#[derive(Debug)]
enum ItemResolution {
    Found(WorkItem),
    Missing(WorkItem),
}

/// One segment still needing to be checked.
#[derive(Debug, Clone)]
struct WorkItem {
    queue_idx: usize,
    file_name: String,
    part: u32,
    message_id: String,
}

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
    let mut outcomes = check_nzbs(&[queue.clone()], tiers, config, progress, None).await?;
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

    let mut totals: Vec<HashMap<String, u32>> = vec![HashMap::new(); queues.len()];
    let mut pending: Vec<WorkItem> = Vec::new();

    for (q_idx, queue) in queues.iter().enumerate() {
        for file in &queue.files {
            totals[q_idx].insert(file.name.clone(), file.segments.len() as u32);
            for seg in &file.segments {
                pending.push(WorkItem {
                    queue_idx: q_idx,
                    file_name: file.name.clone(),
                    part: seg.part,
                    message_id: seg.message_id.clone(),
                });
            }
        }
    }

    let total_items_per_q: Vec<u32> = queues
        .iter()
        .map(|q| q.files.iter().map(|f| f.segments.len() as u32).sum())
        .collect();

    
    let (item_tx, mut item_rx) = tokio::sync::mpsc::unbounded_channel::<ItemResolution>();
    let item_tx_opt = if outcome_tx.is_some() { Some(item_tx) } else { None };

    let tracker_task = if let Some(out_tx) = outcome_tx {
        let qs = queues.to_vec();
        let totals_clone = totals.clone();
        let total_items_per_q_clone = total_items_per_q.clone();
        let start_time = start;
        
        Some(tokio::spawn(async move {
            let mut resolved = vec![0u32; qs.len()];
            let mut present = vec![HashMap::<String, u32>::new(); qs.len()];
            let mut missing = vec![Vec::<MissingSegment>::new(); qs.len()];
            
            while let Some(res) = item_rx.recv().await {
                let q_idx = match &res {
                    ItemResolution::Found(item) => item.queue_idx,
                    ItemResolution::Missing(item) => item.queue_idx,
                };
                
                match res {
                    ItemResolution::Found(item) => {
                        *present[q_idx].entry(item.file_name).or_insert(0) += 1;
                    }
                    ItemResolution::Missing(item) => {
                        missing[q_idx].push(MissingSegment {
                            file_name: item.file_name,
                            part: item.part,
                            message_id: item.message_id,
                        });
                    }
                }
                
                resolved[q_idx] += 1;
                
                if resolved[q_idx] == total_items_per_q_clone[q_idx] {
                    let total_items = total_items_per_q_clone[q_idx];
                    let total_present = total_items - missing[q_idx].len() as u32;
                    
                    let files = qs[q_idx].files.iter().map(|f| {
                        FileCheck {
                            name: f.name.clone(),
                            total_segments: *totals_clone[q_idx].get(&f.name).unwrap_or(&0),
                            present_segments: *present[q_idx].get(&f.name).unwrap_or(&0),
                        }
                    }).collect();
                    
                    let outcome = CheckOutcome {
                        files,
                        missing: std::mem::take(&mut missing[q_idx]),
                        bytes_used: 0,
                        elapsed: start_time.elapsed(),
                        total_checked: total_items,
                        total_present,
                    };
                    
                    let _ = out_tx.send((q_idx, outcome));
                }
            }
        }))
    } else {
        None
    };

    let mut present: Vec<HashMap<String, u32>> = vec![HashMap::new(); queues.len()];
    let mut bytes_used = 0u64;

    let last_tier_idx = tiers.len() - 1;
    for (idx, tier) in tiers.iter().enumerate() {
        if pending.is_empty() {
            break;
        }
        let is_last_tier = idx == last_tier_idx;
        let (found, leftover, bytes) =
            drain_one_tier(tier, pending, config, &progress, is_last_tier, item_tx_opt.clone()).await;
        bytes_used += bytes;
        for item in found {
            *present[item.queue_idx].entry(item.file_name).or_insert(0) += 1;
        }
        pending = leftover;
    }

    let mut missing_per_q: Vec<Vec<MissingSegment>> = vec![Vec::new(); queues.len()];
    for item in pending {
        missing_per_q[item.queue_idx].push(MissingSegment {
            file_name: item.file_name,
            part: item.part,
            message_id: item.message_id,
        });
    }

    drop(item_tx_opt);
    if let Some(task) = tracker_task { task.await.ok(); }
    let mut outcomes = Vec::with_capacity(queues.len());
    let bytes_per_q = if queues.is_empty() {
        0
    } else {
        bytes_used / queues.len() as u64
    };

    for (q_idx, queue) in queues.iter().enumerate() {
        let total_items = total_items_per_q[q_idx];
        let total_present = total_items - missing_per_q[q_idx].len() as u32;

        let files = queue
            .files
            .iter()
            .map(|f| FileCheck {
                name: f.name.clone(),
                total_segments: *totals[q_idx].get(&f.name).unwrap_or(&0),
                present_segments: *present[q_idx].get(&f.name).unwrap_or(&0),
            })
            .collect();

        outcomes.push(CheckOutcome {
            files,
            missing: std::mem::take(&mut missing_per_q[q_idx]),
            bytes_used: bytes_per_q,
            elapsed: start.elapsed(),
            total_checked: total_items,
            total_present,
        });
    }

    Ok(outcomes)
}

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
async fn drain_one_tier(
    tier: &ServerTier,
    pending: Vec<WorkItem>,
    config: &CheckConfig,
    progress: &Option<CheckProgressSender>,
    is_last_tier: bool,
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
    // early (e.g., due to fatal connection errors). They belong in leftover.
    let mut q = queue.lock().unwrap();
    while let Some(item) = q.pop_front() {
        if is_last_tier {
            emit(progress, false);
            if let Some(tx) = &item_tx { let _ = tx.send(ItemResolution::Missing(item.clone())); }
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
                breaker,
                item_tx,
            ).await
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
    worker_count: usize,
    breaker: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    item_tx: Option<tokio::sync::mpsc::UnboundedSender<ItemResolution>>,
) -> (Vec<WorkItem>, Vec<WorkItem>, u64) {
    let mut client: Option<DownloadClient> = None;
    let mut found = Vec::new();
    let mut leftover = Vec::new();
    let mut bytes_used = 0u64;

    loop {
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
                for (item, present) in batch.into_iter().zip(results) {
                    if present {
                        emit(&progress, true);
                        if let Some(tx) = &item_tx { let _ = tx.send(ItemResolution::Found(item.clone())); }
                        found.push(item);
                    } else {
                        if is_last_server {
                            emit(&progress, false);
                            if let Some(tx) = &item_tx { let _ = tx.send(ItemResolution::Missing(item.clone())); }
                        }
                        leftover.push(item);
                    }
                }
            }
            Err(_) => {
                breaker.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if is_last_server {
                    for item in &batch {
                        emit(&progress, false);
                        if let Some(tx) = &item_tx { let _ = tx.send(ItemResolution::Missing(item.clone())); }
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
async fn single_item_worker_loop(
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    server: ServerEntry,
    method: CheckMethod,
    retries: u32,
    progress: Option<CheckProgressSender>,
    is_last_server: bool,
    breaker: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    item_tx: Option<tokio::sync::mpsc::UnboundedSender<ItemResolution>>,
) -> (Vec<WorkItem>, Vec<WorkItem>, u64) {
    let mut client: Option<DownloadClient> = None;
    let mut found = Vec::new();
    let mut leftover = Vec::new();
    let mut bytes_used = 0u64;

    loop {
        if breaker.load(std::sync::atomic::Ordering::Relaxed) >= 5 {
            break;
        }
        let item = {
            let mut q = queue.lock().expect("queue mutex poisoned");
            q.pop_front()
        };
        let Some(item) = item else { break };

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
                if let Some(tx) = &item_tx { let _ = tx.send(ItemResolution::Found(item.clone())); }
                found.push(item);
            }
            Ok(false) => {
                breaker.store(0, std::sync::atomic::Ordering::Relaxed);
                if is_last_server {
                    emit(&progress, false);
                    if let Some(tx) = &item_tx { let _ = tx.send(ItemResolution::Missing(item.clone())); }
                }
                leftover.push(item);
            }
            Err(_) => {
                breaker.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if is_last_server {
                    emit(&progress, false);
                    if let Some(tx) = &item_tx { let _ = tx.send(ItemResolution::Missing(item.clone())); }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_config_default_has_expected_values() {
        let config = CheckConfig::default();
        assert_eq!(config.method, CheckMethod::Stat);
        assert_eq!(config.pipeline_depth, DEFAULT_STAT_PIPELINE_DEPTH);
        assert_eq!(config.pipeline_depth, 128);
        assert_eq!(config.retries, 3);
    }

    #[test]
    fn check_config_new_uses_default_pipeline_depth() {
        let config = CheckConfig::new(CheckMethod::Head, 5);
        assert_eq!(config.method, CheckMethod::Head);
        assert_eq!(config.retries, 5);
        assert_eq!(config.pipeline_depth, DEFAULT_STAT_PIPELINE_DEPTH);
    }

    #[test]
    fn check_outcome_articles_per_second() {
        let outcome = CheckOutcome {
            elapsed: Duration::from_secs(2),
            total_checked: 1000,
            total_present: 990,
            ..Default::default()
        };
        assert!((outcome.articles_per_second() - 500.0).abs() < 0.01);
    }

    #[test]
    fn check_outcome_articles_per_second_zero_elapsed() {
        let outcome = CheckOutcome {
            elapsed: Duration::ZERO,
            total_checked: 100,
            ..Default::default()
        };
        assert_eq!(outcome.articles_per_second(), 0.0);
    }

    #[test]
    fn check_outcome_is_complete_when_no_missing() {
        let outcome = CheckOutcome::default();
        assert!(outcome.is_complete());
    }

    #[test]
    fn check_outcome_not_complete_when_missing() {
        let outcome = CheckOutcome {
            missing: vec![MissingSegment {
                file_name: "test".to_string(),
                part: 1,
                message_id: "id@x".to_string(),
            }],
            ..Default::default()
        };
        assert!(!outcome.is_complete());
        assert_eq!(outcome.missing_count(), 1);
    }

    #[test]
    fn file_check_is_complete_logic() {
        let complete = FileCheck {
            name: "a".to_string(),
            total_segments: 5,
            present_segments: 5,
        };
        assert!(complete.is_complete());

        let incomplete = FileCheck {
            name: "b".to_string(),
            total_segments: 5,
            present_segments: 3,
        };
        assert!(!incomplete.is_complete());

        let empty = FileCheck {
            name: "c".to_string(),
            total_segments: 0,
            present_segments: 0,
        };
        assert!(!empty.is_complete());
    }
}
