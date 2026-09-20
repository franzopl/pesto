//! Shared run state: counters, event channels and reusable buffers.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

use crate::config::Config;
use crate::progress::{ProgressEvent, ProgressSender};
use crate::resume::ResumeState;

use super::outcome::{FailedTask, PostedSegment};

pub(super) struct Shared {
    pub(super) config: Config,
    /// Server list in failover order (primary first).
    pub(super) servers: Arc<Vec<crate::config::ServerEntry>>,

    pub(super) results: Arc<Mutex<Vec<PostedSegment>>>,
    pub(super) failures: Mutex<Vec<String>>,
    pub(super) failed_tasks: Mutex<Vec<FailedTask>>,
    /// Progress channel; `None` keeps the poster silent (library default).
    pub(super) events: Option<ProgressSender>,
    pub(super) cancelled: Arc<AtomicBool>,
    /// Mirrors an external pause flag (see `post_files_inner`'s
    /// `external_pause`). Checked by `worker()` at the same segment-batch
    /// boundary as `cancelled`; unlike `cancelled` this can flip back to
    /// `false`, resuming the same connection instead of tearing it down.
    pub(super) paused: Arc<AtomicBool>,
    /// Resume state shared among workers. `Some` whenever a resume-state
    /// path was given and this isn't a dry run/`--par2-only` — tracked
    /// unconditionally, regardless of `--resume` (see `validate_run`'s call
    /// site), so an incomplete run always has something to persist.
    pub(super) resume: Option<Arc<Mutex<ResumeState>>>,
    /// Path of the resume state file; `None` when resume tracking is disabled
    /// (dry run / `--par2-only`).
    pub(super) resume_path: Option<PathBuf>,
    /// Directory for the type-1 spool (cached encoded articles) — `Some`
    /// only when `config.resume` is explicitly set, unlike `resume` itself:
    /// spooling writes real article bytes to disk on the posting hot path,
    /// a cost a plain run must never pay just because a resume-state path
    /// happened to be available. See `crate::spool`.
    pub(super) spool_dir: Option<PathBuf>,
    /// Reusable article byte buffers (Phase 12b). Workers return their buffer
    /// here after encoding so the producer and reader tasks can reuse it
    /// instead of allocating a fresh `Vec<u8>` for every article.
    pub(super) pool: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Reusable yEnc *output* bodies (P3 encodeTo). Filled by `encode_part_into`.
    pub(super) encode_pool: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Total number of post attempts that failed and triggered a retry (26d).
    pub(super) total_retries: std::sync::atomic::AtomicUsize,
    /// Newsgroup(s) every article in this run is posted to. When several groups
    /// are configured one is picked at random once per run (see
    /// [`super::pick_post_group`]), so a whole upload stays together in a single
    /// group while the footprint spreads across groups over many runs.
    pub(super) post_group: Vec<String>,
    /// Shared subject/yEnc prefix for [`crate::config::ObfuscateMode::FullShared`],
    /// generated once per run so the archive files and every PAR2 volume land on
    /// the wire under the same random name — see
    /// [`crate::config::ObfuscateMode::FullShared`] for why this trades away
    /// `full`'s per-file randomisation. `None` in every other mode.
    pub(super) release_prefix: Option<String>,
    /// Shared `From` header for [`crate::config::ObfuscateMode::FullShared`],
    /// generated once alongside `release_prefix` so the whole release also posts
    /// under one identity instead of a fresh random sender per file. `None` in
    /// every other mode.
    pub(super) release_from: Option<String>,
    /// Unique ID for this run, used to key [`super::par2_temp_dir`] so concurrent
    /// runs in the same process (`--each`/`--season` with `--jobs > 1`) each
    /// get their own PAR2 temp directory instead of colliding on one shared
    /// by process ID alone.
    pub(super) run_id: u64,
    /// Total number of files in the release (data files + PAR2 index +
    /// volumes), computed once up front from `par2_geometry` before any
    /// worker spawns — see that function's doc comment for why this is known
    /// before PAR2 encoding actually starts. `0` when `config.file_counter`
    /// is off, which callers treat as "no counter" (see `FileMeta::file_index`).
    pub(super) total_files: u32,
    /// Sender into the streaming STAT queue. `prepare_ready` arm 2
    /// (re-STAT a stored id, no POST) needs this; the POST path uses it
    /// after a 240. Taken (dropped) before `finish_and_drain` so the
    /// feeder observes end-of-stream.
    pub(super) check_tx: Mutex<Option<tokio::sync::mpsc::UnboundedSender<PostedSegment>>>,
}

impl Shared {
    /// Take a buffer from the pool, or allocate a fresh one, with fallible allocation.
    /// The returned buffer is always exactly `size` bytes long (content is zero-filled).
    ///
    /// # Errors
    ///
    /// Returns `TryReserveError` if buffer allocation or expansion fails.
    pub(super) fn try_acquire_buffer(&self, size: usize) -> anyhow::Result<Vec<u8>> {
        let mut pool = self.pool.lock().unwrap();
        match pool.pop() {
            Some(mut buf) => {
                buf.try_reserve_exact(size.saturating_sub(buf.len()))
                    .map_err(|e| anyhow::anyhow!("buffer expansion failed: {e}"))?;
                buf.resize(size, 0);
                Ok(buf)
            }
            None => {
                let mut buf = Vec::new();
                buf.try_reserve_exact(size)
                    .map_err(|e| anyhow::anyhow!("buffer allocation failed: {e}"))?;
                buf.resize(size, 0);
                Ok(buf)
            }
        }
    }

    /// Return a buffer to the pool. Oversized or empty buffers are dropped.
    pub(super) fn release_buffer(&self, buf: Vec<u8>) {
        if buf.capacity() > 0 && buf.capacity() <= self.config.article_size * 2 {
            self.pool.lock().unwrap().push(buf);
        }
    }

    pub(super) fn acquire_encode_buf(&self) -> Vec<u8> {
        match self.encode_pool.lock().unwrap().pop() {
            Some(mut buf) => {
                buf.clear();
                buf
            }
            None => Vec::new(),
        }
    }

    pub(super) fn release_encode_buf(&self, buf: Vec<u8>) {
        let cap = self.config.article_size.saturating_mul(3).max(64 * 1024);
        if buf.capacity() > 0 && buf.capacity() <= cap {
            self.encode_pool.lock().unwrap().push(buf);
        }
    }

    /// Emit a progress event, ignoring a dropped or absent receiver.
    pub(super) fn emit(&self, event: ProgressEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }
}
