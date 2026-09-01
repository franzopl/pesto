//! Streaming STAT check.
//!
//! Every article that gets a clean `240` is queued for a STAT check a few
//! seconds later (`config.check_delay_secs`) — while the upload keeps
//! posting on its own connections, using a small dedicated pool instead.
//! A miss triggers a repost under a fresh Message-ID (the same rationale as
//! the old `repost_missing_segments`: a server that already cursed the sent
//! ID in its dedup history must not be retried under that same ID) and the
//! fresh copy is re-queued for another check. This mirrors `nyuu`'s default
//! check queue (`check.delay`, `check.tries`, `check.postRetries` in its
//! `config.js`) instead of pesto's old model of a single STAT sweep run only
//! after the whole upload finished.
//!
//! A miss doesn't always wait through the full patient retry sequence
//! first, though — see `should_fast_repost`. Once a run has enough
//! first-time checks to trust its miss rate, an isolated miss (rare
//! against an otherwise clean run) skips straight to a repost instead of
//! waiting out `STAT_RETRY_DELAY_SECS` × `check_retries` to reach an
//! already-foregone verdict. A high miss rate — which looks like the
//! server falling behind on indexing rather than individual articles being
//! lost — keeps the patient behavior, so a systemic problem doesn't get
//! answered by flooding an already-struggling server with reposts.

use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::article::{default_subject, generate_message_id, obfuscated_name, random_from, Article};
use crate::config::{Config, ObfuscateMode};
use crate::nntp::pool::ConnectionSlot;
use crate::progress::{ProgressEvent, ProgressSender};
use crate::resume::{PersistedWireIdentity, ResumeState, SegmentRecord};
use crate::yenc;

use super::PostedSegment;

/// Delay between STAT retries on the same posted copy. Not user-configurable
/// (minimal scope — nobody has asked for control over this); matches the
/// value the old end-of-run check pass used.
const STAT_RETRY_DELAY_SECS: u64 = 20;

/// Minimum number of first-time STAT checks (original copies, not reposts)
/// this run has made before its miss rate is trusted enough to skip
/// patient retries — avoids reacting to a single early miss with no other
/// data yet, when it's just as likely to be one slow article as the start
/// of a systemic problem.
const MIN_SAMPLE_FOR_FAST_REPOST: usize = 20;

/// Above this fraction of first-time checks missing, an individual miss
/// stops looking like "this one article is genuinely gone" and starts
/// looking like "the server is behind on indexing right now" — in which
/// case immediately reposting every miss would flood an already-struggling
/// server with duplicates instead of giving it time to catch up. Below it,
/// misses are rare enough that the patient multi-retry wait
/// (`STAT_RETRY_DELAY_SECS` × `check_retries`) mostly just delays an
/// already-correct "it's gone" verdict, so `process_item` skips straight to
/// `handle_confirmed_miss` instead.
const MASS_FAILURE_RATE_THRESHOLD: f64 = 0.05;

/// Whether an isolated STAT miss should skip the remaining patient retries
/// and repost immediately, based on how rare misses have been among this
/// run's other first-time checks so far. See `MIN_SAMPLE_FOR_FAST_REPOST`
/// and `MASS_FAILURE_RATE_THRESHOLD`.
fn should_fast_repost(first_checks: usize, first_misses: usize) -> bool {
    first_checks >= MIN_SAMPLE_FOR_FAST_REPOST
        && (first_misses as f64 / first_checks.max(1) as f64) <= MASS_FAILURE_RATE_THRESHOLD
}

struct QueueItem {
    ready_at: Instant,
    seg: PostedSegment,
    /// STAT attempts made on the current posted copy; resets on repost.
    stat_attempts: u32,
    /// Distinct reposts made so far for this article.
    repost_count: u32,
}

// Reversed so `BinaryHeap` (a max-heap) pops the item with the *smallest*
// `ready_at` first.
impl PartialEq for QueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.ready_at == other.ready_at
    }
}
impl Eq for QueueItem {}
impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        other.ready_at.cmp(&self.ready_at)
    }
}

struct Inner {
    /// One queue per configured server, indexed by `PostedSegment::server_idx`.
    /// Partitioning by server (instead of one heap shared by every worker)
    /// means a worker whose items are all destined for the same server
    /// never has to `retarget` its connection — that used to happen almost
    /// every item once two providers' articles interleaved in a single
    /// shared queue, turning every STAT into a fresh reconnect+auth. See
    /// `check_worker` for the work-stealing fallback that still guarantees
    /// every queue gets drained even when a server has no home worker.
    heaps: Vec<Mutex<BinaryHeap<QueueItem>>>,
    /// Items queued or currently being processed by a worker. Reaching zero
    /// after `open` goes false means the coordinator is done.
    in_flight: AtomicUsize,
    /// True while `notify_posted` may still be called.
    open: AtomicBool,
    config: Config,
    groups: Vec<String>,
    results: Arc<Mutex<Vec<PostedSegment>>>,
    still_missing: Mutex<Vec<String>>,
    /// STAT transport/auth/timeout/unexpected-code exhausted retries, or
    /// a cancel drain of the queue. Distinct from `still_missing` (430).
    inconclusive: Mutex<Vec<String>>,
    events: Option<ProgressSender>,
    cancel: Option<Arc<AtomicBool>>,
    servers: Arc<Vec<crate::config::ServerEntry>>,
    checked_count: AtomicUsize,
    reposted_count: AtomicUsize,
    /// Running totals behind `should_fast_repost` — first-time STAT checks
    /// of an original (non-reposted) copy, and how many of those came back
    /// a miss. Deliberately whole-run cumulative rather than a sliding
    /// window (simpler, no extra bookkeeping/races): a late-onset problem
    /// on an otherwise clean run dilutes into the average more slowly than
    /// a sliding window would react, trading some responsiveness for
    /// simplicity.
    first_checks: AtomicUsize,
    first_misses: AtomicUsize,
    resume: Option<Arc<Mutex<ResumeState>>>,
}

impl Inner {
    fn is_cancelled(&self) -> bool {
        self.cancel
            .as_ref()
            .is_some_and(|f| f.load(Ordering::Relaxed))
    }

    fn is_done(&self) -> bool {
        !self.open.load(Ordering::Acquire) && self.in_flight.load(Ordering::Acquire) == 0
    }

    fn emit(&self, event: ProgressEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }

    /// Queue `item` on the partition matching the server it's headed to
    /// (its current `seg.server_idx`), clamped defensively in case the
    /// server list ever ends up shorter than the index recorded on an item.
    fn push_item(&self, item: QueueItem) {
        let idx = item.seg.server_idx.min(self.heaps.len() - 1);
        self.heaps[idx].lock().unwrap().push(item);
    }

    /// Pop a ready item, preferring `home_idx`'s own queue and falling back
    /// to stealing a ready item from another server's queue so a worker
    /// never sits idle while a backlog exists elsewhere — this is what
    /// keeps every server's queue drained even with fewer check workers
    /// than servers (a very common case: the check pool is deliberately
    /// small, often 1-4 connections total across every configured server).
    fn try_pop_ready(&self, home_idx: usize) -> Option<QueueItem> {
        let now = Instant::now();
        if let Some(item) = Self::pop_ready(&self.heaps[home_idx], now) {
            return Some(item);
        }
        let n = self.heaps.len();
        for offset in 1..n {
            let idx = (home_idx + offset) % n;
            if let Some(item) = Self::pop_ready(&self.heaps[idx], now) {
                return Some(item);
            }
        }
        None
    }

    fn pop_ready(heap: &Mutex<BinaryHeap<QueueItem>>, now: Instant) -> Option<QueueItem> {
        let mut heap = heap.lock().unwrap();
        match heap.peek() {
            Some(top) if top.ready_at <= now => heap.pop(),
            _ => None,
        }
    }

    /// Replace `results`' entry for `(file_name, part)` — used after a
    /// repost changes an article's Message-ID. Also overwrites the resume
    /// record so a later `--resume` does not re-inject the cursed id.
    fn splice(&self, seg: &PostedSegment) {
        let mut results = self.results.lock().unwrap();
        if let Some(existing) = results
            .iter_mut()
            .find(|s| s.file_name == seg.file_name && s.part == seg.part)
        {
            *existing = seg.clone();
        }
        drop(results);
        if let Some(resume) = &self.resume {
            resume.lock().unwrap().record_with(
                &seg.file_name,
                seg.part,
                SegmentRecord {
                    message_id: seg.message_id.clone(),
                    bytes: seg.bytes,
                    confirmed: false,
                    check_disabled: false,
                    server_idx: seg.server_idx,
                    wire_identity: Some(PersistedWireIdentity {
                        subject_name: seg.wire_name.to_string(),
                        yenc_name: seg.wire_yenc_name.to_string(),
                        from: seg.from.to_string(),
                        date: seg.date.0.clone(),
                        unix_date: seg.date.1,
                    }),
                },
            );
        }
    }
}

/// Handle for feeding freshly posted segments into the streaming check queue
/// and, once posting is done, draining it for a final list of articles that
/// never got confirmed.
pub struct CheckCoordinatorHandle {
    tx: Option<mpsc::UnboundedSender<PostedSegment>>,
    inner: Arc<Inner>,
    feeder: tokio::task::JoinHandle<()>,
    workers: Vec<tokio::task::JoinHandle<ConnectionSlot>>,
}

impl CheckCoordinatorHandle {
    /// A clonable sender that feeds the queue — handed to upload workers so
    /// every confirmed `240` can be queued without going through this handle
    /// (which is moved into the final drain).
    pub fn sender(&self) -> mpsc::UnboundedSender<PostedSegment> {
        self.tx.clone().expect("sender available before drain")
    }

    /// Move idle post slots that just finished posting onto the check
    /// queue. Already in the connection budget — never opens TCP, never
    /// checkins to the broker. Safe to call when the queue is already
    /// empty: idle workers poll and exit without connecting.
    pub fn scale_up(&mut self, slots: Vec<ConnectionSlot>) {
        let additional = slots.len();
        let base_idx = self.workers.len();
        for (i, slot) in slots.into_iter().enumerate() {
            let inner = Arc::clone(&self.inner);
            self.workers.push(tokio::spawn(async move {
                check_worker(inner, base_idx + i, slot).await
            }));
        }
        if additional > 0 {
            // The panel's `conns` line otherwise keeps showing the pool
            // size announced at `Started`, even once it's grown — right
            // when the upload's connections join in, the moment the check
            // pool is doing its most visible catch-up work.
            self.inner.emit(ProgressEvent::CheckPoolScaledUp {
                check_connections: self.workers.len(),
            });
        }
    }

    /// Close the input (no more segments will be queued) and wait for every
    /// queued/in-flight article to resolve — verified, reposted-and-verified,
    /// given up on as a confirmed 430 miss, or left inconclusive. `failed`
    /// on `CheckDone` is the MissingConfirmed count only. The slots remain
    /// checked out for recovery and are returned to the caller as one set.
    pub async fn finish_and_drain(mut self) -> CheckDrain {
        drop(self.tx.take());
        let _ = self.feeder.await;
        let mut slots = Vec::with_capacity(self.workers.len());
        for w in self.workers {
            if let Ok(slot) = w.await {
                slots.push(slot);
            }
        }
        let still_missing = self.inner.still_missing.lock().unwrap().clone();
        let inconclusive = self.inner.inconclusive.lock().unwrap().clone();
        if !inconclusive.is_empty() {
            self.inner.emit(ProgressEvent::CheckInconclusive {
                count: inconclusive.len() as u64,
                reason: "check path failed",
            });
        }
        self.inner.emit(ProgressEvent::CheckDone {
            failed: still_missing.len() as u64,
            inconclusive: inconclusive.len() as u64,
        });
        CheckDrain {
            still_missing,
            inconclusive,
            slots,
        }
    }
}

/// Result of draining the streaming STAT queue.
#[derive(Default)]
pub struct CheckDrain {
    /// STAT 430 exhausted every retry/repost. MissingConfirmed.
    pub still_missing: Vec<String>,
    /// STAT path failed without a 430 (transport, timeout, 480/502, cancel).
    pub inconclusive: Vec<String>,
    /// Slots owned by the completed workers, still within the run's budget.
    pub slots: Vec<ConnectionSlot>,
}

/// Spawn the streaming check coordinator: a feeder task that queues incoming
/// segments with a per-article delay, and one worker per already-checked-out
/// slot. The caller carves those slots out of the configured total (see
/// `post_files_with_progress_and_cancel`) so the run never exceeds what the
/// user asked for — workers never open their own TCP.
pub fn spawn_check_coordinator(
    config: Config,
    groups: Vec<String>,
    results: Arc<Mutex<Vec<PostedSegment>>>,
    events: Option<ProgressSender>,
    cancel: Option<Arc<AtomicBool>>,
    check_slots: Vec<ConnectionSlot>,
    resume: Option<Arc<Mutex<ResumeState>>>,
) -> CheckCoordinatorHandle {
    let servers: Arc<Vec<_>> = Arc::new(config.all_servers().collect());
    let n_workers = check_slots.len();
    let n_heaps = servers.len().max(1);

    let inner = Arc::new(Inner {
        heaps: (0..n_heaps)
            .map(|_| Mutex::new(BinaryHeap::new()))
            .collect(),
        in_flight: AtomicUsize::new(0),
        open: AtomicBool::new(true),
        config,
        groups,
        results,
        still_missing: Mutex::new(Vec::new()),
        inconclusive: Mutex::new(Vec::new()),
        events,
        cancel,
        servers,
        checked_count: AtomicUsize::new(0),
        reposted_count: AtomicUsize::new(0),
        first_checks: AtomicUsize::new(0),
        first_misses: AtomicUsize::new(0),
        resume,
    });

    let (tx, mut rx) = mpsc::unbounded_channel::<PostedSegment>();

    let feeder_inner = Arc::clone(&inner);
    let feeder = tokio::spawn(async move {
        let delay = Duration::from_secs(feeder_inner.config.check_delay_secs);
        while let Some(seg) = rx.recv().await {
            feeder_inner.in_flight.fetch_add(1, Ordering::AcqRel);
            feeder_inner.push_item(QueueItem {
                ready_at: Instant::now() + delay,
                seg,
                stat_attempts: 0,
                repost_count: 0,
            });
        }
        feeder_inner.open.store(false, Ordering::Release);
    });

    let mut workers = Vec::with_capacity(n_workers);
    for (worker_idx, slot) in check_slots.into_iter().enumerate() {
        let inner = Arc::clone(&inner);
        workers.push(tokio::spawn(async move {
            check_worker(inner, worker_idx, slot).await
        }));
    }

    CheckCoordinatorHandle {
        tx: Some(tx),
        inner,
        feeder,
        workers,
    }
}

async fn check_worker(
    inner: Arc<Inner>,
    worker_idx: usize,
    mut slot: ConnectionSlot,
) -> ConnectionSlot {
    // Prefer the queue matching the slot's current server so a worker that
    // was assigned (or just posted on) one host doesn't retarget on every
    // item. Stealing from another server's queue (`Inner::try_pop_ready`)
    // still happens once the home queue is empty; that can concentrate
    // every live socket on one `[[servers]]` host even though the process
    // total stays in budget — a documented limitation, not a new knob.
    let home_idx = if inner.servers.is_empty() {
        0
    } else {
        slot.server_idx().min(inner.servers.len() - 1)
    };

    loop {
        if inner.is_cancelled() {
            // Drain without further network calls so `finish_and_drain`
            // doesn't hang. Cancel is Inconclusive, not MissingConfirmed:
            // the check simply did not finish, so `--resume --check` re-STATs.
            for heap in &inner.heaps {
                let mut heap = heap.lock().unwrap();
                while let Some(item) = heap.pop() {
                    inner
                        .inconclusive
                        .lock()
                        .unwrap()
                        .push(item.seg.message_id.clone());
                    inner.in_flight.fetch_sub(1, Ordering::AcqRel);
                }
            }
            if inner.is_done() {
                break;
            }
        }

        let item = inner.try_pop_ready(home_idx);

        let Some(item) = item else {
            if inner.is_done() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(250)).await;
            continue;
        };

        // Busy for the duration of `process_item`'s actual network activity
        // (STAT, and any repost it triggers) — everywhere else in this loop
        // (draining a cancelled queue, or the 250ms poll above while every
        // item is still inside its retry/repost delay) this connection is
        // genuinely idle, which the old panel had no way to distinguish
        // from "working through a backlog".
        inner.emit(ProgressEvent::CheckConnectionBusy { conn: worker_idx });
        process_item(&inner, &mut slot, worker_idx, item).await;
        inner.emit(ProgressEvent::CheckConnectionIdle { conn: worker_idx });
    }

    slot
}

async fn process_item(
    inner: &Arc<Inner>,
    slot: &mut ConnectionSlot,
    worker_idx: usize,
    mut item: QueueItem,
) {
    let max_stat_attempts = inner.config.check_retries.max(1);

    // Whether this is the very first STAT attempt on an article's original
    // (never-reposted) copy — the signal `should_fast_repost` is trained
    // on. Retries of the same copy and checks of a reposted copy don't
    // count: only the first look at each genuinely new article should move
    // the running miss rate, or a batch of slow retries/reposts would
    // itself skew the rate that decides how to handle them.
    let is_first_attempt = item.repost_count == 0 && item.stat_attempts == 0;

    // Always start from the server this article was actually posted to
    // (see `PostedSegment::server_idx`) rather than whichever server this
    // worker's slot happens to be on — a multi-server failover config can
    // legitimately land different articles on different servers, and only
    // the server that has the article can confirm it. `retarget` is a no-op
    // when already pointed there, so this doesn't churn the connection for
    // the common case of consecutive items on the same server.
    slot.retarget(item.seg.server_idx);

    let stat_result = match slot.ensure_connected().await {
        Ok(conn) => conn.stat(&item.seg.message_id).await,
        Err(e) => Err(e),
    };

    match stat_result {
        Ok(true) => {
            if item.repost_count > 0 {
                inner.emit(ProgressEvent::CheckRetryRecovered);
            }
            if is_first_attempt {
                inner.first_checks.fetch_add(1, Ordering::Relaxed);
            }
            if let Some(resume) = &inner.resume {
                resume
                    .lock()
                    .unwrap()
                    .mark_confirmed(&item.seg.file_name, item.seg.part);
            }
            let checked = inner.checked_count.fetch_add(1, Ordering::Relaxed) + 1;
            inner.emit(ProgressEvent::CheckProgress {
                checked: checked as u64,
                ok: true,
            });
            inner.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
        Ok(false) => {
            // 430 is a definitive miss on this connection, not a dead
            // socket. Invalidating here would open a replacement TCP per
            // miss and break the process-wide connection budget.
            item.stat_attempts += 1;
            if is_first_attempt {
                let checks = inner.first_checks.fetch_add(1, Ordering::Relaxed) + 1;
                let misses = inner.first_misses.fetch_add(1, Ordering::Relaxed) + 1;
                if should_fast_repost(checks, misses) {
                    // Misses have been rare against a large-enough sample of
                    // this run's other checks — an isolated miss is more
                    // likely a genuinely lost article than a server that's
                    // simply behind on indexing, so skip the remaining
                    // patient retries (which would just delay an
                    // already-correct verdict) and repost right away.
                    inner.emit(ProgressEvent::CheckFastRepost {
                        first_checks: checks as u64,
                        first_misses: misses as u64,
                    });
                    info!(
                        first_checks = checks,
                        first_misses = misses,
                        "check: fast-repost of isolated miss"
                    );
                    handle_confirmed_miss(inner, slot, worker_idx, item).await;
                    return;
                }
            }
            if item.stat_attempts < max_stat_attempts {
                inner.emit(ProgressEvent::CheckRetrying {
                    attempt: item.stat_attempts,
                    max_attempts: max_stat_attempts,
                    delay_secs: STAT_RETRY_DELAY_SECS,
                    reason: "article not found",
                });
                inner.push_item(QueueItem {
                    ready_at: Instant::now() + Duration::from_secs(STAT_RETRY_DELAY_SECS),
                    ..item
                });
                return;
            }
            handle_confirmed_miss(inner, slot, worker_idx, item).await;
        }
        Err(e) => {
            warn!(
                segment = %item.seg.message_id,
                slot_id = worker_idx,
                error = %e,
                "check: STAT error; invalidating slot"
            );
            slot.invalidate("stat_err");
            item.stat_attempts += 1;
            if item.stat_attempts < max_stat_attempts {
                let base = super::jittered(slot.retry_delay(), worker_idx);
                // Unlike the "not found" path above, this used to be silent
                // in the UI — only a `tracing::warn!`, which is a no-op
                // unless the user runs with `-v`/`--session-log`. A run of
                // connection failures then looked indistinguishable from a
                // hang instead of a visible backoff.
                inner.emit(ProgressEvent::CheckRetrying {
                    attempt: item.stat_attempts,
                    max_attempts: max_stat_attempts,
                    delay_secs: base.as_secs(),
                    reason: "connection error",
                });
                inner.push_item(QueueItem {
                    ready_at: Instant::now() + base,
                    ..item
                });
                return;
            }
            // Exhausted STAT retries without ever seeing 430 — the article
            // may well be on the server; the check path died. Do not repost
            // and do not classify as MissingConfirmed.
            mark_inconclusive(inner, item.seg.message_id, "connection error");
        }
    }
}

fn mark_inconclusive(inner: &Inner, message_id: String, reason: &'static str) {
    let count = {
        let mut list = inner.inconclusive.lock().unwrap();
        list.push(message_id);
        list.len()
    };
    warn!(
        count,
        reason, "check: article inconclusive (check path failed — not a confirmed gap)"
    );
    inner.emit(ProgressEvent::CheckInconclusive {
        count: count as u64,
        reason,
    });
    inner.in_flight.fetch_sub(1, Ordering::AcqRel);
}

/// An article's current posted copy has exhausted its STAT attempts. Repost
/// it under a fresh Message-ID if repost attempts remain; otherwise it's
/// permanently missing.
async fn handle_confirmed_miss(
    inner: &Arc<Inner>,
    slot: &mut ConnectionSlot,
    worker_idx: usize,
    item: QueueItem,
) {
    let max_post_retries = inner.config.check_post_retries;
    if item.repost_count >= max_post_retries {
        warn!(id = %item.seg.message_id, "check: article still missing after all repost attempts");
        inner
            .still_missing
            .lock()
            .unwrap()
            .push(item.seg.message_id.clone());
        let checked = inner.checked_count.fetch_add(1, Ordering::Relaxed) + 1;
        inner.emit(ProgressEvent::CheckProgress {
            checked: checked as u64,
            ok: false,
        });
        inner.in_flight.fetch_sub(1, Ordering::AcqRel);
        return;
    }

    match repost_one(&inner.config, slot, &item.seg, &inner.groups).await {
        Ok(new_seg) => {
            let reposted = inner.reposted_count.fetch_add(1, Ordering::Relaxed) + 1;
            inner.emit(ProgressEvent::CheckReposted {
                reposted: reposted as u64,
            });
            info!(
                old_id = %item.seg.message_id,
                new_id = %new_seg.message_id,
                attempt = item.repost_count + 1,
                max_attempts = max_post_retries,
                "check: repost accepted; waiting for confirmation"
            );
            inner.emit(ProgressEvent::Status {
                text: format!(
                    "check: reposted {} (attempt {}/{})",
                    new_seg.message_id,
                    item.repost_count + 1,
                    max_post_retries
                ),
            });
            inner.splice(&new_seg);
            let delay = Duration::from_secs(inner.config.check_delay_secs);
            inner.push_item(QueueItem {
                ready_at: Instant::now() + delay,
                seg: new_seg,
                stat_attempts: 0,
                repost_count: item.repost_count + 1,
            });
        }
        Err(e) => {
            warn!(
                id = %item.seg.message_id,
                slot_id = worker_idx,
                error = %e,
                "check: repost failed; giving up on this article"
            );
            if is_post_refusal(&e) {
                inner
                    .still_missing
                    .lock()
                    .unwrap()
                    .push(item.seg.message_id.clone());
                let checked = inner.checked_count.fetch_add(1, Ordering::Relaxed) + 1;
                inner.emit(ProgressEvent::CheckProgress {
                    checked: checked as u64,
                    ok: false,
                });
                inner.in_flight.fetch_sub(1, Ordering::AcqRel);
            } else {
                // Transport/timeout on the POST itself is not a 430 gap.
                mark_inconclusive(inner, item.seg.message_id, "repost connection error");
            }
        }
    }
}

/// Re-read `seg`'s slice from disk, re-encode it, and post it under a fresh
/// Message-ID. Deliberately never reuses `seg.message_id` — see the module
/// doc comment for why reposting under a cursed ID is unsafe.
async fn repost_one(
    config: &Config,
    slot: &mut ConnectionSlot,
    seg: &PostedSegment,
    groups: &[String],
) -> anyhow::Result<PostedSegment> {
    let offset = (seg.part as u64 - 1) * config.article_size as u64;

    let mut file = tokio::fs::File::open(&seg.file_path).await?;
    file.seek(std::io::SeekFrom::Start(offset)).await?;
    let read_len = (seg.file_size - offset).min(config.article_size as u64) as usize;
    let mut buf = Vec::new();
    buf.try_reserve_exact(read_len)
        .map_err(|e| anyhow::anyhow!("allocating repost buffer: {e}"))?;
    buf.resize(read_len, 0);
    file.read_exact(&mut buf).await?;

    let spec = yenc::PartSpec {
        number: seg.part,
        total: seg.total,
        offset,
    };
    let file_crc32 = (seg.part == seg.total).then_some(seg.full_crc32);
    let (wire_subject, wire_yenc, from, date) = if config.obfuscate == ObfuscateMode::Article {
        (
            obfuscated_name(),
            seg.wire_yenc_name.to_string(),
            random_from(),
            super::resolve_date(config.date.as_deref()),
        )
    } else {
        (
            seg.wire_name.to_string(),
            seg.wire_yenc_name.to_string(),
            seg.from.to_string(),
            seg.date.clone(),
        )
    };
    // `seg.subject_name` is always the *real* filename (see `PostedSegment`'s
    // doc comment) — using it here would repost an obfuscated release under
    // its real name, undoing `--obfuscate` the moment one article needs a
    // repost. `wire_name` carries the identity actually posted with.
    let encoded = yenc::encode_part(
        &wire_yenc,
        seg.file_size,
        spec,
        &buf,
        config.line_length,
        file_crc32,
    );
    let (rfc_date, _ts) = &date;
    let mut message_id = generate_message_id(config.message_id_domain.as_deref());
    let article = Article {
        message_id: message_id.clone(),
        from: from.clone(),
        newsgroups: groups.to_vec(),
        subject: default_subject(
            &wire_subject,
            seg.part,
            seg.total,
            (seg.total_files > 0).then_some((seg.file_index, seg.total_files)),
        ),
        date: rfc_date.clone(),
        no_archive: config.no_archive,
    };
    let headers = article.build_headers();
    let wire_bytes = (headers.len() + encoded.body.len()) as u64;

    let max_retries = config.retries.max(1);
    let mut last_err = anyhow::anyhow!("repost: no attempt made");
    for attempt in 1..=max_retries {
        match slot.ensure_connected().await {
            Ok(conn) => match conn.repost_parts_confirmed(&headers, &encoded.body).await {
                Ok(returned_id) => {
                    if let Some(server_id) = returned_id {
                        if server_id != message_id {
                            warn!(
                                sent = %message_id,
                                returned = %server_id,
                                "server returned a different Message-ID than sent; adopting it"
                            );
                            message_id = server_id;
                        }
                    }
                    return Ok(PostedSegment {
                        file_name: seg.file_name.clone(),
                        file_path: seg.file_path.clone(),
                        subject_name: seg.subject_name.clone(),
                        wire_name: Arc::from(wire_subject.as_str()),
                        wire_yenc_name: Arc::from(wire_yenc.as_str()),
                        file_size: seg.file_size,
                        part: seg.part,
                        total: seg.total,
                        message_id,
                        bytes: wire_bytes,
                        from: Arc::from(from.as_str()),
                        date: date.clone(),
                        full_crc32: seg.full_crc32,
                        server_idx: slot.server_idx(),
                        file_index: seg.file_index,
                        total_files: seg.total_files,
                    });
                }
                Err(e) => {
                    slot.invalidate("post_err");
                    last_err = e;
                }
            },
            Err(e) => {
                last_err = e;
            }
        }
        if attempt < max_retries {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    }
    Err(last_err)
}

/// NNTP response code embedded in a POST error, if any.
fn nntp_error_code(err: &anyhow::Error) -> Option<u16> {
    let s = err.to_string();
    for prefix in [
        "article rejected by server (",
        "authentication rejected by server (code ",
        "unexpected POST response: ",
        "unexpected POST response (pipelined): ",
        "POST not permitted: ",
        "POST not permitted (pipelined): ",
    ] {
        if let Some(rest) = s.split_once(prefix).map(|(_, r)| r) {
            let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(code) = digits.parse::<u16>() {
                if (200..600).contains(&code) {
                    return Some(code);
                }
            }
        }
    }
    None
}

/// True when a repost failed because the server refused the article (441 or
/// other 4xx except the AUTH 48x class). Connect/timeout/5xx/AUTH 480–489
/// are Inconclusive.
fn is_post_refusal(err: &anyhow::Error) -> bool {
    let s = err.to_string();
    if s.contains("authentication rejected by server") {
        return false;
    }
    match nntp_error_code(err) {
        Some(480..=489) => false,
        Some(code) if (400..500).contains(&code) => true,
        _ => false,
    }
}

/// Recovered (STAT 223) vs Inconclusive (STAT `Err`, or recovery POST
/// transport/AUTH/5xx) subsets of a recovery pass.
/// Anything in neither is still MissingConfirmed (430, or a 441/4xx recovery
/// POST refusal). Recovery never runs on articles that were already
/// Inconclusive — the caller only passes MissingConfirmed.
pub(crate) struct RecoverOutcome {
    pub recovered: Vec<PostedSegment>,
    pub inconclusive: Vec<PostedSegment>,
    pub slots: Vec<ConnectionSlot>,
}

/// One extra, bounded recovery attempt for articles that are still missing
/// after every `check_post_retries` round already ran out — the caller (see
/// `poster::maybe_recover_missing`) has already decided this batch is small
/// enough to be worth it. Unlike the streaming coordinator's normal flow,
/// every item here is already *known* to be missing (that's how it ended up
/// here), so this skips straight to [`repost_one`] instead of re-running the
/// patient STAT-retry sequence first, then verifies once via a single STAT
/// after `check_delay_secs` — one round trip per article, not the full
/// `check_retries`-attempt cycle.
///
/// Runs the batch across the supplied slots (already in the connection
/// budget — typically those returned by
/// [`CheckCoordinatorHandle::finish_and_drain`]), never more workers than
/// there is work or slots. Unused slots stay held so `--jobs` cannot start
/// the next episode until this pass is done. This used to be a strict
/// one-article-at-a-time loop: with `check_recover_max` defaulting to 50
/// and each article costing a repost round trip plus `check_delay_secs`, a
/// stubborn batch could take minutes with the upload already at 100% and
/// every upload connection sitting idle — the exact "why is this frozen"
/// case the recovery pass exists to resolve *quickly*.
///
/// Returns the subset of `segments` that got reposted *and* confirmed
/// present, plus the same slots (caller checkins the whole set). Anything
/// not in the recovered list is still genuinely missing.
pub(crate) async fn recover_missing(
    config: &Config,
    groups: &[String],
    segments: Vec<PostedSegment>,
    events: Option<&ProgressSender>,
    mut slots: Vec<ConnectionSlot>,
) -> RecoverOutcome {
    if segments.is_empty() || slots.is_empty() {
        return RecoverOutcome {
            recovered: Vec::new(),
            inconclusive: Vec::new(),
            slots,
        };
    }

    let total = segments.len() as u64;
    if let Some(tx) = events {
        let _ = tx.send(ProgressEvent::CheckRecoverStarted { total });
    }

    let n_workers = slots.len().min(segments.len());
    let work_slots: Vec<_> = slots.drain(..n_workers).collect();

    let queue = Arc::new(Mutex::new(segments));
    let done = Arc::new(AtomicUsize::new(0));
    let recovered = Arc::new(Mutex::new(Vec::with_capacity(total as usize)));
    let inconclusive = Arc::new(Mutex::new(Vec::new()));

    let mut workers = Vec::with_capacity(n_workers);
    for mut slot in work_slots {
        let config = config.clone();
        let groups = groups.to_vec();
        let queue = Arc::clone(&queue);
        let done = Arc::clone(&done);
        let recovered = Arc::clone(&recovered);
        let inconclusive = Arc::clone(&inconclusive);
        let events = events.cloned();

        workers.push(tokio::spawn(async move {
            loop {
                let seg = queue.lock().unwrap().pop();
                let Some(seg) = seg else { break };

                slot.retarget(seg.server_idx);

                let ok = match repost_one(&config, &mut slot, &seg, &groups).await {
                    Ok(new_seg) => {
                        tokio::time::sleep(Duration::from_secs(config.check_delay_secs)).await;
                        match slot.ensure_connected().await {
                            Ok(conn) => match conn.stat(&new_seg.message_id).await {
                                Ok(true) => {
                                    recovered.lock().unwrap().push(new_seg);
                                    true
                                }
                                Ok(false) => {
                                    warn!(
                                        id = %new_seg.message_id,
                                        "check: recovery repost accepted but STAT 430 on final check"
                                    );
                                    false
                                }
                                Err(e) => {
                                    warn!(
                                        id = %new_seg.message_id,
                                        error = %e,
                                        "check: recovery STAT error; classifying as inconclusive"
                                    );
                                    inconclusive.lock().unwrap().push(new_seg);
                                    false
                                }
                            },
                            Err(e) => {
                                warn!(
                                    id = %new_seg.message_id,
                                    error = %e,
                                    "check: recovery STAT connect error; classifying as inconclusive"
                                );
                                inconclusive.lock().unwrap().push(new_seg);
                                false
                            }
                        }
                    }
                    Err(e) => {
                        if is_post_refusal(&e) {
                            warn!(
                                id = %seg.message_id,
                                error = %e,
                                "check: final recovery repost refused; still missing"
                            );
                            false
                        } else {
                            warn!(
                                id = %seg.message_id,
                                error = %e,
                                "check: recovery POST path failed; classifying as inconclusive"
                            );
                            inconclusive.lock().unwrap().push(seg);
                            false
                        }
                    }
                };

                // Emits `CheckRecoverProgress` for *every* resolution,
                // success or failure — unlike the old plain `Status` text,
                // which only fired on success. A batch with real,
                // unrecoverable misses used to go completely silent
                // (nothing but a `tracing::warn!`, invisible without `-v`)
                // for however long those repost/STAT round trips took.
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(tx) = &events {
                    let _ = tx.send(ProgressEvent::CheckRecoverProgress {
                        done: n as u64,
                        total,
                        ok,
                    });
                }
            }
            slot
        }));
    }

    for w in workers {
        if let Ok(slot) = w.await {
            slots.push(slot);
        }
    }

    RecoverOutcome {
        recovered: Arc::try_unwrap(recovered)
            .expect("every worker task has finished by now")
            .into_inner()
            .unwrap(),
        inconclusive: Arc::try_unwrap(inconclusive)
            .expect("every worker task has finished by now")
            .into_inner()
            .unwrap(),
        slots,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_repost_withheld_below_the_sample_floor() {
        // Even a 0% miss rate shouldn't be trusted with almost no data —
        // a single miss out of 3 checks is not distinguishable from a
        // systemic problem yet.
        assert!(!should_fast_repost(3, 1));
        assert!(!should_fast_repost(MIN_SAMPLE_FOR_FAST_REPOST - 1, 0));
    }

    #[test]
    fn fast_repost_allowed_once_sample_floor_met_with_a_low_rate() {
        // 1 miss in 20 checks (5%) sits right at the threshold — allowed.
        assert!(should_fast_repost(MIN_SAMPLE_FOR_FAST_REPOST, 1));
        // A single isolated miss in a much larger, otherwise-clean run.
        assert!(should_fast_repost(1000, 5));
    }

    #[test]
    fn fast_repost_withheld_once_the_rate_looks_systemic() {
        // 2 misses in 20 checks (10%) is over the 5% threshold.
        assert!(!should_fast_repost(MIN_SAMPLE_FOR_FAST_REPOST, 2));
        // A third of checks missing is a server having a bad time, not a
        // handful of unlucky articles.
        assert!(!should_fast_repost(300, 100));
    }

    fn err(msg: &str) -> anyhow::Error {
        anyhow::anyhow!("{msg}")
    }

    #[test]
    fn post_refusal_is_441_and_other_4xx_except_auth() {
        assert!(is_post_refusal(&err(
            "article rejected by server (441): 435 Already exists in history"
        )));
        assert!(is_post_refusal(&err(
            "POST not permitted: 440 Posting Not Allowed"
        )));
        assert!(is_post_refusal(&err(
            "unexpected POST response: 441 article rejected"
        )));
        assert!(!is_post_refusal(&err(
            "authentication rejected by server (code 502); check the configured username and password"
        )));
        assert!(!is_post_refusal(&err(
            "authentication rejected by server (code 481); check the configured username and password"
        )));
        assert!(!is_post_refusal(&err(
            "authentication rejected by server (code 482); check the configured username and password"
        )));
        assert!(!is_post_refusal(&err(
            "POST not permitted: 480 Authentication required"
        )));
        assert!(!is_post_refusal(&err(
            "unexpected POST response: 481 Authentication failed"
        )));
        assert!(!is_post_refusal(&err(
            "unexpected POST response: 502 Permission denied"
        )));
        assert!(!is_post_refusal(&err("connection reset by peer")));
        assert!(!is_post_refusal(&err("timed out")));
    }
}
