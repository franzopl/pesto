//! Posted-article payloads and the per-worker dispatcher that routes them.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;

use crate::yenc;

use super::FileMeta;

/// Fans posted articles out to per-worker channels instead of one channel
/// shared behind a lock — see the `tx_opt` construction site in
/// `post_files_inner` for why. Each worker owns its `Receiver` outright, so
/// dequeuing never contends with any other worker.
pub(super) struct TaskDispatcher<T> {
    senders: Vec<tokio::sync::mpsc::Sender<T>>,
    next: std::sync::atomic::AtomicUsize,
}

impl<T: Send> TaskDispatcher<T> {
    pub(super) fn new(senders: Vec<tokio::sync::mpsc::Sender<T>>) -> Self {
        TaskDispatcher {
            senders,
            next: std::sync::atomic::AtomicUsize::new(0),
        }
    }

    /// Offer the task to a worker that still has channel room, starting at
    /// the next round-robin index. A stalled (slow-server) worker fills its
    /// channel and must not pin further articles — or the producer — while
    /// idle workers sit empty (issue #145). If every channel is full, wait
    /// on the original target so backpressure still applies.
    pub(super) async fn send(&self, task: T) -> Result<(), tokio::sync::mpsc::error::SendError<T>> {
        let n = self.senders.len();
        let start = self.next.fetch_add(1, Ordering::Relaxed);
        let mut task = task;
        for i in 0..n {
            let idx = (start + i) % n;
            match self.senders[idx].try_send(task) {
                Ok(()) => return Ok(()),
                Err(tokio::sync::mpsc::error::TrySendError::Full(t)) => task = t,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(t)) => {
                    return Err(tokio::sync::mpsc::error::SendError(t));
                }
            }
        }
        let idx = start % n;
        self.senders[idx].send(task).await
    }
}

pub(super) struct PostTask {
    pub(super) meta: Arc<FileMeta>,
    pub(super) part: u32,
    pub(super) total: u32,
    pub(super) offset: u64,
    pub(super) data: Vec<u8>,
    /// Per-article subject token. In article mode each article gets a unique
    /// value; otherwise this mirrors `meta.subject_name`.
    pub(super) subject_name: String,
    /// Per-article yEnc `name=`. In article mode this is unique per segment;
    /// otherwise it mirrors `meta.yenc_name`.
    pub(super) yenc_name: String,
    /// Per-article From header. In article mode each article gets a unique
    /// identity; otherwise this mirrors `meta.from`.
    pub(super) from: String,
    /// Date header for this article: `(rfc_string, unix_timestamp)`.
    /// In article mode each article gets a unique value; otherwise this
    /// mirrors `meta.date`.
    pub(super) date: (Option<String>, Option<u64>),
    /// CRC-32 of the whole file, appended (as `crc32=`) to the `=yend` line —
    /// see the yEnc draft §4 and `nyuu`'s `MultiEncoder` (`lib/article.js`),
    /// which always includes it. `Some` only on the file's *last* part;
    /// computed by the reader task as it streams the file for upload (the
    /// same read the article body comes from), so no separate whole-file
    /// pre-pass is needed before posting can start.
    pub(super) file_crc32: Option<u32>,
}

/// Encoded article ready for NNTP (nyuu `Post` after `generate`).
pub(super) struct ReadyArticle {
    pub(super) task: PostTask,
    pub(super) message_id: String,
    pub(super) headers: Vec<u8>,
    pub(super) encoded: yenc::EncodedPart,
    pub(super) encode_time: Duration,
    pub(super) date: (Option<String>, Option<u64>),
}
