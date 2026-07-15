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

use std::cmp::Ordering as CmpOrdering;
use std::collections::BinaryHeap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc;
use tracing::warn;

use crate::article::{default_subject, generate_message_id, Article};
use crate::config::Config;
use crate::nntp::pool::ConnectionSlot;
use crate::progress::{ProgressEvent, ProgressSender};
use crate::yenc;

use super::PostedSegment;

/// Delay between STAT retries on the same posted copy. Not user-configurable
/// (minimal scope — nobody has asked for control over this); matches the
/// value the old end-of-run check pass used.
const STAT_RETRY_DELAY_SECS: u64 = 20;

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
    events: Option<ProgressSender>,
    cancel: Option<Arc<AtomicBool>>,
    servers: Arc<Vec<crate::config::ServerEntry>>,
    checked_count: AtomicUsize,
    reposted_count: AtomicUsize,
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
    /// repost changes an article's Message-ID.
    fn splice(&self, seg: &PostedSegment) {
        let mut results = self.results.lock().unwrap();
        if let Some(existing) = results
            .iter_mut()
            .find(|s| s.file_name == seg.file_name && s.part == seg.part)
        {
            *existing = seg.clone();
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
    workers: Vec<tokio::task::JoinHandle<()>>,
}

impl CheckCoordinatorHandle {
    /// A clonable sender that feeds the queue — handed to upload workers so
    /// every confirmed `240` can be queued without going through this handle
    /// (which is moved into the final drain).
    pub fn sender(&self) -> mpsc::UnboundedSender<PostedSegment> {
        self.tx.clone().expect("sender available before drain")
    }

    /// Spawn `additional` more check workers on the same queue, reusing
    /// connections that just freed up — e.g. once the upload's own worker
    /// pool has finished and would otherwise sit idle while a small
    /// dedicated check pool drains whatever backlog is left. Safe to call
    /// any time, including when the queue is already empty or draining has
    /// finished: idle workers just poll harmlessly and exit once there's
    /// nothing left, without ever opening a connection (connections are
    /// opened lazily, only when a worker actually has an article to check).
    pub fn scale_up(&mut self, additional: usize) {
        let base_idx = self.workers.len();
        for i in 0..additional {
            let inner = Arc::clone(&self.inner);
            self.workers.push(tokio::spawn(async move {
                check_worker(inner, base_idx + i).await;
            }));
        }
    }

    /// Close the input (no more segments will be queued) and wait for every
    /// queued/in-flight article to resolve — verified, reposted-and-verified,
    /// or given up on. Returns the Message-IDs that could never be confirmed.
    pub async fn finish_and_drain(mut self) -> Vec<String> {
        drop(self.tx.take());
        let _ = self.feeder.await;
        for w in self.workers {
            let _ = w.await;
        }
        let still_missing = self.inner.still_missing.lock().unwrap().clone();
        self.inner.emit(ProgressEvent::CheckDone {
            failed: still_missing.len() as u64,
        });
        still_missing
    }
}

/// Spawn the streaming check coordinator: a feeder task that queues incoming
/// segments with a per-article delay, and `check_connections` worker tasks
/// that drain the queue via dedicated NNTP connections. `check_connections`
/// is the caller's responsibility to size — see
/// `post_files_with_progress_and_cancel`, which carves it out of the
/// configured total connection count so the run never exceeds what the user
/// asked for (e.g. `-n 50` means 50 connections total, split between
/// posting and checking, not 50 + a check pool on top).
pub fn spawn_check_coordinator(
    config: Config,
    groups: Vec<String>,
    results: Arc<Mutex<Vec<PostedSegment>>>,
    events: Option<ProgressSender>,
    cancel: Option<Arc<AtomicBool>>,
    check_connections: usize,
) -> CheckCoordinatorHandle {
    let servers: Arc<Vec<_>> = Arc::new(config.all_servers().collect());
    let n_workers = check_connections;
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
        events,
        cancel,
        servers,
        checked_count: AtomicUsize::new(0),
        reposted_count: AtomicUsize::new(0),
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
    for worker_idx in 0..n_workers {
        let inner = Arc::clone(&inner);
        workers.push(tokio::spawn(async move {
            check_worker(inner, worker_idx).await;
        }));
    }

    CheckCoordinatorHandle {
        tx: Some(tx),
        inner,
        feeder,
        workers,
    }
}

async fn check_worker(inner: Arc<Inner>, worker_idx: usize) {
    // Each worker has a "home" server whose queue it drains preferentially,
    // spread round-robin across the configured servers so every server gets
    // a fair share of home workers regardless of how the check pool size
    // relates to the server count (the pool is often smaller than the
    // server count, since it's deliberately kept small — see
    // `effective_check_connections`). A worker only steals from another
    // server's queue (`Inner::try_pop_ready`) once its own is empty, which
    // keeps the connection on one server for as long as there's real work
    // there instead of retargeting on every item.
    let home_idx = if inner.servers.is_empty() {
        0
    } else {
        worker_idx % inner.servers.len()
    };
    let mut slot = ConnectionSlot::with_id(Arc::clone(&inner.servers), home_idx, worker_idx);

    loop {
        if inner.is_cancelled() {
            // Drain whatever remains without further network calls so
            // `finish_and_drain` doesn't hang waiting on cancelled work.
            for heap in &inner.heaps {
                let mut heap = heap.lock().unwrap();
                while let Some(item) = heap.pop() {
                    inner
                        .still_missing
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

        process_item(&inner, &mut slot, worker_idx, item).await;
    }

    slot.quit().await;
}

async fn process_item(
    inner: &Arc<Inner>,
    slot: &mut ConnectionSlot,
    worker_idx: usize,
    mut item: QueueItem,
) {
    let max_stat_attempts = inner.config.check_retries.max(1);

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
            let checked = inner.checked_count.fetch_add(1, Ordering::Relaxed) + 1;
            inner.emit(ProgressEvent::CheckProgress {
                checked: checked as u64,
                ok: true,
            });
            inner.in_flight.fetch_sub(1, Ordering::AcqRel);
        }
        Ok(false) => {
            slot.invalidate("stat_430");
            item.stat_attempts += 1;
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
            handle_confirmed_miss(inner, slot, worker_idx, item).await;
        }
    }
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
    let mut buf = vec![0u8; read_len];
    file.read_exact(&mut buf).await?;

    let spec = yenc::PartSpec {
        number: seg.part,
        total: seg.total,
        offset,
    };
    let file_crc32 = (seg.part == seg.total).then_some(seg.full_crc32);
    let encoded = yenc::encode_part(
        &seg.subject_name,
        seg.file_size,
        spec,
        &buf,
        config.line_length,
        file_crc32,
    );
    let (rfc_date, _ts) = &seg.date;
    let mut message_id = generate_message_id(config.message_id_domain.as_deref());
    let article = Article {
        message_id: message_id.clone(),
        from: seg.from.clone(),
        newsgroups: groups.to_vec(),
        subject: default_subject(&seg.subject_name, seg.part, seg.total),
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
                        file_size: seg.file_size,
                        part: seg.part,
                        total: seg.total,
                        message_id,
                        bytes: wire_bytes,
                        from: seg.from.clone(),
                        date: seg.date.clone(),
                        full_crc32: seg.full_crc32,
                        server_idx: slot.server_idx(),
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
