//! STAT-only completeness check: verifies every segment a `.nzb` lists is
//! still present on at least one configured server, without downloading,
//! decoding, or writing anything to disk.
//!
//! Mirrors [`crate::download::download_queue`]'s shape — per-server
//! priority order, up to `server.connections` workers per server, retry
//! with backoff on a transient error — but is deliberately its own,
//! simpler implementation rather than a generalisation of it: there is no
//! body to decode, no bytes to cache for resume, and the per-item result is
//! a plain "present or not" instead of a decoded article. Forcing both into
//! one function would trade a small amount of duplication for a
//! meaningfully more complicated one.
//!
//! `STAT` (RFC 3977 §6.2.4) is a small existence-check round trip, not an
//! article transfer, so a full-release check is far cheaper over the wire
//! than actually downloading it — the point of this module is to answer
//! "is this NZB still fully grabbable" before committing to a real
//! download. [`CheckOutcome::bytes_used`] makes that cheapness visible
//! instead of just asserted: every byte a check actually sent or received
//! is tracked on [`pesto::nntp::Connection`] itself
//! (`bytes_written`/`bytes_read`) and summed up here, so the terminal
//! report can show, say, "12.3 KiB to check a 4 GiB release".

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Result;
use pesto::config::ServerEntry;
use tokio::task::JoinSet;

use crate::client::DownloadClient;
use crate::queue::DownloadQueue;

/// A segment no configured server confirmed via `STAT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSegment {
    pub file_name: String,
    pub part: u32,
    pub message_id: String,
}

/// One file's completeness: how many of its segments a server confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
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
#[derive(Debug, Default)]
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
}

impl CheckOutcome {
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

/// One segment still needing to be checked.
#[derive(Debug, Clone)]
struct WorkItem {
    file_name: String,
    part: u32,
    message_id: String,
}

/// Check every segment in `queue` against `servers`, tried in priority
/// order, up to `server.connections` workers concurrently per server. A
/// segment confirmed missing (`430`) from every configured server lands in
/// [`CheckOutcome::missing`]; everything else counts as present.
pub async fn check_queue(
    queue: &DownloadQueue,
    servers: &[ServerEntry],
    retries: u32,
) -> Result<CheckOutcome> {
    anyhow::ensure!(!servers.is_empty(), "no servers configured");

    let mut totals: HashMap<String, u32> = HashMap::new();
    let mut pending: Vec<WorkItem> = Vec::new();
    for file in &queue.files {
        totals.insert(file.name.clone(), file.segments.len() as u32);
        for seg in &file.segments {
            pending.push(WorkItem {
                file_name: file.name.clone(),
                part: seg.part,
                message_id: seg.message_id.clone(),
            });
        }
    }

    let mut present: HashMap<String, u32> = HashMap::new();
    let mut bytes_used = 0u64;

    for server in servers {
        if pending.is_empty() {
            break;
        }
        let worker_count = server.connections.max(1);
        let (found, leftover, bytes) =
            drain_one_server(server, pending, retries, worker_count).await;
        bytes_used += bytes;
        for item in found {
            *present.entry(item.file_name).or_insert(0) += 1;
        }
        pending = leftover;
    }

    let missing = pending
        .into_iter()
        .map(|item| MissingSegment {
            file_name: item.file_name,
            part: item.part,
            message_id: item.message_id,
        })
        .collect();

    // Deterministic, `.nzb`-queue-order output regardless of `HashMap`
    // iteration order.
    let files = queue
        .files
        .iter()
        .map(|f| FileCheck {
            name: f.name.clone(),
            total_segments: *totals.get(&f.name).unwrap_or(&0),
            present_segments: *present.get(&f.name).unwrap_or(&0),
        })
        .collect();

    Ok(CheckOutcome {
        files,
        missing,
        bytes_used,
    })
}

/// Drain `pending` against `server` using `worker_count` concurrent
/// connections. Returns `(found, leftover, bytes_used)`: `found` is
/// everything this server confirmed present; `leftover` is everything it
/// didn't (missing, or a STAT attempt that exhausted its retries), for the
/// next server in priority order to try; `bytes_used` is every byte sent or
/// received across every connection this server's workers opened.
async fn drain_one_server(
    server: &ServerEntry,
    pending: Vec<WorkItem>,
    retries: u32,
    worker_count: usize,
) -> (Vec<WorkItem>, Vec<WorkItem>, u64) {
    let queue = Arc::new(Mutex::new(VecDeque::from(pending)));

    let mut workers = JoinSet::new();
    for _ in 0..worker_count {
        workers.spawn(worker_loop(queue.clone(), server.clone(), retries));
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
    (found, leftover, bytes_used)
}

/// One worker's whole pass over `queue`: pop an item, `STAT` it against
/// `server` (retrying per [`stat_with_retry`]), repeat until the queue is
/// empty. Keeps one connection open for the entire pass.
async fn worker_loop(
    queue: Arc<Mutex<VecDeque<WorkItem>>>,
    server: ServerEntry,
    retries: u32,
) -> (Vec<WorkItem>, Vec<WorkItem>, u64) {
    let mut client: Option<DownloadClient> = None;
    let mut found = Vec::new();
    let mut leftover = Vec::new();
    let mut bytes_used = 0u64;

    loop {
        let item = {
            let mut q = queue.lock().expect("queue mutex poisoned");
            q.pop_front()
        };
        let Some(item) = item else { break };

        match stat_with_retry(
            &mut client,
            &server,
            &item.message_id,
            retries,
            &mut bytes_used,
        )
        .await
        {
            Ok(true) => found.push(item),
            Ok(false) | Err(_) => leftover.push(item),
        }
    }

    if let Some(c) = client {
        bytes_used += c.bytes_written() + c.bytes_read();
        c.quit().await;
    }

    (found, leftover, bytes_used)
}

/// `STAT` `message_id` against `server` over `client` (connected lazily on
/// first use), retrying a connection or transport error up to `retries`
/// times. `Ok(false)` (the server explicitly doesn't have the article,
/// `430`) is never retried — that is a definitive answer, not a transient
/// failure. Mirrors [`crate::download::fetch_with_retry`]'s retry shape.
///
/// Every byte a connection transferred is added to `bytes_used` right
/// before that connection is dropped (on a transport error) — not just
/// once at the very end — so a reconnect mid-retry never loses the bytes
/// the abandoned connection already spent.
async fn stat_with_retry(
    client: &mut Option<DownloadClient>,
    server: &ServerEntry,
    message_id: &str,
    retries: u32,
    bytes_used: &mut u64,
) -> Result<bool> {
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
        match c.stat(message_id).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                *bytes_used += c.bytes_written() + c.bytes_read();
                *client = None;
                last_err = Some(e);
            }
        }
    }

    Err(last_err.expect("loop always runs at least once and only exits early on Ok"))
}
