//! Draining a [`DownloadQueue`] against configured servers.
//!
//! Fetches each queued segment's article body and decodes it with
//! `pesto::yenc::decode_part` (Phase 3), trying servers in priority order
//! per segment — so a primary provider missing, or serving a truncated copy
//! of, a handful of articles doesn't fail a file a backup server has intact.
//! File assembly (Phase 4) consumes the [`pesto::yenc::DecodedPart`]s this
//! returns; this module does no disk I/O of its own, beyond consulting
//! [`crate::cache`] for resume (Phase 8).
//!
//! **Concurrency (`ROADMAP.md` Phase 2's long-standing open item, closed in
//! Phase 9):** each server is drained by up to `server.connections` workers
//! running at once — real throughput, not one segment at a time. Servers
//! are still tried strictly in priority order (all of server 1's workers
//! finish their pass before server 2's start), since "missing from this
//! server" is an expected, per-segment condition for a downloader, not a
//! failure to rotate away from the way `pesto::nntp::pool` does for
//! posting — a backup provider only gets asked about the segments the
//! primary didn't have.
//!
//! Two resilience mechanisms live here (`ROADMAP.md` Phase 8):
//! - **Cache-first fetch:** before any network request, [`crate::cache`] is
//!   checked for a body already fetched in a previous, interrupted run of
//!   this same download. A cache hit skips the network entirely.
//! - **Retry with backoff:** a connection or fetch error against one server
//!   is retried up to `retries` times (each server's own `retry_delay`
//!   governs the pause) before moving on to the next configured server — a
//!   transient hiccup shouldn't immediately write off a server that
//!   otherwise has the article.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use pesto::config::ServerEntry;
use pesto::yenc::{decode_part, DecodedPart};
use tokio::task::JoinSet;

use crate::cache;
use crate::client::DownloadClient;
use crate::progress::{ProgressEvent, ProgressSender};
use crate::queue::DownloadQueue;

/// A segment that no configured server had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSegment {
    pub file_name: String,
    pub part: u32,
    pub message_id: String,
}

/// A segment that was fetched but could not be decoded as yEnc by any server
/// that had it (a truncated or otherwise corrupted transfer). Distinct from
/// [`MissingSegment`]: the article exists somewhere, but no copy retrieved
/// was structurally valid yEnc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptSegment {
    pub file_name: String,
    pub part: u32,
    pub message_id: String,
    /// The last decode error seen, from whichever server's copy was tried
    /// last.
    pub error: String,
}

/// Result of draining a [`DownloadQueue`] against a set of servers.
#[derive(Debug, Default)]
pub struct DownloadOutcome {
    /// Successfully fetched and decoded segments, keyed by Message-ID. Check
    /// [`DecodedPart::crc_matches`] before trusting the content — a segment
    /// can decode structurally fine yet still fail its own checksum.
    pub segments: HashMap<String, DecodedPart>,
    /// Segments no configured server had.
    pub missing: Vec<MissingSegment>,
    /// Segments fetched but not decodable from any server that had them.
    pub corrupt: Vec<CorruptSegment>,
}

/// One segment still needing to be fetched, with just enough owned data
/// (no borrows) to move across worker tasks.
#[derive(Debug, Clone)]
struct WorkItem {
    file_name: String,
    part: u32,
    message_id: String,
}

/// Fetch and decode every segment in `queue` from `servers`, tried in
/// priority order. Within each server's pass, up to `server.connections`
/// workers run concurrently. A decode failure on one server's copy is not
/// fatal: the next configured server is tried before giving up on the
/// segment, since the failure may be specific to that one transfer.
///
/// `dest_dir` is used only to consult/populate the resume cache
/// ([`crate::cache`]) — no other file I/O happens here. `retries` bounds how
/// many times a connection/fetch error against one server is retried (with
/// that server's own `retry_delay` between attempts) before moving to the
/// next server.
pub async fn download_queue(
    queue: &DownloadQueue,
    servers: &[ServerEntry],
    dest_dir: &Path,
    retries: u32,
    progress: Option<ProgressSender>,
) -> Result<DownloadOutcome> {
    anyhow::ensure!(!servers.is_empty(), "no servers configured");

    emit(&progress, || ProgressEvent::Started {
        files: queue
            .files
            .iter()
            .map(|f| crate::progress::FileEntry {
                name: f.name.clone(),
                segments: f.segments.len() as u32,
                bytes: f.segments.iter().map(|s| s.bytes).sum(),
            })
            .collect(),
    });

    let mut outcome = DownloadOutcome::default();

    // Cache hits are resolved up front, sequentially — they're pure disk
    // reads, not worth spinning up a worker pool for — so no network
    // worker ever spends a slot on a segment already resumed from a
    // previous run.
    let mut pending: Vec<WorkItem> = Vec::new();
    for file in &queue.files {
        for seg in &file.segments {
            if let Some(cached) = cache::load(dest_dir, &seg.message_id) {
                if let Ok(decoded) = decode_part(&cached) {
                    emit(&progress, || ProgressEvent::SegmentDownloaded {
                        file_name: file.name.clone(),
                        part: seg.part,
                        bytes: decoded.data.len() as u64,
                    });
                    outcome.segments.insert(seg.message_id.clone(), decoded);
                    continue;
                }
                // A corrupted cache entry (shouldn't happen, but a killed
                // write mid-flush is possible) falls through to a normal
                // network fetch rather than failing the segment outright.
            }
            pending.push(WorkItem {
                file_name: file.name.clone(),
                part: seg.part,
                message_id: seg.message_id.clone(),
            });
        }
    }

    // The most recent decode error per still-outstanding Message-ID, so the
    // final missing-vs-corrupt classification (once every server has been
    // tried) can tell the two apart.
    let mut last_decode_err: HashMap<String, String> = HashMap::new();

    let last_server_idx = servers.len() - 1;
    for (idx, server) in servers.iter().enumerate() {
        if pending.is_empty() {
            break;
        }
        let worker_count = server.connections.max(1);
        let is_last_server = idx == last_server_idx;
        let (fetched, leftover) = drain_one_server(
            server,
            pending,
            dest_dir,
            retries,
            worker_count,
            &progress,
            is_last_server,
        )
        .await;

        for (item, decoded) in fetched {
            last_decode_err.remove(&item.message_id);
            outcome.segments.insert(item.message_id, decoded);
        }

        pending = Vec::with_capacity(leftover.len());
        for (item, decode_err) in leftover {
            if let Some(err) = decode_err {
                last_decode_err.insert(item.message_id.clone(), err);
            }
            pending.push(item);
        }
    }

    // Every item still here failed even the last configured server, so
    // `worker_loop` (told it was draining the last server) has already
    // emitted its `SegmentMissing`/`SegmentCorrupt` progress event for each
    // — this just builds the precise final report from the full
    // cross-server `last_decode_err` history, which is more authoritative
    // than any single pass's own view (see that map's own doc comment).
    for item in pending {
        match last_decode_err.remove(&item.message_id) {
            Some(error) => {
                outcome.corrupt.push(CorruptSegment {
                    file_name: item.file_name,
                    part: item.part,
                    message_id: item.message_id,
                    error,
                });
            }
            None => {
                outcome.missing.push(MissingSegment {
                    file_name: item.file_name,
                    part: item.part,
                    message_id: item.message_id,
                });
            }
        }
    }

    Ok(outcome)
}

fn emit(progress: &Option<ProgressSender>, event: impl FnOnce() -> ProgressEvent) {
    if let Some(tx) = progress {
        let _ = tx.send(event());
    }
}

/// Drain `pending` against `server` using `worker_count` concurrent
/// connections. Returns `(fetched, leftover)`: `fetched` pairs each item
/// with its decoded body; `leftover` is everything this server didn't
/// resolve (missing, or fetched-but-undecodable — paired with the decode
/// error when that's why), for the next server in priority order to try.
///
/// `progress` is threaded down into each worker so a `SegmentDownloaded`
/// event fires the instant *that item* is fetched and decoded — not
/// batched up and only emitted once this whole function returns. All of
/// `worker_count` workers pull from one shared queue and only stop once
/// it's empty, so without per-item emission every worker's task finishes
/// within the same instant at the very end of the pass regardless of queue
/// size, making the progress panel sit still for the whole fetch and then
/// jump straight to 100% (found and fixed via the identical bug in
/// `penne::check::drain_one_server` — see that module's history).
///
/// `is_last_server` mirrors `penne::check`'s fix too: a "missing"/"corrupt"
/// verdict is normally only final once every configured server has had a
/// turn, but for the *last* one that's true the instant each item
/// resolves, so it emits per-item there too instead of leaving every
/// leftover item to pile up for one batch after the whole multi-server
/// check returns — the difference between a mostly-missing release
/// updating the panel throughout the download or not moving until the end.
async fn drain_one_server(
    server: &ServerEntry,
    pending: Vec<WorkItem>,
    dest_dir: &Path,
    retries: u32,
    worker_count: usize,
    progress: &Option<ProgressSender>,
    is_last_server: bool,
) -> (
    Vec<(WorkItem, DecodedPart)>,
    Vec<(WorkItem, Option<String>)>,
) {
    let queue = Arc::new(Mutex::new(VecDeque::from(pending)));

    let mut workers = JoinSet::new();
    for _ in 0..worker_count {
        workers.spawn(worker_loop(
            queue.clone(),
            server.clone(),
            dest_dir.to_path_buf(),
            retries,
            progress.clone(),
            is_last_server,
        ));
    }

    let mut fetched = Vec::new();
    let mut leftover = Vec::new();
    while let Some(result) = workers.join_next().await {
        // A worker task can only fail by panicking, which would be a bug in
        // `worker_loop` itself, not a runtime condition to recover from;
        // any items that worker hadn't gotten to yet are simply still
        // sitting in `queue` and will be picked up by whichever worker
        // empties it next (or, if it panicked mid-item, that one item is
        // lost from this pass — acceptably rare against "never panics").
        if let Ok((f, l)) = result {
            fetched.extend(f);
            leftover.extend(l);
        }
    }
    (fetched, leftover)
}

/// One worker's whole pass over `queue`: pop an item, fetch+decode it
/// against `server` (retrying per [`fetch_with_retry`]), repeat until the
/// queue is empty. Keeps one connection open for the entire pass rather
/// than reconnecting per item.
///
/// Emits `SegmentDownloaded` as soon as each item is fetched and decoded.
/// A `SegmentMissing`/`SegmentCorrupt` event fires per-item too, but only
/// when `is_last_server` — classified from *this pass's own* outcome
/// (missing if the server never had it, corrupt if it had it but decoding
/// failed), a reasonable live approximation; the authoritative
/// classification `download_queue` ultimately reports still comes from its
/// own full cross-server `last_decode_err` history, unaffected by this.
async fn worker_loop(
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    server: ServerEntry,
    dest_dir: PathBuf,
    retries: u32,
    progress: Option<ProgressSender>,
    is_last_server: bool,
) -> (
    Vec<(WorkItem, DecodedPart)>,
    Vec<(WorkItem, Option<String>)>,
) {
    let mut client: Option<DownloadClient> = None;
    let mut fetched = Vec::new();
    let mut leftover = Vec::new();

    loop {
        let item = {
            let mut q = queue.lock().expect("queue mutex poisoned");
            q.pop_front()
        };
        let Some(item) = item else { break };

        let body = match fetch_with_retry(&mut client, &server, &item.message_id, retries).await {
            Ok(Some(body)) => body,
            Ok(None) => {
                if is_last_server {
                    emit(&progress, || ProgressEvent::SegmentMissing {
                        file_name: item.file_name.clone(),
                        part: item.part,
                    });
                }
                leftover.push((item, None));
                continue;
            }
            Err(_) => {
                // Exhausted retries against this server; the next server in
                // priority order gets a turn.
                if is_last_server {
                    emit(&progress, || ProgressEvent::SegmentMissing {
                        file_name: item.file_name.clone(),
                        part: item.part,
                    });
                }
                leftover.push((item, None));
                continue;
            }
        };

        match decode_part(&body) {
            Ok(decoded) => {
                // Cache the raw body, not the decoded form — see the module
                // docs on `crate::cache` for why.
                let _ = cache::store(&dest_dir, &item.message_id, &body);
                emit(&progress, || ProgressEvent::SegmentDownloaded {
                    file_name: item.file_name.clone(),
                    part: item.part,
                    bytes: decoded.data.len() as u64,
                });
                fetched.push((item, decoded));
            }
            Err(e) => {
                if is_last_server {
                    emit(&progress, || ProgressEvent::SegmentCorrupt {
                        file_name: item.file_name.clone(),
                        part: item.part,
                        error: e.to_string(),
                    });
                }
                leftover.push((item, Some(e.to_string())));
            }
        }
    }

    if let Some(c) = client {
        c.quit().await;
    }

    (fetched, leftover)
}

/// Fetch `message_id` from `server` over `client` (this worker's own
/// persistent connection, connected lazily on first use), retrying a
/// connection or transport error up to `retries` times (sleeping
/// `server.retry_delay` seconds between attempts), reconnecting each time
/// since an error likely means the connection is now dead.
///
/// `Ok(None)` (the server explicitly doesn't have the article, `430`) is
/// never retried — that is a definitive answer, not a transient failure.
async fn fetch_with_retry(
    client: &mut Option<DownloadClient>,
    server: &ServerEntry,
    message_id: &str,
    retries: u32,
) -> Result<Option<Vec<u8>>> {
    let mut last_err = None;

    for attempt in 0..=retries {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(server.retry_delay)).await;
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
        match c.body(message_id).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                // Connection likely dead; drop it so the next attempt
                // reconnects instead of reusing it.
                *client = None;
                last_err = Some(e);
            }
        }
    }

    Err(last_err.expect("loop always runs at least once and only exits early on Ok"))
}
