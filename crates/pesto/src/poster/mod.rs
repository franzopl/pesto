//! Parallel posting: the orchestration that ties together file reading, yEnc
//! encoding, article assembly and the NNTP client.
//!
//! Files are read sequentially by a producer. yEnc runs on a small encode
//! pool (nyuu: one encoder filling a ready-article queue). NNTP workers only
//! POST. If PAR2 recovery exceeds a memory limit, the producer re-reads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, error, info, warn};

use crate::article::{
    default_subject, format_rfc2822, generate_message_id, obfuscated_name,
    obfuscated_name_with_prefix, rand_u64, random_from, Article,
};
use crate::config::{types::MAX_AUTO_PIPELINE_DEPTH, Config, ObfuscateMode};
use crate::nntp::pool::{ConnectionBroker, ConnectionPool, ConnectionSlot};
use crate::progress::{FileEntry, ProgressEvent, ProgressSender, RunMode};
use crate::resume::ResumeState;
use crate::walk::{natural_cmp, InputFile};
use crate::yenc;
use parmesan::encoder::{FileHasher, FileHashes, RecoveryEncoder};
use parmesan::layout;
use parmesan::packet::{self, SliceChecksum};

use parmesan::ops::{
    calculate_geometry, ingest_files_with, CreateOptions as Par2CreateOptions,
    InputFile as Par2InputFile,
};
use parmesan::worker::Par2Worker;

mod check;
use check::spawn_check_coordinator;

/// Compute the PAR2 recovery-set geometry `(slice_size_bytes,
/// total_input_slices, recovery_block_count)` that `producer` will use for
/// this batch of files, given the current config. Pure and cheap — only
/// reads file sizes already collected in `metas`, no I/O — so it can be
/// called before encoding actually starts to seed an exact (not estimated)
/// progress total. Mirrors the geometry logic in `producer` exactly; keep
/// the two in sync.
fn par2_geometry(metas: &[Arc<FileMeta>], config: &Config) -> (usize, usize, usize) {
    let sizes: Vec<u64> = metas.iter().map(|m| m.size).collect();
    par2_geometry_from_sizes(&sizes, config)
}

/// Shared PAR2 geometry for the per-file path and the season path so
/// `--par2-slice-size` / `--par2-slice-count` / `--par2-recovery-count`
/// cannot drift between them.
fn par2_geometry_from_sizes(sizes: &[u64], config: &Config) -> (usize, usize, usize) {
    let files: Vec<Par2InputFile> = sizes
        .iter()
        .enumerate()
        .map(|(i, &size)| Par2InputFile {
            path: PathBuf::new(),
            display_name: i.to_string(),
            size,
        })
        .collect();
    let options = Par2CreateOptions {
        slice_size: config.par2_slice_size,
        slice_count: config.par2_slice_count,
        recovery_count: config.par2_recovery_count,
        recovery_pct: config.par2,
        ..Par2CreateOptions::default()
    };
    match calculate_geometry(&files, &options) {
        Ok(geometry) => geometry,
        Err(_) => {
            // Overflow of the PAR2 slice/recovery ceilings: return the counts
            // so the caller can emit the same error it always has.
            let s = config
                .par2_slice_size
                .map(|s| (s / 64 * 64).max(64))
                .unwrap_or(64);
            let n: usize = sizes
                .iter()
                .map(|sz| (*sz as usize).div_ceil(s.max(1)))
                .sum();
            let rec = config
                .par2_recovery_count
                .unwrap_or(n.saturating_mul(config.par2 as usize) / 100);
            (s, n, rec)
        }
    }
}

/// Split the configured total connection count between upload workers and
/// the check queue. An auto-derived check pool (`check_connections == 0`)
/// is carved out of the total so `-n 50` always means 50 connections to
/// the server, not 50 + a check pool on top — that total is frequently a
/// hard provider-enforced cap. An *explicit* `--check-connections` is a
/// deliberate, separate budget the user stated on purpose, so it's honored
/// additively instead of eating into `--connections`. Returns
/// `(check_conns, upload_conns)`.
fn split_connections(config: &Config, check_enabled: bool) -> (usize, usize) {
    let total_conns = config.total_connections();
    if !check_enabled {
        return (0, total_conns);
    }
    if config.check_connections == 0 {
        // Reserve at least 1 connection for uploading; if the total is too
        // small to spare any for checking (e.g. `-n 1`), checking is
        // silently skipped for this run rather than exceeding the budget.
        let check = config
            .effective_check_connections()
            .min(total_conns.saturating_sub(1));
        (check, total_conns.saturating_sub(check))
    } else {
        (config.check_connections, total_conns)
    }
}

/// A posted segment, retained for later `.nzb` generation.
///
/// `file_path`, `subject_name` and `from` are `Arc`-shared rather than owned
/// `PathBuf`/`String`: every segment is held twice at once — once in
/// `Shared::results`, once again as a `check::QueueItem` in the streaming
/// check queue's per-server heap while it awaits its `STAT` — and these three
/// fields are identical across every segment of the same file (or, for
/// `from` outside paranoid mode, the whole run). Measured on an
/// 83.4 GiB / 116 619-segment run, the two copies together cost ~150 MiB;
/// sharing these three turns the second copy's allocation for them into a
/// refcount bump. `file_name`/`message_id` stay owned `String` — they're
/// unique per segment, so there's nothing to share.
#[derive(Debug, Clone)]
pub struct PostedSegment {
    pub file_name: String,
    /// Absolute filesystem path of the source file, preserved so a post-check
    /// repost can re-read the segment regardless of the current working
    /// directory. `file_name` alone (the published/relative name) is
    /// insufficient — see `FailedTask::file_path` (issue #23), which this
    /// mirrors for the `--check` repost path.
    pub file_path: Arc<Path>,
    pub subject_name: Arc<str>,
    /// The wire identity (Subject/yEnc `name=`) actually used to post this
    /// segment — independent of `subject_name`, which is always the real
    /// filename for NZB purposes regardless of `--obfuscate` (see
    /// `generate`'s doc comment in `nzb.rs`). A `--check` repost of a
    /// missing article must reuse *this*, not `subject_name`, or an
    /// obfuscated release leaks its real name back onto the wire the moment
    /// one article needs reposting. Empty for segments reconstructed from a
    /// parsed `.nzb` (`nzb::parse`), which never re-encode.
    pub wire_name: Arc<str>,
    pub file_size: u64,
    pub part: u32,
    pub total: u32,
    pub message_id: String,
    pub bytes: u64,
    pub from: Arc<str>,
    /// Date header as `(rfc_string, unix_timestamp)`. Both parts are preserved
    /// so fixed dates survive round-trips and retries.
    pub date: (Option<String>, Option<u64>),
    /// CRC-32 of the whole file this segment belongs to. Only meaningful (and
    /// only ever emitted on the `=yend` line) when `part == total` — see
    /// `PostTask::file_crc32`.
    pub full_crc32: u32,
    /// Index into this run's server list (`Config::all_servers()` order) of
    /// the server that actually accepted this article's `240`. The
    /// streaming check queue (`poster::check`) uses this to `STAT` the same
    /// server the article was posted to, instead of guessing — with a
    /// multi-server failover config, different articles from the same run
    /// can legitimately land on different servers, and a provider that
    /// never received an article obviously can't confirm it. Meaningless
    /// (left as `0`) for segments that never go through the check queue:
    /// resume-skipped segments (already confirmed in a prior run) and
    /// dry-run segments (nothing was actually posted).
    pub server_idx: usize,
    /// This file's 1-based position among every file in the release, and the
    /// release's total file count — the `--file-counter` subject prefix.
    /// `(0, 0)` when the flag is off; see `Shared::total_files`. Denormalized
    /// here (rather than looked up via `Shared`) because both the NZB writer
    /// and a `--check` repost rebuild the subject from a `PostedSegment`
    /// alone, long after `Shared` is gone.
    pub file_index: u32,
    pub total_files: u32,
}

/// A segment that failed to post during the upload run. Carries enough
/// information to re-post the *same* article on the end-of-run retry pass.
#[derive(Debug, Clone)]
pub struct FailedTask {
    /// Published name (relative path / base name) used for NZB metadata and
    /// logging. Not a filesystem path — see [`FailedTask::file_path`].
    pub file_name: String,
    /// Absolute filesystem path of the source file, preserved so the end-of-run
    /// retry can re-read the segment regardless of the current working
    /// directory. `file_name` alone is insufficient (issue #23).
    pub file_path: PathBuf,
    /// The Message-ID the in-run attempts used. The end-of-run retry re-posts
    /// with this *same* ID so that, if the article actually reached the server
    /// during the run (e.g. the `240` ack was lost when the connection died),
    /// the server can deduplicate it: it answers `441 … 435 Already exists`,
    /// which is now treated as success instead of producing a duplicate article
    /// under a fresh ID. Mirrors nyuu's same-Message-ID repost strategy.
    pub message_id: String,
    pub subject_name: String,
    /// The yEnc `=ybegin ... name=` value the in-run attempt used —
    /// independent of `subject_name` under `Full`/`Paranoid`/`FullShared`
    /// obfuscation (see `poster/mod.rs`'s `ObfuscateMode` match arms).
    /// Carried through so a repost doesn't fall back to reusing
    /// `subject_name` for both, which would reintroduce the exact-match
    /// signature those modes deliberately avoid.
    pub yenc_name: String,
    pub file_size: u64,
    pub part: u32,
    pub total: u32,
    pub from: String,
    /// Date header as `(rfc_string, unix_timestamp)`. Both are preserved so
    /// fixed dates (which have `Some` RFC but `None` timestamp) are not lost.
    pub date: (Option<String>, Option<u64>),
    /// CRC-32 of the whole file this segment belongs to — see
    /// `PostedSegment::full_crc32`. Only meaningful when `part == total`.
    pub full_crc32: u32,
    /// See `PostedSegment::file_index`/`total_files` — carried through so the
    /// end-of-run retry (which only has `&[FailedTask]`, not `Shared`) can
    /// rebuild the identical subject.
    pub file_index: u32,
    pub total_files: u32,
}

/// The result of a posting run.
#[derive(Debug)]
pub struct PostOutcome {
    pub segments: Vec<PostedSegment>,
    pub failures: Vec<String>,
    /// Segments that never got a `240` even after the in-run blind retry
    /// pass, preserved so the caller can report them.
    pub failed_tasks: Vec<FailedTask>,
    pub cancelled: bool,
    /// The newsgroup(s) actually used for this upload (one entry when multiple
    /// groups are configured, since `pick_post_group` selects one at random).
    pub groups: Vec<String>,
    /// The server(s) that actually accepted at least one article this run —
    /// derived from `PostedSegment::server_idx` on the final, post-check
    /// segment list, not just the configured list. In a multi-server
    /// (failover) config this can legitimately be a subset (a server that
    /// was unreachable all run) or, more commonly, every configured server
    /// that had a connection quota. Empty for `--par2-only`/`--dry-run`.
    pub servers: Vec<String>,
    /// Message-IDs that were posted (`240`) but never confirmed retrievable
    /// via the streaming STAT check, even after every repost attempt. Empty
    /// when `config.check` is disabled. A non-empty list means the run
    /// produced content that is not fully confirmed on the server.
    pub still_missing: Vec<String>,
    /// Set when the run stopped because `producer` returned an error (bad
    /// PAR2 geometry, a memory-budget check, file I/O, …) rather than because
    /// the user cancelled it. `cancelled` is `true` in both cases — callers
    /// that want to tell "the user pressed Ctrl-C" apart from "the run failed
    /// and here's why" should check this field first. See issue #57: without
    /// it, callers had no way to surface the actual failure and could only
    /// print a generic "interrupted" message.
    pub failure_reason: Option<String>,
    /// This run's PAR2 temp directory (see [`par2_temp_dir`]). Always set,
    /// even when PAR2 was never generated — removing a directory that was
    /// never created is a harmless no-op. Callers use this instead of
    /// calling `par2_temp_dir` themselves, so each concurrent `--each`/
    /// `--season` entry (`--jobs > 1`) cleans up only its own directory
    /// instead of a path shared by every run in the process (issue #67).
    pub par2_temp_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct FileMeta {
    path: PathBuf,
    real_name: String,
    subject_name: String,
    yenc_name: String,
    /// Poster identity for this file. In obfuscate mode a fresh random
    /// identity is generated per file so segments cannot be correlated
    /// across files by the From header.
    from: String,
    /// Date header resolved once per file: `(rfc_string, unix_timestamp)`.
    /// Fixed dates have `Some` RFC but `None` timestamp.
    date: (Option<String>, Option<u64>),
    size: u64,
    /// This file's 1-based position among every file in the release (data
    /// files, then the PAR2 index, then the PAR2 volumes) — used for the
    /// `--file-counter` `[filenum/total]` subject prefix. Meaningless
    /// (left as `0`) when `Shared::total_files` is `0`, i.e. the flag is off.
    file_index: u32,
}

/// How many dedicated yEnc workers fill the ready-article queue.
///
/// Encode is off the POST path, so this is not one-SIMD-per-connection.
/// Cap at performance cores. A single encoder on c7i (4c) left post-only
/// movie at 0.85× nyuu (`20260820T091733Z`); `min(cores, conns)` is the
/// fill rate the queue needs at 0 ms mock.
fn encode_concurrency(perf_cores: usize, connections: usize) -> usize {
    perf_cores.min(connections.max(1)).max(1)
}

/// Nyuu `articleQueueBuffer`: `min(round(conns*0.5)+2, 25)`.
fn ready_queue_depth(connections: usize) -> usize {
    let n = connections.max(1);
    let half = n / 2 + n % 2; // round(n*0.5) for integers
    (half + 2).clamp(4, 25)
}

/// Fans posted articles out to per-worker channels instead of one channel
/// shared behind a lock — see the `tx_opt` construction site in
/// `post_files_inner` for why. Each worker owns its `Receiver` outright, so
/// dequeuing never contends with any other worker.
struct TaskDispatcher<T> {
    senders: Vec<tokio::sync::mpsc::Sender<T>>,
    next: std::sync::atomic::AtomicUsize,
}

impl<T: Send> TaskDispatcher<T> {
    fn new(senders: Vec<tokio::sync::mpsc::Sender<T>>) -> Self {
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
    async fn send(&self, task: T) -> Result<(), tokio::sync::mpsc::error::SendError<T>> {
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

struct PostTask {
    meta: Arc<FileMeta>,
    part: u32,
    total: u32,
    offset: u64,
    data: Vec<u8>,
    /// Per-article subject token. In paranoid mode each article gets a unique
    /// value; otherwise this mirrors `meta.subject_name`.
    subject_name: String,
    /// Per-article From header. In paranoid mode each article gets a unique
    /// identity; otherwise this mirrors `meta.from`.
    from: String,
    /// Date header for this article: `(rfc_string, unix_timestamp)`.
    /// In paranoid mode each article gets a unique value; otherwise this
    /// mirrors `meta.date`.
    date: (Option<String>, Option<u64>),
    /// CRC-32 of the whole file, appended (as `crc32=`) to the `=yend` line —
    /// see the yEnc draft §4 and `nyuu`'s `MultiEncoder` (`lib/article.js`),
    /// which always includes it. `Some` only on the file's *last* part;
    /// computed by the reader task as it streams the file for upload (the
    /// same read the article body comes from), so no separate whole-file
    /// pre-pass is needed before posting can start.
    file_crc32: Option<u32>,
}

/// Encoded article ready for NNTP (nyuu `Post` after `generate`).
struct ReadyArticle {
    task: PostTask,
    message_id: String,
    headers: Vec<u8>,
    encoded: yenc::EncodedPart,
    encode_time: Duration,
    date: (Option<String>, Option<u64>),
}

struct Shared {
    config: Config,
    /// Server list in failover order (primary first).
    servers: Arc<Vec<crate::config::ServerEntry>>,

    results: Arc<Mutex<Vec<PostedSegment>>>,
    failures: Mutex<Vec<String>>,
    failed_tasks: Mutex<Vec<FailedTask>>,
    /// Progress channel; `None` keeps the poster silent (library default).
    events: Option<ProgressSender>,
    cancelled: Arc<AtomicBool>,
    /// Mirrors an external pause flag (see `post_files_inner`'s
    /// `external_pause`). Checked by `worker()` at the same segment-batch
    /// boundary as `cancelled`; unlike `cancelled` this can flip back to
    /// `false`, resuming the same connection instead of tearing it down.
    paused: Arc<AtomicBool>,
    /// Resume state shared among workers. `Some` whenever a resume-state
    /// path was given and this isn't a dry run/`--par2-only` — tracked
    /// unconditionally, regardless of `--resume` (see `validate_run`'s call
    /// site), so an incomplete run always has something to persist.
    resume: Option<Arc<Mutex<ResumeState>>>,
    /// Path of the resume state file; `None` when resume tracking is disabled
    /// (dry run / `--par2-only`).
    resume_path: Option<PathBuf>,
    /// Directory for the type-1 spool (cached encoded articles) — `Some`
    /// only when `config.resume` is explicitly set, unlike `resume` itself:
    /// spooling writes real article bytes to disk on the posting hot path,
    /// a cost a plain run must never pay just because a resume-state path
    /// happened to be available. See `crate::spool`.
    spool_dir: Option<PathBuf>,
    /// Reusable article byte buffers (Phase 12b). Workers return their buffer
    /// here after encoding so the producer and reader tasks can reuse it
    /// instead of allocating a fresh `Vec<u8>` for every article.
    pool: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Reusable yEnc *output* bodies (P3 encodeTo). Filled by `encode_part_into`.
    encode_pool: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Total number of post attempts that failed and triggered a retry (26d).
    total_retries: std::sync::atomic::AtomicUsize,
    /// Newsgroup(s) every article in this run is posted to. When several groups
    /// are configured one is picked at random once per run (see
    /// [`pick_post_group`]), so a whole upload stays together in a single group
    /// while the footprint spreads across groups over many runs.
    post_group: Vec<String>,
    /// Shared subject/yEnc prefix for [`ObfuscateMode::FullShared`], generated
    /// once per run so the archive files and every PAR2 volume land on the
    /// wire under the same random name — see [`ObfuscateMode::FullShared`] for
    /// why this trades away `full`'s per-file randomisation. `None` in every
    /// other mode.
    release_prefix: Option<String>,
    /// Shared `From` header for [`ObfuscateMode::FullShared`], generated once
    /// alongside `release_prefix` so the whole release also posts under one
    /// identity instead of a fresh random sender per file. `None` in every
    /// other mode.
    release_from: Option<String>,
    /// Unique ID for this run, used to key [`par2_temp_dir`] so concurrent
    /// runs in the same process (`--each`/`--season` with `--jobs > 1`) each
    /// get their own PAR2 temp directory instead of colliding on one shared
    /// by process ID alone.
    run_id: u64,
    /// Total number of files in the release (data files + PAR2 index +
    /// volumes), computed once up front from `par2_geometry` before any
    /// worker spawns — see that function's doc comment for why this is known
    /// before PAR2 encoding actually starts. `0` when `config.file_counter`
    /// is off, which callers treat as "no counter" (see `FileMeta::file_index`).
    total_files: u32,
}

impl Shared {
    /// Take a buffer from the pool, or allocate a fresh one, with fallible allocation.
    /// The returned buffer is always exactly `size` bytes long (content is zero-filled).
    ///
    /// # Errors
    ///
    /// Returns `TryReserveError` if buffer allocation or expansion fails.
    fn try_acquire_buffer(&self, size: usize) -> anyhow::Result<Vec<u8>> {
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
    fn release_buffer(&self, buf: Vec<u8>) {
        if buf.capacity() > 0 && buf.capacity() <= self.config.article_size * 2 {
            self.pool.lock().unwrap().push(buf);
        }
    }

    fn acquire_encode_buf(&self) -> Vec<u8> {
        match self.encode_pool.lock().unwrap().pop() {
            Some(mut buf) => {
                buf.clear();
                buf
            }
            None => Vec::new(),
        }
    }

    fn release_encode_buf(&self, buf: Vec<u8>) {
        let cap = self.config.article_size.saturating_mul(3).max(64 * 1024);
        if buf.capacity() > 0 && buf.capacity() <= cap {
            self.encode_pool.lock().unwrap().push(buf);
        }
    }
}

impl Shared {
    /// Emit a progress event, ignoring a dropped or absent receiver.
    fn emit(&self, event: ProgressEvent) {
        if let Some(tx) = &self.events {
            let _ = tx.send(event);
        }
    }
}

/// Post every file in `files` to the groups configured in `config`.
///
/// This is the silent entry point; use [`post_files_with_progress`] to observe
/// the run through a [`ProgressEvent`] channel. Build the [`InputFile`] list
/// with [`crate::walk::expand_inputs`], which also expands directories.
pub async fn post_files(config: &Config, files: &[InputFile]) -> Result<PostOutcome> {
    post_files_with_progress(config, files, None, None, None).await
}

/// Post every file in `files`, emitting [`ProgressEvent`]s on `events`.
///
/// `resume_state_path` is the path of the `.pesto-state` sidecar file.
/// Progress is tracked in memory whenever this path is given, regardless of
/// `config.resume` — that flag only controls whether a *prior* run's
/// on-disk state at this path is loaded and used to skip already-posted
/// segments. At the end of the run, the state is written to disk once if
/// the run ended incomplete (so a later `--resume` has something to load),
/// or deleted if it ended complete (nothing left to resume).
///
/// Passing `None` for `events` keeps the poster silent (library default).
pub async fn post_files_with_progress(
    config: &Config,
    files: &[InputFile],
    events: Option<ProgressSender>,
    resume_state_path: Option<&Path>,
    entry_label: Option<&str>,
) -> Result<PostOutcome> {
    post_files_with_progress_and_cancel(config, files, events, resume_state_path, None, entry_label)
        .await
}

/// Like [`post_files_with_progress`] but accepts an external cancel flag.
///
/// Setting `external_cancel` to `true` causes the run to stop at the next
/// segment boundary, exactly as if the user had pressed Ctrl-C. This is the
/// integration point for embedding applications such as `upapasta`.
pub async fn post_files_with_progress_and_cancel(
    config: &Config,
    files: &[InputFile],
    events: Option<ProgressSender>,
    resume_state_path: Option<&Path>,
    external_cancel: Option<Arc<AtomicBool>>,
    entry_label: Option<&str>,
) -> Result<PostOutcome> {
    post_files_inner(
        config,
        files,
        events,
        resume_state_path,
        external_cancel,
        entry_label,
        None,
        None,
    )
    .await
}

/// Like [`post_files_with_progress_and_cancel`], but lets the caller supply a
/// [`ConnectionBroker`] whose already-authenticated connections are checked
/// out for this run and checked back in (instead of disconnected) when done,
/// so a later call sharing the same broker reuses them without paying a
/// fresh TLS+AUTH handshake, and/or an `external_pause` flag: setting it to
/// `true` suspends every posting worker at the next segment-batch boundary
/// (connections stay open and kept alive) and setting it back to `false`
/// resumes immediately, without paying a reconnect. Only the posting phase
/// is pausable — PAR2 generation, compression and the final check/repost
/// passes run to completion regardless, the same phase scoping `cancel`
/// already has.
///
/// This is CLI-internal plumbing for `--each`/`--season` batching (see
/// `run_batch` in `bin/pesto.rs`) — embedders should use
/// [`post_files_with_progress_and_cancel`], `post`, `post_cancelable` or
/// `post_pausable`, which always build and tear down their own pool per
/// call and remain unaffected by the `broker` parameter (`broker: None`).
#[allow(clippy::too_many_arguments)]
pub async fn post_files_inner(
    config: &Config,
    files: &[InputFile],
    events: Option<ProgressSender>,
    resume_state_path: Option<&Path>,
    external_cancel: Option<Arc<AtomicBool>>,
    entry_label: Option<&str>,
    broker: Option<Arc<ConnectionBroker>>,
    external_pause: Option<Arc<AtomicBool>>,
) -> Result<PostOutcome> {
    configure_rayon(config.threads);

    // Resume state is tracked in memory for *every* run that could plausibly
    // need it (not gated behind --resume), so a run that ends incomplete
    // always has something to persist for a later retry — without
    // `--resume`, deciding you need it only happens *after* a failure, which
    // is too late if nothing was ever recorded (see issue #18). Only
    // *loading* a prior run's on-disk state (to skip already-posted
    // segments) stays gated behind --resume: silently trusting whatever
    // `.pesto-state` file happens to already sit next to the target, without
    // being asked to, is exactly the "stale state reused blindly" hazard
    // issue #18 warns about.
    let (resume_arc, resume_path_owned) = if !config.dry_run && !config.par2_only {
        if let Some(rp) = resume_state_path {
            let state = if config.resume {
                ResumeState::load(rp)?
            } else {
                ResumeState::default()
            };
            (Some(Arc::new(Mutex::new(state))), Some(rp.to_path_buf()))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    // Type-1 spool: only when --resume was actually passed (see the field
    // doc on `Shared::spool_dir` for why this is a stricter condition than
    // `resume_arc` itself).
    let spool_dir_owned = if config.resume {
        resume_path_owned.as_deref().map(crate::spool::spool_dir)
    } else {
        None
    };

    // Posting parameters that change how the whole input is chunked or
    // named — compared against whatever fingerprint a loaded state was
    // recorded under. A mismatch (e.g. this run's --article-size differs
    // from the run that originally populated the state) means every
    // recorded Message-ID could reference the wrong byte range, so the
    // *entire* state is discarded rather than trusted partially — see
    // `resume::RunFingerprint` and GitHub issue #18.
    let run_fingerprint = crate::resume::RunFingerprint::from_config(config);
    if let Some(resume) = &resume_arc {
        let mut state = resume.lock().unwrap();
        let had_segments = !state.is_empty();
        if !state.validate_run(&run_fingerprint) {
            eprintln!(
                "resume: posting parameters changed since the saved state was recorded \
                 (--article-size/--obfuscate/--compress/--par2/--file-counter) — ignoring it \
                 and starting fresh"
            );
        } else if had_segments {
            eprintln!(
                "resuming: {} segment(s) already posted, skipping",
                state.len()
            );
        }
    }

    // Generated once per run (not per file) so every file posted under
    // `FullShared`/`Light` — archive parts and PAR2 volumes alike — shares
    // the same wire name prefix and sender identity. See
    // `ObfuscateMode::FullShared` and `ObfuscateMode::Light`. Randomly
    // generated fresh by default, which would otherwise make a `--resume`
    // run's segments unmatchable against a prior run's (its wire identity,
    // though not the resume key itself, would differ) — a compatible prior
    // state (see `validate_run` above) reuses the same identity instead of
    // generating a new one; see issue #18's resume follow-up discussion.
    let (release_prefix, release_from) = if matches!(
        config.obfuscate,
        ObfuscateMode::FullShared | ObfuscateMode::Light
    ) {
        let reused = resume_arc.as_ref().and_then(|r| {
            r.lock()
                .unwrap()
                .release_identity()
                .map(|(p, f)| (p.to_string(), f.to_string()))
        });
        let (prefix, from) = reused.unwrap_or_else(|| (obfuscated_name(), random_from()));
        if let Some(resume) = &resume_arc {
            resume
                .lock()
                .unwrap()
                .set_release_identity(prefix.clone(), from.clone());
        }
        (Some(prefix), Some(from))
    } else {
        (None, None)
    };

    let mut metas = Vec::with_capacity(files.len());
    for (idx, input) in files.iter().enumerate() {
        let path = &input.path;
        let md = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("reading metadata of `{}`", path.display()))?;
        if !md.is_file() {
            bail!("`{}` is not a regular file", path.display());
        }
        // `real_name` is the published name: a relative path like
        // `season01/ep01.mkv` for files found inside a directory argument.
        let real_name = input.name.clone();
        let size = md.len();

        if let Some(resume) = &resume_arc {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            let file_fp = crate::resume::FileFingerprint { size, mtime };
            let mut state = resume.lock().unwrap();
            if !state.file_matches(&real_name, &file_fp) {
                if config.par2 > 0 {
                    // PAR2 recovery blocks are computed over the whole
                    // recovery set together, not per file — one file's
                    // content changing invalidates every volume's segments
                    // too, not just this file's own (see
                    // `forget_all_segments`'s doc comment). PAR2 volumes
                    // never go through this per-file check themselves
                    // (they're generated later, straight into the posting
                    // queue — see `push_par2_file`), so this is the only
                    // place that can catch it.
                    eprintln!(
                        "resume: `{real_name}` changed size or modification time since the \
                         saved state was recorded — ignoring all saved segments, including \
                         PAR2 volumes, since recovery data no longer matches this file"
                    );
                    state.forget_all_segments();
                } else {
                    eprintln!(
                        "resume: `{real_name}` changed size or modification time since the \
                         saved state was recorded — ignoring its saved segments and \
                         re-posting it"
                    );
                    state.forget_file(&real_name);
                }
            }
            state.record_file(&real_name, file_fp);
        }
        let (subject_name, yenc_name, from) = match config.obfuscate {
            ObfuscateMode::None => {
                let wn = wire_name(&real_name).to_string();
                (wn.clone(), wn, config.from.clone())
            }
            ObfuscateMode::Full | ObfuscateMode::Paranoid => {
                // 0-byte files have no content to protect; use the real name
                // so download clients (e.g. SABnzbd) can place them correctly
                // without needing md5_16k matching (which fails for empty files).
                if size == 0 {
                    let wn = wire_name(&real_name).to_string();
                    (wn.clone(), wn, random_from())
                } else {
                    // Independently-random subject and yEnc name: reusing the
                    // same string for both leaves an exact-match signature
                    // (Subject header == yEnc body name=) that fingerprints
                    // this specific tool's obfuscation, undermining part of
                    // what obfuscation is for.
                    (obfuscated_name(), obfuscated_name(), random_from())
                }
            }
            ObfuscateMode::Light | ObfuscateMode::FullShared => {
                let from = release_from.clone().unwrap_or_default();
                if size == 0 {
                    let wn = wire_name(&real_name).to_string();
                    (wn.clone(), wn, from)
                } else {
                    let prefix = release_prefix.as_deref().unwrap_or_default();
                    // A `--compress-volume-size` archive part carries a
                    // volume suffix (`.partNN.rar`, `.7z.NNN`) that indexers
                    // key their "same release" grouping off of — preserve it
                    // verbatim instead of the generic numbered suffix below,
                    // or the release fails to group under full-shared/light
                    // obfuscation (issue #68).
                    let name = if let Some(suffix) = crate::compress::volume_suffix(&real_name) {
                        format!("{prefix}{suffix}")
                    } else {
                        let ext = Path::new(&real_name)
                            .extension()
                            .map(|e| format!(".{}", e.to_string_lossy()))
                            .unwrap_or_default();
                        // A single-file release (the common case: one archive,
                        // or one loose file) keeps a bare `prefix.ext`;
                        // multiple unrelated files use a `.partNN` marker
                        // ahead of the extension instead of a bare `-NN`
                        // suffix. Indexer subject-cleaning regexes (e.g.
                        // nZEDb's `CollectionsCleaning::generic()`) strip a
                        // known `\.part\d*(\.rar)?` prefix together with the
                        // trailing extension as one unit — the same way they
                        // already strip `.volNNN+NNN.par2` — so every file
                        // collapses back to the same collection key. A bare
                        // `-NN` before the extension isn't part of that
                        // pattern and survives cleaning, giving each file its
                        // own key and defeating the grouping `full-shared`/
                        // `light` exist for (confirmed empirically: real
                        // upload's `.par2`/`.volNNN+NNN.par2` set grouped on
                        // binsearch, its loose `-NN.mkv` files did not).
                        if files.len() == 1 {
                            format!("{prefix}{ext}")
                        } else {
                            format!("{prefix}.part{:02}{ext}", idx + 1)
                        }
                    };
                    // The shared prefix stays on the subject — that's what
                    // indexers actually key "same release" grouping off of
                    // (issue #58/#68, both subject-based). Under `light`,
                    // the yEnc body name= is that same string verbatim
                    // (issue #106's "option 1" — restores full-shared's
                    // pre-0.6.1 behavior for indexers that key grouping off
                    // an exact Subject/yEnc-name match). Under `full-shared`,
                    // the yEnc name= starts with that same prefix but adds
                    // its own random suffix instead: an indexer that can
                    // only see the yEnc body still recognises the article as
                    // part of the release, while the random suffix avoids
                    // an exact Subject/yEnc match.
                    let yenc_name = if config.obfuscate == ObfuscateMode::Light {
                        name.clone()
                    } else {
                        obfuscated_name_with_prefix(prefix)
                    };
                    (name, yenc_name, from)
                }
            }
        };
        let date = resolve_date(config.date.as_deref());
        metas.push(Arc::new(FileMeta {
            path: path.clone(),
            real_name,
            subject_name,
            yenc_name,
            from,
            date,
            size: md.len(),
            // Assigned below, once `metas`' final posting order is settled —
            // see the `config.file_counter` pass after the File-ID sort.
            file_index: 0,
        }));
    }

    // PAR2 numbers its input blocks by walking the recovery-set files in
    // File-ID order (par2 spec, Main packet). The producer feeds slices to the
    // encoder in `metas` order, so for a multi-file set to be repairable
    // `metas` must already be sorted by File ID. A single-file set is
    // trivially ordered; with PAR2 disabled the order is irrelevant.
    if config.par2 > 0 && metas.len() > 1 {
        let mut keyed = Vec::with_capacity(metas.len());
        for meta in &metas {
            let md5_16k = file_md5_16k(&meta.path, meta.size).await?;
            // Use wire_name so the File ID matches what the PAR2 packets will
            // store — the sort order for recovery blocks must be consistent.
            let file_id = packet::compute_file_id(&md5_16k, meta.size, wire_name(&meta.real_name));
            keyed.push((file_id, meta.clone()));
        }
        keyed.sort_by_key(|(file_id, _)| *file_id);
        metas = keyed.into_iter().map(|(_, meta)| meta).collect();
    }

    // `--file-counter`'s `[filenum/total]` numbers every file in the release,
    // so it can only be assigned now that the full file list is settled — not
    // at push time above.
    //
    // The number must follow the release's own order (`part1.rar` is `[1/N]`,
    // the PAR2 set closes it out), *not* `metas`' order: the File-ID sort
    // above keys on an MD5, i.e. it shuffles the volumes with respect to their
    // volume numbers. Indexers sort a collection by Subject and the counter is
    // the Subject's leading field, so inheriting that order listed the release
    // scrambled — a real upload came out with `part4.rar` as `[1/14]`.
    // `metas` itself stays in File-ID order, since the producer feeds PAR2
    // slices in that order and the par2 spec requires it (see the sort above).
    if config.file_counter {
        let mut order: Vec<usize> = (0..metas.len()).collect();
        order.sort_by(|&a, &b| natural_cmp(&metas[a].real_name, &metas[b].real_name));
        let mut rank = vec![0u32; metas.len()];
        for (pos, &idx) in order.iter().enumerate() {
            rank[idx] = pos as u32 + 1;
        }
        metas = metas
            .into_iter()
            .zip(rank)
            .map(|(m, file_index)| {
                Arc::new(FileMeta {
                    file_index,
                    ..(*m).clone()
                })
            })
            .collect();
    }

    let mut initial_segments = 0;
    for meta in &metas {
        initial_segments += yenc::segments(meta.size, config.article_size).len() as u64;
    }

    info!(
        entry = entry_label.unwrap_or(""),
        files = metas.len(),
        segments = initial_segments,
        article_size = config.article_size,
        par2_pct = config.par2,
        "upload plan"
    );

    let servers: Arc<Vec<crate::config::ServerEntry>> = Arc::new(config.all_servers().collect());
    let total_conns = config.total_connections();

    let check_enabled = config.check && !config.dry_run && !config.par2_only;
    let (check_conns, upload_conns) = split_connections(config, check_enabled);

    let worker_count = if config.par2_only {
        0
    } else {
        upload_conns.max(1).min(initial_segments.max(1) as usize)
    };
    info!(
        workers = worker_count,
        check_workers = check_conns,
        connections = total_conns,
        "connection pool"
    );

    // Pre-seed the buffer pool with enough buffers to keep all workers and the
    // double-buffer reader supplied without allocating during the hot path.
    let pool_size = worker_count + 4;
    let initial_pool: Vec<Vec<u8>> = (0..pool_size)
        .map(|_| vec![0u8; config.article_size])
        .collect();

    // Unique per call to this function, i.e. per posting run — not per
    // process. `--each`/`--season` with `--jobs > 1` spawn several runs
    // concurrently in the same process; each needs its own PAR2 temp
    // directory (see `par2_temp_dir`'s doc comment / GitHub issue #67).
    static RUN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let run_id = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);

    // Computed once, unconditionally, and reused below for `total_files`,
    // `par2_bytes_hint`, and the `--par2-before-upload` decision.
    // `par2_geometry` is metadata-only (file sizes + config, no I/O — see
    // its doc comment) so this is exact, not an estimate, and safe to
    // compute before `producer` actually runs the encoder.
    let (par2_slice_size, _total_slices, recovery_count) = par2_geometry(&metas, config);

    // Total release file count for `--file-counter`: data files, plus (when
    // there's any recovery data to write) the index file and every volume
    // `plan_volumes` will produce. Gated on `recovery_count > 0`, exactly
    // like `producer`'s own `worker_opt`/index-write gate — not on
    // `config.par2 > 0` directly, since `par2_geometry` can still land on
    // zero recovery blocks with PAR2 "on" (e.g. a tiny release where
    // `total_slices * pct / 100` floors to 0), in which case `producer`
    // never writes an index or volumes at all.
    let total_files: u32 = if config.file_counter {
        let par2_file_count = if recovery_count > 0 {
            1 + layout::plan_volumes(recovery_count as u32).len()
        } else {
            0
        };
        (metas.len() + par2_file_count) as u32
    } else {
        0
    };

    let shared = Arc::new(Shared {
        config: config.clone(),
        servers,

        results: Arc::new(Mutex::new(Vec::new())),
        failures: Mutex::new(Vec::new()),
        failed_tasks: Mutex::new(Vec::new()),
        events,
        cancelled: Arc::new(AtomicBool::new(false)),
        paused: Arc::new(AtomicBool::new(false)),
        resume: resume_arc,
        resume_path: resume_path_owned,
        spool_dir: spool_dir_owned,
        pool: Arc::new(Mutex::new(initial_pool)),
        encode_pool: Arc::new(Mutex::new(Vec::new())),
        total_retries: std::sync::atomic::AtomicUsize::new(0),
        post_group: pick_post_group(&config.groups),
        release_prefix,
        release_from,
        run_id,
        total_files,
    });

    // Announce the work plan: one `FileEntry` per source file, with the
    // segment count posting will use. PAR2 files are added later, once the
    // data pass has computed them, via `ProgressEvent::QueueExtended`.
    let (mode, target) = if config.par2_only {
        (RunMode::Par2Only, None)
    } else if config.dry_run {
        (RunMode::DryRun, None)
    } else {
        // Every configured server (primary + extra_servers) gets a share of
        // worker connections from the start (see `assign_workers`), unlike
        // `groups` — where only one of the configured groups is picked at
        // random per run — so the full server list is already known here,
        // not just after the fact. Reporting only `config.host` (the
        // primary) used to make a failover/multi-provider run look
        // single-server for its entire duration.
        let all_servers: Vec<_> = config.all_servers().collect();
        let label = target_label(&all_servers, config.total_connections());
        (RunMode::Post, Some(label))
    };
    let _ = &target; // used below
                     // Exact PAR2 recovery-set geometry, computed with the same formula
                     // `producer` will actually use — not an estimate. This lets the total
                     // segment/byte counts be seeded correctly up front instead of jumping
                     // once PAR2 encoding finishes and its volumes get queued for posting.
    let (par2_bytes_hint, par2_segments_hint) =
        if config.par2 > 0 && !config.par2_only && !config.dry_run {
            let recovery_bytes = recovery_count as u64 * par2_slice_size as u64;
            let packet_overhead = recovery_count as u64 * packet::HEADER_LEN as u64;
            // Small fixed overhead for the index file's Main/FileDesc/IFSC
            // packets — negligible next to recovery_bytes, not worth
            // computing exactly for a progress estimate.
            let index_est = metas.len() as u64 * 128 + 4096;
            let bytes_hint = recovery_bytes + packet_overhead + index_est;
            let segments_hint = yenc::segments(bytes_hint, config.article_size).len() as u64;
            (bytes_hint, segments_hint)
        } else {
            (0, 0)
        };
    let file_entries = metas
        .iter()
        .map(|m| FileEntry {
            name: m.real_name.clone(),
            segments: yenc::segments(m.size, config.article_size).len() as u64,
            bytes: m.size,
        })
        .collect();
    shared.emit(ProgressEvent::Started {
        mode,
        files: file_entries,
        connections: worker_count,
        check_connections: check_conns,
        target,
        par2_bytes_hint,
        par2_segments_hint,
    });

    // Warn when the release contains 0-byte files: download clients identify
    // obfuscated files by their md5_16k hash and cannot match empty files,
    // so they end up misplaced after download.  Compression (--compress=rar
    // or --compress=7z) avoids the issue entirely.
    let zero_byte_names: Vec<&str> = metas
        .iter()
        .filter(|m| m.size == 0)
        .map(|m| wire_name(&m.real_name))
        .collect();
    if !zero_byte_names.is_empty() {
        let names = zero_byte_names.join(", ");
        shared.emit(ProgressEvent::Status {
            text: format!(
                "warning: release contains {n} empty file(s) ({names}); \
                 download clients cannot place empty files automatically — \
                 consider using --compress=rar or --compress=7z",
                n = zero_byte_names.len(),
            ),
        });
    }

    let cancel_handle = {
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
    };

    // `--par2-before-upload`: when there's real recovery data to generate,
    // run PAR2 generation to completion *before* opening any NNTP
    // connection. `producer(.., None, .., 0)` writes every index/volume
    // file to `par2_dir` without posting (the `tx_opt: None` path already
    // used by `--par2-only`), and `active_connections: 0` means the PAR2
    // memory budget isn't shrunk to make room for connections that don't
    // exist yet — the connection pool and its workers only spin up further
    // down, once this is done. `post_pregenerated_release` then posts the
    // data files followed by the files this call already wrote, back to
    // back with no gap. See `ROADMAP.md` and GitHub issue #68.
    let will_defer = config.par2_before_upload && recovery_count > 0 && worker_count > 0;
    let par2_dir = par2_temp_dir(config.par2_temp_dir.as_deref(), run_id);
    let mut failure_reason: Option<String> = None;
    if will_defer {
        if let Err(e) = producer(metas.clone(), None, shared.clone(), 0).await {
            let description = format!("producer error: {e:#}");
            error!(error = %e, "producer error");
            shared.cancelled.store(true, Ordering::Relaxed);
            shared.emit(ProgressEvent::Failed {
                description: description.clone(),
            });
            failure_reason = Some(description);
        }
    }

    // Streaming check: every segment that gets a clean `240` is queued here
    // and STAT-checked a few seconds later, concurrently with the rest of
    // the upload, instead of waiting for the whole run to finish.
    let mut check_coordinator = if check_enabled && check_conns > 0 {
        Some(spawn_check_coordinator(
            config.clone(),
            shared.post_group.clone(),
            Arc::clone(&shared.results),
            shared.events.clone(),
            Some(Arc::clone(&shared.cancelled)),
            check_conns,
        ))
    } else {
        None
    };
    let check_tx = check_coordinator.as_ref().map(|c| c.sender());

    crate::memory::set_phase(crate::memory::Phase::Posting);
    let t_post_start = std::time::Instant::now();
    let mut handles = Vec::with_capacity(worker_count);
    let mut encode_handles = Vec::new();
    let tx_opt = if worker_count > 0 {
        let ready_n = ready_queue_depth(worker_count);
        let post_depth = (ready_n / worker_count).max(2);
        let mut post_senders = Vec::with_capacity(worker_count);
        let mut post_receivers = Vec::with_capacity(worker_count);
        for _ in 0..worker_count {
            let (tx, rx) = tokio::sync::mpsc::channel(post_depth);
            post_senders.push(tx);
            post_receivers.push(rx);
        }
        let post_disp = Arc::new(TaskDispatcher::new(post_senders));
        let slots = match &broker {
            Some(broker) => broker.checkout(worker_count).await,
            None => ConnectionPool::build(shared.servers.clone(), worker_count).into_slots(),
        };
        for (idx, (slot, rx)) in slots.into_iter().zip(post_receivers).enumerate() {
            handles.push(tokio::spawn(worker(
                shared.clone(),
                rx,
                idx,
                slot,
                check_tx.clone(),
                broker.clone(),
            )));
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

    // Producer (or, when PAR2 was already generated above, the
    // already-generated-files poster) runs in this thread. Skipped entirely
    // if the generation phase above already failed — nothing valid to post.
    if failure_reason.is_none() {
        let result = if will_defer {
            let result = match tx_opt.as_ref() {
                Some(tx) => {
                    post_pregenerated_release(&metas, &par2_dir, recovery_count, tx, &shared).await
                }
                None => Ok(()),
            };
            // Unlike `producer`, which owns (and so drops) `tx_opt` itself,
            // `post_pregenerated_release` only borrows `tx` — drop the real
            // owner explicitly now that posting is done. Without this the
            // channel never closes, `rx.recv()` in each worker never sees
            // the end of the stream, and the join loop below hangs forever
            // waiting for workers that are just idling.
            drop(tx_opt);
            result
        } else {
            producer(metas, tx_opt, shared.clone(), total_conns).await
        };
        if let Err(e) = result {
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

    for handle in encode_handles {
        let _ = handle.await;
    }
    for handle in handles {
        let _ = handle.await;
    }

    // The upload's own connections are now idle — reuse that budget to
    // drain any remaining check backlog faster instead of leaving it to a
    // handful of dedicated connections that were sized for running
    // *alongside* the upload, not for a burst catch-up at the end.
    if let Some(coordinator) = check_coordinator.as_mut() {
        coordinator.scale_up(upload_conns);
    }

    cancel_handle.abort();

    let mut failures = std::mem::take(&mut *shared.failures.lock().unwrap());
    let mut failed_tasks = std::mem::take(&mut *shared.failed_tasks.lock().unwrap());
    let cancelled = shared.cancelled.load(Ordering::Relaxed);

    // Blind retry for segments that never got a `240` in the main loop
    // (connection drops, timeouts, etc — never confirmed by the server at
    // all). Recovered segments flow into the same streaming check queue as
    // everything else, so they get the same STAT confirmation before the
    // run reports them as posted.
    if !failed_tasks.is_empty() && !cancelled {
        let n = failed_tasks.len();
        info!(count = n, "retrying segments that failed during upload");
        let recovered = repost_failed_tasks(
            config,
            &failed_tasks,
            &shared.post_group,
            shared.events.as_ref(),
            Some(&shared.cancelled),
        )
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, "retry: repost_failed_tasks error");
            Vec::new()
        });
        let recovered_keys: std::collections::HashSet<(String, u32, u32)> = recovered
            .iter()
            .map(|s| (s.file_name.clone(), s.part, s.total))
            .collect();
        for seg in recovered {
            if let Some(tx) = &check_tx {
                let _ = tx.send(seg.clone());
            }
            shared.results.lock().unwrap().push(seg);
        }
        failed_tasks.retain(|t| !recovered_keys.contains(&(t.file_name.clone(), t.part, t.total)));
        failures.retain(|f| {
            !recovered_keys.iter().any(|(name, part, total)| {
                f.starts_with(name.as_str()) && f.contains(&format!("{part}/{total}"))
            })
        });
    }

    // The PAR2 files posted in normal mode are written to a per-process temp
    // directory purely as an intermediate. Cleanup is deliberately *not* done
    // here: the streaming check's repost path may still need to re-read a
    // PAR2 file's bytes while it drains below. The caller is responsible for
    // removing `par2_temp_dir()` once it's truly done with the run (see
    // `run_single_upload` / `run_upload`).
    drop(check_tx);
    crate::memory::set_phase(crate::memory::Phase::Check);
    let mut still_missing = if let Some(coordinator) = check_coordinator {
        coordinator.finish_and_drain().await
    } else {
        Vec::new()
    };

    // One more, bounded automatic recovery attempt for a small stubborn
    // tail. The common real-world case this targets: posting finished, the
    // streaming check failed to confirm a handful of articles even after
    // every `check_post_retries` round, and the NZB is about to be refused.
    // Reposting those few articles right here — still in this same process,
    // with the source files still on disk — is strictly cheaper and simpler
    // than requiring the user to notice the failure and rerun with
    // `--resume` by hand. Only kicks in when the leftover count is small
    // enough (`check_recover_percent`/`check_recover_max`) to still count as
    // "cheap": a release with a large fraction missing looks like a
    // systemic server problem, not a handful of unlucky articles, and
    // retrying that automatically would just hammer an already-struggling
    // server.
    if !still_missing.is_empty() && !cancelled {
        let total = shared.results.lock().unwrap().len();
        if is_cheap_to_recover(still_missing.len(), total, config) {
            let candidates: Vec<PostedSegment> = {
                let results = shared.results.lock().unwrap();
                results
                    .iter()
                    .filter(|s| still_missing.contains(&s.message_id))
                    .cloned()
                    .collect()
            };
            // `recover_missing` itself emits `CheckRecoverStarted`/
            // `CheckRecoverProgress` (structured, so the renderer can show a
            // real progress box instead of a one-shot status line — see
            // `ui::terminal`'s "recover" box).
            //
            // `recover_missing` returns *fresh* Message-IDs (every repost
            // gets a new one — see `repost_one`), so its output can never be
            // matched directly against `still_missing`'s old ids. Snapshot
            // old-id -> (file_name, part) identity before the candidates are
            // moved into the call, so the retain below can match by that
            // identity instead.
            let old_identity: std::collections::HashMap<String, (String, u32)> = candidates
                .iter()
                .map(|c| (c.message_id.clone(), (c.file_name.clone(), c.part)))
                .collect();
            let recovered = check::recover_missing(
                config,
                &shared.post_group,
                candidates,
                shared.events.as_ref(),
            )
            .await;
            for seg in &recovered {
                let mut results = shared.results.lock().unwrap();
                if let Some(existing) = results
                    .iter_mut()
                    .find(|s| s.file_name == seg.file_name && s.part == seg.part)
                {
                    *existing = seg.clone();
                }
            }
            let recovered_keys: std::collections::HashSet<(String, u32)> = recovered
                .iter()
                .map(|s| (s.file_name.clone(), s.part))
                .collect();
            still_missing.retain(|id| {
                !old_identity
                    .get(id)
                    .is_some_and(|key| recovered_keys.contains(key))
            });
        }
    }

    // Whatever is left in `still_missing` at this point is confirmed bad:
    // the original POST got a `240`, but every STAT check and every repost
    // attempt (both the normal `check_post_retries` rounds and the recovery
    // pass above) failed to make the article retrievable. Its recorded
    // Message-ID must be forgotten now — otherwise a later `--resume` would
    // trust that known-bad ID and silently skip re-posting the segment,
    // producing an NZB that looks complete but references an article that
    // was never actually confirmed present.
    if let Some(resume) = &shared.resume {
        if !still_missing.is_empty() {
            let results = shared.results.lock().unwrap();
            let mut state = resume.lock().unwrap();
            for id in &still_missing {
                if let Some(seg) = results.iter().find(|s| &s.message_id == id) {
                    state.remove(&seg.file_name, seg.part);
                    // No spool cleanup needed here: a segment can only reach
                    // `still_missing` after already being confirmed posted
                    // once (`commit_result`'s `posted: true` branch), and
                    // that branch already removes its spool entry — before
                    // the check coordinator that could ever mark it missing
                    // even sees it. See `crate::spool`.
                }
            }
        }
    }

    // Single, final resume-state persistence decision, replacing the old
    // per-segment write in `commit_result`. Mirrors the completeness check
    // `run_single_upload` uses to decide whether the NZB itself gets
    // written: a run cancellation makes `still_missing` meaningless (the
    // check simply didn't get to finish verifying everything, not a
    // confirmed gap), and `allow_incomplete_nzb` means the user has already
    // accepted the remaining gap and is relying on PAR2, not `--resume`, to
    // fill it. Whenever the run is *not* complete by those terms, persist
    // once so a later `--resume` has something to load; otherwise delete
    // any state file (freshly written or left over from an earlier failed
    // attempt at the same output path) — there is nothing left to resume.
    if let (Some(resume), Some(rp)) = (&shared.resume, &shared.resume_path) {
        let has_post_failures = !failed_tasks.is_empty();
        let has_confirmed_missing = !cancelled && !still_missing.is_empty();
        let incomplete =
            has_post_failures || (has_confirmed_missing && !config.allow_incomplete_nzb);
        if incomplete {
            let _ = resume.lock().unwrap().save(rp);
        } else {
            let _ = std::fs::remove_file(rp);
            if let Some(dir) = &shared.spool_dir {
                crate::spool::remove_all(dir);
            }
        }
    }

    shared.emit(ProgressEvent::Finished);

    let mut segments = std::mem::take(&mut *shared.results.lock().unwrap());
    // Natural (not lexicographic) by name, so the NZB lists `part2.rar` before
    // `part10.rar` — the same volume order `--file-counter` numbers by.
    segments.sort_by(|a, b| natural_cmp(&a.file_name, &b.file_name).then(a.part.cmp(&b.part)));

    // 26d/26g — network performance summary + post phase timing
    let total_retries = shared.total_retries.load(Ordering::Relaxed);
    info!(
        posted = segments.len(),
        failed = failures.len(),
        retries = total_retries,
        still_missing = still_missing.len(),
        elapsed_ms = t_post_start.elapsed().as_millis(),
        phase = "post",
        "network summary"
    );

    let all_servers: Vec<_> = config.all_servers().collect();
    let mut used_server_idxs: Vec<usize> = segments.iter().map(|s| s.server_idx).collect();
    used_server_idxs.sort_unstable();
    used_server_idxs.dedup();
    let servers_used: Vec<String> = used_server_idxs
        .into_iter()
        .filter_map(|idx| all_servers.get(idx))
        .map(|s| s.host.clone())
        .collect();

    Ok(PostOutcome {
        segments,
        failures,
        failed_tasks,
        cancelled,
        groups: shared.post_group.clone(),
        still_missing,
        servers: servers_used,
        failure_reason,
        par2_temp_dir: par2_temp_dir(config.par2_temp_dir.as_deref(), shared.run_id),
    })
}

/// Per-run temp directory holding the intermediate PAR2 files written during
/// a normal posting run. Keyed by `run_id` (unique per [`PostOutcome`]), not
/// just the process ID: `--each`/`--season` with `--jobs > 1` run several
/// posting tasks concurrently *in the same process*, and a PID-only path
/// used to collide them all into one directory — one entry finishing would
/// delete PAR2 source files a sibling entry was still reading to repost
/// (see GitHub issue #67). Callers should remove
/// `par2_temp_dir(outcome.run_id)` (when `!config.par2_only`) once the
/// *entire* run is done — including any `--check` repost pass or end-of-run
/// failed-task retry — not right after the main post loop finishes, since
/// both of those may still need to re-read a PAR2 file's bytes from disk.
///
/// `base` overrides the parent directory the per-run subdirectory is created
/// under (see `Config::par2_temp_dir`). `None` falls back to
/// `std::env::temp_dir()`, which may sit on a different filesystem — with
/// less free space or a stricter quota — than the destination disk.
pub fn par2_temp_dir(base: Option<&Path>, run_id: u64) -> PathBuf {
    let base = base
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("parmesan_{}_{run_id}", std::process::id()))
}

/// Restrict the global Rayon pool to physical cores. The PAR2 encoder is pure
/// SIMD/ALU work; sibling hyperthreads contend for the same execution ports
/// and add almost nothing, so one worker per logical CPU only heats the
/// machine. Called once; a no-op if a global pool already exists.
fn configure_rayon(threads: usize) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let n = if threads > 0 {
            threads
        } else {
            parmesan::performance_core_count()
        };
        // Thread *count* stays at physical cores — PAR2 is the genuinely
        // CPU-bound stage and wants them. Only the per-thread stack shrinks,
        // from Rust's 2 MiB default: on a 128-core host that is ~130 MiB of
        // address space reclaimed for the PAR2 budget itself, at no cost to
        // throughput. See `crate::memory` for the per-thread measurements.
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .stack_size(crate::memory::ThreadTuning::detect().thread_stack_size)
            .build_global();
    });
}

/// Pad the accumulated real bytes to the full PAR2 slice size and forward
/// the slice to the background [`Par2Worker`]. Leaves `accum` empty (or
/// containing the leftover bytes if a split occurred).
fn feed_par2_slice(
    accum: &mut Vec<u8>,
    par2_slice_size: usize,
    worker: &Par2Worker,
    is_last_of_file: bool,
) -> anyhow::Result<()> {
    if accum.len() == par2_slice_size {
        // Zero-copy optimization for the common case (slice size matches accumulation).
        let next = worker
            .try_take_buffer(par2_slice_size)
            .context("allocating PAR2 slice buffer")?;
        let padded = std::mem::replace(accum, next);
        tokio::task::block_in_place(|| worker.send_slice(padded, par2_slice_size, is_last_of_file));
    } else if accum.len() > par2_slice_size {
        // Splitting case (manual slice size < article size): take exactly one slice.
        let mut slice_buf = worker
            .try_take_buffer(par2_slice_size)
            .context("allocating PAR2 slice buffer")?;
        slice_buf.extend_from_slice(&accum[..par2_slice_size]);
        accum.drain(..par2_slice_size);
        tokio::task::block_in_place(|| {
            worker.send_slice(slice_buf, par2_slice_size, is_last_of_file)
        });
    } else {
        // Final slice of a file: pad with zeros.
        let actual_len = accum.len();
        let mut padded = std::mem::take(accum);
        padded.resize(par2_slice_size, 0);
        tokio::task::block_in_place(|| worker.send_slice(padded, actual_len, is_last_of_file));
    }
    Ok(())
}

/// Base name for the PAR2 set's on-disk files. A published name may be a
/// relative path (`season01/ep01.mkv`); the PAR2 index and volume files live
/// at a single level, so they take the top-level component (the root folder,
/// or the file's own name for a single-file upload) as their base.
/// `--par2-only` fast read path. Reads source files in `par2_slice_size`
/// chunks and feeds them directly to the encoder, bypassing the article-sized
/// channel pipeline that exists for the posting path. Each file is treated
/// independently (slice boundaries reset at every file boundary), matching the
/// behaviour of the standard path.
///
/// Emits `SegmentDone` events in `article_size` increments so the progress
/// bar advances at the same cadence as the standard path — but only for a
/// genuine `--par2-only` run (`shared.config.par2_only`), where this is the
/// *only* source of data-file progress since nothing is ever posted. This
/// same `tx_opt: None` path is also used by `--par2-before-upload`'s
/// generation-only pre-pass (`producer(.., None, .., 0)` in
/// `post_files_with_progress_and_cancel`), where the data files *do* get
/// posted for real afterward (`post_pregenerated_release`) — faking their
/// progress here too would double-count every data segment once the real
/// `SegmentDone` events arrive later.
async fn par2_only_ingest(
    metas: &[Arc<FileMeta>],
    worker: &Par2Worker,
    par2_slice_size: usize,
    article_size: usize,
    total_slices: usize,
    par2_slices_fed: &mut usize,
    shared: &Arc<Shared>,
) -> Result<()> {
    let files: Vec<Par2InputFile> = metas
        .iter()
        .map(|m| Par2InputFile {
            path: m.path.clone(),
            display_name: m.real_name.clone(),
            size: m.size,
        })
        .collect();

    ingest_files_with(
        &files,
        worker,
        par2_slice_size,
        Some(&shared.cancelled),
        |file| {
            *par2_slices_fed += (file.size as usize).div_ceil(par2_slice_size);
            shared.emit(ProgressEvent::Par2InputProgress {
                done: *par2_slices_fed,
                total: total_slices,
            });
            if shared.config.par2_only {
                let mut credited = 0usize;
                let size = file.size as usize;
                while credited + article_size <= size {
                    shared.emit(ProgressEvent::SegmentDone {
                        file: file.display_name.clone(),
                        bytes: article_size as u64,
                        ok: true,
                    });
                    credited += article_size;
                }
                let leftover = size - credited;
                if leftover > 0 {
                    shared.emit(ProgressEvent::SegmentDone {
                        file: file.display_name.clone(),
                        bytes: leftover as u64,
                        ok: true,
                    });
                }
            }
            Ok(())
        },
    )
    .await
}

fn par2_base(name: &str) -> &str {
    name.split('/').next().unwrap_or(name)
}

/// Base name for the PAR2 index/volumes: [`par2_base`], but first strips a
/// `--compress-volume-size` volume suffix (`.partNN.rar`, `.NNN` after
/// `.7z`/`.zip`) if present.
///
/// Without this, a volume-split archive's PAR2 set borrowed whichever file
/// happened to be `metas[0]` verbatim — e.g. `archive.part04.rar.par2` — even
/// though the recovery set actually covers every volume together. That's
/// misleading (it reads as if only `part04` were protected) and, for
/// `--obfuscate none`/`full` where the real name *is* the wire name, it also
/// put a single volume's name on every PAR2 article's Subject instead of a
/// name shared by the whole release.
fn par2_release_base(name: &str) -> &str {
    let trimmed = match crate::compress::volume_suffix(name) {
        Some(suffix) => &name[..name.len() - suffix.len()],
        None => name,
    };
    par2_base(trimmed)
}

/// Strip the first path component (the release/top-level directory name).
///
/// The first component of a directory upload's `real_name` is the release
/// folder itself (e.g. `"Season01"` in `"Season01/ep01.mkv"`). Download
/// clients create a folder for the release, so only the path *within* that
/// folder is meaningful for yEnc `name=` and PAR2 file descriptions. Matching
/// both lets `par2 repair` find files when run from the release download dir.
///
/// `"Season01/ep01.mkv"` → `"ep01.mkv"`
/// `"Release/VIDEO_TS/file.vob"` → `"VIDEO_TS/file.vob"`
/// `"movie.mkv"` → `"movie.mkv"` (no slash → unchanged)
fn wire_name(name: &str) -> &str {
    match name.find('/') {
        Some(pos) => &name[pos + 1..],
        None => name,
    }
}

/// MD5 of a file's first 16 KiB — the PAR2 "16k hash" half of a File ID.
/// Read in a tiny pre-pass so files can be ordered before the encode pass.
async fn file_md5_16k(path: &std::path::Path, size: u64) -> Result<[u8; 16]> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("opening `{}`", path.display()))?;
    let take = size.min(16 * 1024) as usize;
    let mut buf = vec![0u8; take];
    file.read_exact(&mut buf)
        .await
        .with_context(|| format!("reading `{}`", path.display()))?;
    let mut hasher = FileHasher::new();
    hasher.update(&buf);
    Ok(hasher.finish().md5_16k)
}

/// Directory where `--par2-only` writes the recovery set.
///
/// File Description packets store each file's *relative* name, so `par2` must
/// be run from the directory that contains the root folder. The published
/// name has one path component per directory level; stripping that many
/// components off the filesystem path lands exactly there. A loose file
/// (single component) yields its parent directory, as before.
fn par2_output_dir(meta: &FileMeta) -> PathBuf {
    let depth = meta.real_name.split('/').count();
    meta.path
        .ancestors()
        .nth(depth)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// This process's own virtual address-space ceiling — see
/// [`crate::memory::address_space_limit`], which owns the implementation now
/// that startup tuning needs it too.
fn address_space_limit() -> Option<u64> {
    crate::memory::address_space_limit()
}

/// Rough reservation for the process overhead that `par2_memory_limit`
/// doesn't account for — per-connection TLS/article buffers, the tokio
/// runtime, the check-connection pool — so the PAR2 pass doesn't get sized
/// right up to the address-space ceiling and starve everything else that
/// also has to fit inside it.
fn connection_overhead_reserve(connections: usize, threads: usize) -> u64 {
    const PER_CONNECTION: u64 = 8 * 1024 * 1024; // generous: TLS + article buffers
                                                 // Was 32 MiB, chosen before anything measured per-thread cost. A live musl
                                                 // run on a 128-core seedbox reserved 2.0 GiB under that figure (64 threads)
                                                 // for stacks whose real size, after `crate::memory` bounded them, is 1 MiB
                                                 // — a ~32x over-estimate, and 69% of the entire reserve. 4 MiB keeps 3 MiB
                                                 // of headroom per thread for SIMD/GF16 scratch on top of the measured
                                                 // stack. Over-reserving is not free: the budget below is derived from what
                                                 // this leaves, so every wasted GiB here costs PAR2 budget and can force an
                                                 // extra pass — and each pass is another full read of the input.
    const PER_THREAD: u64 = 4 * 1024 * 1024; // stack (measured 1 MiB) + scratch
                                             // Was 512 MiB. Raised after a completed 83.4 GiB / 116 619-segment musl run
                                             // showed the process's true peak is *not* in the PAR2 passes: those topped
                                             // out at 7.38 GiB, then the tail of the run (final posting, the accumulated
                                             // `results` vector, the check queue's heap, NZB assembly) added another
                                             // 0.75 GiB to reach 8.13 GiB. None of that is PAR2 budget, so it belongs
                                             // here — sizing the budget as if the passes were the high-water mark
                                             // understates the real ceiling pressure by exactly that much.
                                             //
                                             // This term scales with segment count in reality; a flat 1 GiB covers the
                                             // ~116 k-segment case measured. Phase 1's sampler should replace it with a
                                             // real per-segment figure (see docs/memory-management.md).
    const BASELINE: u64 = 1024 * 1024 * 1024; // runtime, results/NZB/check tail
    BASELINE + (connections as u64) * PER_CONNECTION + (threads as u64) * PER_THREAD
}

/// Share of the address-space ceiling the whole process is allowed to reach.
///
/// `RLIMIT_AS` is a hard, zero-tolerance wall — one allocation across it aborts
/// via `handle_alloc_error`, and with `panic = "abort"` nothing unwinds far
/// enough to log why. The margin is deliberately wide.
const CEILING_TARGET: f64 = 0.85;

/// A pass's real working set as a multiple of its memory budget.
///
/// The budget sizes the recovery buffers; on top of those the encoder gets a
/// flush queue of `memory_limit / 4` (see the `queue_limit` computation in
/// `producer`), so a pass actually occupies ~1.25x what it is budgeted.
const PASS_WORKING_SET_FACTOR: f64 = 1.25;

/// Share of a finished pass's working set still held by the allocator when the
/// next pass allocates.
///
/// The pass loop drops each `Par2Worker` before creating the next, so this is
/// not a leak — it is musl's allocator not returning freed spans to the OS, and
/// the next pass's differently-shaped allocations not fitting the holes left
/// behind. `RLIMIT_AS` counts the retained mapping regardless.
///
/// Measured on a live 75.8 GiB / 3-pass musl run: VmPeak went 4.98 GiB during
/// pass 1 to 7.15 GiB during pass 2, a 2.17 GiB step against a 4.15 GiB pass
/// working set — 52% retained. Rounded up to 0.55.
///
/// This is the term the old formula omitted entirely, and omitting it is why
/// the over-sized `PER_THREAD` above was load-bearing: two errors cancelled.
/// Correcting only one of them would have raised the budget to ~4.2 GiB/pass
/// and pushed predicted peak use to ~92% of the ceiling.
const CROSS_PASS_RETENTION: f64 = 0.55;

/// Safe per-pass PAR2 budget, derived from this process's own `RLIMIT_AS`
/// rather than host/cgroup RAM (see [`address_space_limit`]).
///
/// The model is:
///
/// ```text
/// peak ≈ reserve + budget × PASS_WORKING_SET_FACTOR × (1 + retention)
/// ```
///
/// where `retention` is [`CROSS_PASS_RETENTION`] for a multi-pass run and zero
/// for a single-pass one — a run that never starts a second pass cannot be
/// holding a first pass's memory. Solving for `budget` under
/// `peak ≤ ceiling × CEILING_TARGET` gives the two branches below.
///
/// Splitting the single-pass case out matters: it is both the common case and
/// the one the old flat 50% penalised hardest. On a 9.5 GiB ceiling the budget
/// for a single-pass run goes from 3.3 GiB to ~5.6 GiB, which is itself the
/// cheapest way to *avoid* multi-pass runs — and every pass avoided is one
/// less full read of the input.
///
/// `recovery_count == 0` (no PAR2 requested) takes the single-pass branch; the
/// budget is unused in that case but must still be a sane number.
fn address_space_budget(reserve: u64, slice_size: usize, recovery_count: usize) -> Option<u64> {
    let as_limit = address_space_limit()?;
    let headroom = (as_limit as f64 * CEILING_TARGET) - reserve as f64;
    if headroom <= 0.0 {
        // The reserve alone already exceeds the target. Return 0 and let the
        // caller surface it — silently handing back a tiny budget here would
        // produce thousands of passes instead of an actionable error.
        return Some(0);
    }

    let single_pass = headroom / PASS_WORKING_SET_FACTOR;
    let fits_in_one_pass = slice_size == 0
        || recovery_count == 0
        || (single_pass as u64) / (slice_size as u64) >= recovery_count as u64;
    if fits_in_one_pass {
        return Some(single_pass as u64);
    }

    Some((headroom / (PASS_WORKING_SET_FACTOR * (1.0 + CROSS_PASS_RETENTION))) as u64)
}

/// Shared PAR2 memory budget + pass list used by the per-file producer and
/// the season path so `--memory-limit` cannot drift between them.
fn par2_memory_plan(
    config: &Config,
    par2_slice_size: usize,
    recovery_count: usize,
    active_connections: usize,
) -> Result<(usize, Vec<(u32, usize)>)> {
    let reserve_threads = if config.threads > 0 {
        config.threads
    } else {
        parmesan::performance_core_count()
    };
    let overhead_reserve = connection_overhead_reserve(active_connections, reserve_threads);
    let as_budget = address_space_budget(overhead_reserve, par2_slice_size, recovery_count);
    if as_budget == Some(0) && recovery_count > 0 {
        anyhow::bail!(
            "not enough address space to generate PAR2: this session's limit \
             (RLIMIT_AS = {}) is already exceeded by the ~{} reserved for {} \
             connections and {} PAR2 threads. Lower --connections/--threads, \
             disable PAR2 with --par2 0, or raise `ulimit -v` for this session.",
            crate::progress::format_size(address_space_limit().unwrap_or_default()),
            crate::progress::format_size(overhead_reserve),
            active_connections,
            reserve_threads,
        );
    }
    let ceiling = crate::memory::Ceiling::discover(config.memory_limit);
    let non_as_par2_share = crate::memory::budget::share_of(
        ceiling.effective_excluding_address_space(),
        crate::memory::budget::Stage::Par2,
    );
    let binding_budget = as_budget.map_or(non_as_par2_share, |b| b.min(non_as_par2_share));

    let memory_limit = match config.par2_memory_limit {
        Some(limit) => {
            if limit as u64 > binding_budget {
                let bound_by_as = as_budget.is_some_and(|b| b <= non_as_par2_share);
                anyhow::bail!(
                    "--par2-memory-limit {} won't fit safely: {} leaves a safe budget of \
                     only {} once ~{} is reserved for {} connections and {} PAR2 threads. \
                     Lower --par2-memory-limit (or --memory-limit / --connections/--threads), \
                     or {}.",
                    crate::progress::format_size(limit as u64),
                    if bound_by_as {
                        format!(
                            "this session's address-space limit (RLIMIT_AS = {})",
                            crate::progress::format_size(address_space_limit().unwrap_or_default())
                        )
                    } else {
                        format!(
                            "the global --memory-limit budget (effective ceiling {})",
                            crate::progress::format_size(ceiling.effective)
                        )
                    },
                    crate::progress::format_size(binding_budget),
                    crate::progress::format_size(overhead_reserve),
                    active_connections,
                    reserve_threads,
                    if bound_by_as {
                        "raise `ulimit -v` for this session"
                    } else {
                        "raise --memory-limit"
                    },
                );
            }
            limit
        }
        None => (binding_budget as usize).max(256 * 1024 * 1024),
    };

    let slices_per_pass = (memory_limit / par2_slice_size.max(1)).max(1);
    let mut passes = Vec::new();
    if recovery_count > 0 {
        let mut start = 0;
        while start < recovery_count {
            let count = (recovery_count - start).min(slices_per_pass);
            passes.push((start as u32, count));
            start += count;
        }
    } else {
        passes.push((0, 0));
    }
    Ok((memory_limit, passes))
}

async fn producer(
    metas: Vec<Arc<FileMeta>>,
    tx_opt: Option<TaskDispatcher<PostTask>>,
    shared: Arc<Shared>,
    // Connections actually competing for RAM *right now*, used to size the
    // PAR2 memory budget (see `connection_overhead_reserve`) — normally
    // `shared.config.total_connections()`, but the caller passes `0` for a
    // `--par2-before-upload` generation-only call (`tx_opt: None`), since no
    // connection pool exists yet at that point (see
    // `post_files_with_progress_and_cancel`): reserving RAM for connections
    // that aren't open yet would just force more read passes than necessary.
    active_connections: usize,
) -> Result<()> {
    let article_size = shared.config.article_size;

    // Article count per file — one article is one posted segment.
    // Empty files (size == 0) contribute zero PAR2 input slices per spec;
    // `yenc::segments(0, ..)` returns 1 to produce one (empty) article, but
    // that must not be counted as a PAR2 input block.
    let mut per_file_articles = Vec::with_capacity(metas.len());
    for meta in &metas {
        per_file_articles.push(if meta.size == 0 {
            0
        } else {
            yenc::segments(meta.size, article_size).len()
        });
    }

    // Same geometry `par2_geometry` already computed to seed the progress totals
    // at `Started` — file-size heuristic via `parmesan::ops::calculate_geometry`.
    let (par2_slice_size, total_slices, recovery_count) = par2_geometry(&metas, &shared.config);

    // Validate PAR2 spec limits.
    if total_slices > 32768 {
        anyhow::bail!("too many input slices: {total_slices} (max 32768). Increase --slice-size or decrease --slice-count.");
    }
    if recovery_count > 65535 {
        anyhow::bail!("too many recovery blocks: {recovery_count} (max 65535). Increase --slice-size or decrease --par2/--recovery-count.");
    }

    info!(
        input_slices = total_slices,
        recovery_blocks = recovery_count,
        slice_size = par2_slice_size,
        "PAR2 geometry"
    );

    // Auto-detect safe RAM limit if not specified (70% of available RAM).
    // `available_memory()` reports the host's RAM and ignores cgroup/container
    // limits, so on a memory-limited container it can report far more than is
    // actually usable, letting the computed limit blow past the real ceiling
    // and OOM. Take the tighter of the host figure and the cgroup's free
    // memory (when the process is confined by one) instead.
    //
    // Neither of those sees a per-session `RLIMIT_AS` (`ulimit -v`), which
    // shared seedboxes commonly cap far below host RAM regardless of cgroup
    // (PAM `limits.conf`, applied to every login session, not a container).
    // Blowing past it aborts via `handle_alloc_error` — with `panic = "abort"`
    // in the release profile nothing unwinds long enough to flush a log line,
    // so it looks like the process just vanishes mid-upload.
    let reserve_threads = if shared.config.threads > 0 {
        shared.config.threads
    } else {
        parmesan::performance_core_count()
    };
    let overhead_reserve = connection_overhead_reserve(active_connections, reserve_threads);
    let ceiling = crate::memory::Ceiling::discover(shared.config.memory_limit);
    let (memory_limit, passes) = par2_memory_plan(
        &shared.config,
        par2_slice_size,
        recovery_count,
        active_connections,
    )?;

    if recovery_count > 0 {
        // A single combined status line (not gated on -v): the numbers
        // behind the PAR2 memory budget used to be invisible, which is
        // exactly why a process could vanish mid-upload
        // (`handle_alloc_error`, no unwind long enough to flush a log line)
        // with nothing in the terminal pointing at memory as the cause.
        // Deliberately one `Status` emission, not two — a separate banner
        // plus this pass-count line raced against each other (the renderer
        // only keeps the most recent status), so whichever lost never made
        // it to the terminal.
        let ceiling_text = match address_space_limit() {
            Some(limit) => crate::progress::format_size(limit),
            None => "none detected".to_string(),
        };
        let passes_suffix = if passes.len() > 1 {
            format!(" | split into {} passes", passes.len())
        } else {
            String::new()
        };
        // Only named when the user actually set a global budget — in the
        // "auto" case this would just restate host-RAM-derived numbers
        // nobody asked about, adding noise rather than clarity.
        let global_suffix = shared
            .config
            .memory_limit
            .map(|_| {
                format!(
                    " | global --memory-limit ceiling {}",
                    crate::progress::format_size(ceiling.effective)
                )
            })
            .unwrap_or_default();
        shared.emit(crate::progress::ProgressEvent::Status {
            text: format!(
                "memory: address-space limit {} | reserved for overhead \
                 (connections+threads+runtime) {} | PAR2 budget {}/pass{}{}",
                ceiling_text,
                crate::progress::format_size(overhead_reserve),
                crate::progress::format_size(memory_limit as u64),
                passes_suffix,
                global_suffix,
            ),
        });
    }

    let mut all_checksums: Vec<Vec<SliceChecksum>> = vec![Vec::new(); metas.len()];

    if recovery_count > 0 {
        let simd_method = if shared.config.simd != parmesan::SimdPath::Auto {
            shared.config.simd.to_string()
        } else {
            parmesan::detect_simd().to_string()
        };
        let effective_threads = if shared.config.threads > 0 {
            shared.config.threads
        } else {
            parmesan::performance_core_count()
        };
        info!(
            simd = simd_method,
            threads = effective_threads,
            passes = passes.len(),
            "RS encoder"
        );

        let chunk_size_bytes = 16384usize * 2; // 16384 u16 words × 2 bytes = 32 KiB
        crate::memory::set_phase(crate::memory::Phase::Par2);
        shared.emit(crate::progress::ProgressEvent::Par2EncodeStarted {
            input_bytes: metas.iter().map(|m| m.size).sum(),
            input_slices: total_slices,
            input_files: metas.len(),
            recovery_slices: recovery_count,
            slice_size: par2_slice_size,
            passes: passes.len(),
            chunk_size: chunk_size_bytes,
            simd_method: simd_method.to_string(),
            threads: parmesan::performance_core_count(),
            memory_limit,
        });
        shared.emit(crate::progress::ProgressEvent::Par2WriteStarted {
            total: recovery_count as u32,
        });
    }

    let mut par2_dir = None;
    let mut base_packets = Vec::new();
    let mut rsid = [0u8; 16];

    for (pass_idx, (exp_start, rec_count)) in passes.iter().copied().enumerate() {
        let worker_opt: Option<Par2Worker> = if rec_count > 0 {
            let enc =
                RecoveryEncoder::try_new_smart(par2_slice_size, total_slices, exp_start, rec_count)
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "not enough memory to allocate PAR2 recovery buffers for pass {} \
                     ({} recovery blocks × {} bytes each): {}. Lower --memory-limit or \
                     --par2-memory-limit, or increase available memory.",
                            pass_idx,
                            rec_count,
                            par2_slice_size,
                            e
                        )
                    })?;
            // On passes with many recovery blocks, increasing the queue size
            // (cache blocking) amortizes the flush cost over more input data.
            // We use 1/4 of the available memory limit for the queue, capped
            // between 256MB and 2GB.
            let queue_limit = (memory_limit / 4).clamp(256 * 1024 * 1024, 2 * 1024 * 1024 * 1024);
            let enc = enc
                .with_flush_limit(queue_limit)
                .with_simd_path(shared.config.simd);

            // On pass 0 enable parallel checksum computation inside the encoder
            // so rayon::join overlaps MD5+CRC32 with RS work.
            let enc = if pass_idx == 0 {
                enc.with_checksums()
            } else {
                enc
            };
            Some(Par2Worker::spawn(
                enc,
                pass_idx == 0,
                parmesan::worker::DEFAULT_CHANNEL_DEPTH,
            ))
        } else {
            None
        };

        let mut par2_slices_fed: usize = 0;

        // Fast path for `--par2-only`: read directly in slice-sized chunks,
        // skipping the article-channel pipeline that exists for posting.
        // Only used when there is recovery work to do (worker is Some).
        if tx_opt.is_none() {
            if let Some(worker) = &worker_opt {
                par2_only_ingest(
                    &metas,
                    worker,
                    par2_slice_size,
                    article_size,
                    total_slices,
                    &mut par2_slices_fed,
                    &shared,
                )
                .await?;
            }
        } else {
            for meta in metas.iter() {
                let segments: Vec<(u64, usize)> = yenc::segments(meta.size, article_size);
                let total_parts = segments.len() as u32;

                const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
                let mut file_buf = None;
                let mut read_rx = None;
                let mut reader_handle = None;

                if meta.size <= CHUNK_SIZE {
                    file_buf = Some(
                        tokio::fs::read(&meta.path)
                            .await
                            .with_context(|| format!("reading `{}`", meta.path.display()))?,
                    );
                } else {
                    let (rx, handle) =
                        spawn_double_buffered_reader(meta.path.clone(), segments.clone(), &shared);
                    read_rx = Some(rx);
                    reader_handle = Some(handle);
                }

                let mut crc = yenc::Crc32::new();
                let last_idx = segments.len().saturating_sub(1);

                // Real bytes of the PAR2 input slice currently being assembled.
                // Source the buffer from the worker's recycled-buffer pool so
                // subsequent files reuse allocations from earlier flushes.
                let mut par2_accum: Vec<u8> = match worker_opt.as_ref() {
                    Some(w) => w
                        .try_take_buffer(par2_slice_size)
                        .context("allocating PAR2 slice buffer")?,
                    None => Vec::new(),
                };

                let mut i: u32 = 0;
                for (idx, &(offset, len)) in segments.iter().enumerate() {
                    if shared.cancelled.load(Ordering::Relaxed) {
                        if let Some(handle) = reader_handle {
                            let _ = handle.await;
                        }
                        return Ok(());
                    }

                    let (buf, file_crc32) = if let Some(fb) = &file_buf {
                        let mut buf = shared
                            .try_acquire_buffer(len)
                            .context("allocating article buffer")?;
                        let start = offset as usize;
                        buf.copy_from_slice(&fb[start..start + len]);
                        crc.update(&buf);
                        let full_crc32 = (idx == last_idx).then(|| crc.finalize());
                        (buf, full_crc32)
                    } else {
                        match read_rx.as_mut().unwrap().recv().await {
                            Some((_, buf, file_crc32)) => (buf, file_crc32),
                            None => break,
                        }
                    };

                    // PAR2 work is gated on the worker being active.
                    if let Some(worker) = &worker_opt {
                        // Append the article to the current PAR2 slice.
                        par2_accum.extend_from_slice(&buf);
                        // Strictly `>`, not `>=`: draining to exactly 0 here would
                        // send the file's true last slice with `is_last_of_file:
                        // false` whenever the file size is an exact multiple of
                        // `par2_slice_size`, since the trailing flush below (the
                        // only call site that passes `true`) is skipped once
                        // `par2_accum` is empty. That left the worker's hasher
                        // (crates/parmesan/src/worker.rs) never finalized for that
                        // file — silently folding its bytes into the next file's
                        // hash, or panicking on `hashes.len()` mismatch if it was
                        // the last file in the set. Keeping at least one byte
                        // buffered here always routes the file's final slice
                        // through the trailing flush instead.
                        while par2_accum.len() > par2_slice_size {
                            feed_par2_slice(&mut par2_accum, par2_slice_size, worker, false)?;
                            par2_slices_fed += 1;
                            shared.emit(crate::progress::ProgressEvent::Par2InputProgress {
                                done: par2_slices_fed,
                                total: total_slices,
                            });
                        }
                    }

                    i += 1;
                    if pass_idx == 0 {
                        if let Some(tx) = &tx_opt {
                            // Send buf to the worker; the worker will return it to
                            // the pool (Phase 12b) after encoding the article.
                            if tx
                                .send(make_task(
                                    meta.clone(),
                                    i,
                                    total_parts,
                                    offset,
                                    buf,
                                    file_crc32,
                                    &shared.config,
                                ))
                                .await
                                .is_err()
                            {
                                if let Some(handle) = reader_handle {
                                    let _ = handle.await;
                                }
                                return Ok(()); // channel closed
                            }
                        } else {
                            // No posting pool (`--par2-only`): report progress
                            // and return the buffer to the pool immediately.
                            let bytes = buf.len() as u64;
                            shared.release_buffer(buf);
                            shared.emit(ProgressEvent::SegmentDone {
                                file: meta.real_name.clone(),
                                bytes,
                                ok: true,
                            });
                        }
                    } else {
                        // Subsequent pass: buffer no longer needed; return to pool.
                        shared.release_buffer(buf);
                    }
                }

                if let Some(handle) = reader_handle {
                    let _ = handle.await?;
                }

                // Flush the file's final, partial PAR2 slice (zero-padded).
                if let Some(worker) = &worker_opt {
                    if !par2_accum.is_empty() {
                        feed_par2_slice(&mut par2_accum, par2_slice_size, worker, true)?;
                        par2_slices_fed += 1;
                        shared.emit(crate::progress::ProgressEvent::Par2InputProgress {
                            done: par2_slices_fed,
                            total: total_slices,
                        });
                    }
                }
            }
        } // end else (standard posting path)

        if let Some(worker) = worker_opt {
            shared.emit(ProgressEvent::Status {
                text: "computing PAR2 recovery data".to_string(),
            });
            let t_par2_compute = std::time::Instant::now();
            // finish() closes the slice channel and waits for the worker thread
            // to drain any remaining slices and run the final flush.
            let (recovery_slices, slice_checksums, hashes) =
                tokio::task::block_in_place(|| worker.finish());
            let par2_compute_ms = t_par2_compute.elapsed().as_millis();
            info!(
                elapsed_ms = par2_compute_ms,
                phase = "par2_compute",
                "phase done"
            );
            shared.emit(ProgressEvent::Status {
                text: String::new(),
            });

            if pass_idx == 0 {
                // Distribute per-slice checksums back to per-file buckets.
                // Slice count is `ceil(file_size / slice_size)`, not an article
                // grouping: when the slice is smaller than one article (the
                // many-small case) `slice_size / article_size` is zero.
                let mut cs_iter = slice_checksums.into_iter();
                for (file_idx, meta) in metas.iter().enumerate() {
                    let file_slices = if meta.size == 0 {
                        0
                    } else {
                        (meta.size as usize).div_ceil(par2_slice_size)
                    };
                    all_checksums[file_idx] = cs_iter.by_ref().take(file_slices).collect();
                }

                // Hashes were computed during the first read pass to avoid
                // redundant I/O.  Empty files are never fed to the worker
                // (the hasher requires at least one slice to finalize), so
                // `hashes` may have fewer entries than `metas`. Reconstruct
                // the per-file hash sequence by inserting known-empty entries
                // at positions where meta.size == 0.
                let md5_empty: [u8; 16] = parmesan::packet::md5(b"");
                let mut file_ids = Vec::new();
                let mut final_hashes = Vec::new();
                let mut worker_hash_iter = hashes.into_iter();

                for meta in &metas {
                    let fh = if meta.size == 0 {
                        FileHashes {
                            md5_full: md5_empty,
                            md5_16k: md5_empty,
                            length: 0,
                        }
                    } else {
                        worker_hash_iter
                            .next()
                            .expect("worker returned fewer hashes than non-empty files")
                    };
                    // PAR2 file descriptions use the path relative to the
                    // release root (first component stripped). Download clients
                    // create the release folder; `par2 repair` run from inside
                    // it must find files without an extra path prefix.
                    let fid =
                        packet::compute_file_id(&fh.md5_16k, fh.length, wire_name(&meta.real_name));
                    file_ids.push(fid);
                    final_hashes.push(fh);
                }

                let main_b = packet::main_body(par2_slice_size as u64, &file_ids);
                rsid = packet::recovery_set_id(&main_b);
                let pkt_main = packet::serialize_packet(&rsid, &packet::TYPE_MAIN, &main_b);
                let pkt_creator = packet::serialize_packet(
                    &rsid,
                    &packet::TYPE_CREATOR,
                    &packet::creator_body("pesto"),
                );

                base_packets.extend(pkt_main);
                base_packets.extend(pkt_creator);

                for (idx, fh) in final_hashes.iter().enumerate() {
                    let fid = &file_ids[idx];
                    let pkt_file_desc = packet::serialize_packet(
                        &rsid,
                        &packet::TYPE_FILE_DESC,
                        &packet::file_description_body(
                            fid,
                            &fh.md5_full,
                            &fh.md5_16k,
                            fh.length,
                            wire_name(&metas[idx].real_name),
                        ),
                    );
                    let pkt_ifsc = packet::serialize_packet(
                        &rsid,
                        &packet::TYPE_IFSC,
                        &packet::ifsc_body(fid, &all_checksums[idx]),
                    );
                    base_packets.extend(pkt_file_desc);
                    base_packets.extend(pkt_ifsc);
                }

                if shared.config.par2_only {
                    par2_dir = Some(par2_output_dir(&metas[0]));
                } else {
                    par2_dir = Some(par2_temp_dir(
                        shared.config.par2_temp_dir.as_deref(),
                        shared.run_id,
                    ));
                    tokio::fs::create_dir_all(par2_dir.as_ref().unwrap()).await?;
                }

                let index_name = layout::index_name(par2_release_base(&metas[0].real_name));
                let index_path = par2_dir.as_ref().unwrap().join(&index_name);
                tokio::fs::write(&index_path, &base_packets).await?;
                if let Some(tx) = &tx_opt {
                    // In `FullShared` mode, root the wire name at the release
                    // prefix instead of the (possibly real) on-disk base name,
                    // so this index file groups with the rest of the release.
                    let wire_override = shared.release_prefix.as_deref().map(layout::index_name);
                    // The index file is the release's first file after the
                    // data files — see `Shared::total_files`'s doc comment.
                    let file_index = metas.len() as u32 + 1;
                    push_par2_file(
                        &index_path,
                        index_name,
                        wire_override,
                        file_index,
                        &shared,
                        tx,
                    )
                    .await?;
                }
            }

            let t_par2_write = std::time::Instant::now();
            let volumes = layout::plan_volumes(recovery_count as u32);
            for slice in recovery_slices {
                let (vol_idx, vol) = volumes
                    .iter()
                    .enumerate()
                    .find(|(_, v)| slice.exponent >= v.first && slice.exponent < v.first + v.count)
                    .unwrap();
                let vol_name = layout::volume_name(par2_release_base(&metas[0].real_name), *vol);
                let vol_path = par2_dir.as_ref().unwrap().join(&vol_name);

                let mut file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(&vol_path)
                    .await?;

                if slice.exponent == vol.first {
                    file.write_all(&base_packets).await?;
                }

                let pkt = packet::serialize_packet(
                    &rsid,
                    &packet::TYPE_RECOVERY,
                    &packet::recovery_body(slice.exponent, &slice.data),
                );
                file.write_all(&pkt).await?;
                shared.emit(crate::progress::ProgressEvent::Par2SliceWritten);

                if slice.exponent == vol.first + vol.count - 1 {
                    if let Some(tx) = &tx_opt {
                        let wire_override = shared
                            .release_prefix
                            .as_deref()
                            .map(|prefix| layout::volume_name(prefix, *vol));
                        // Volumes follow the data files and the index file
                        // (+1), in `plan_volumes`' order (+vol_idx).
                        let file_index = metas.len() as u32 + 2 + vol_idx as u32;
                        push_par2_file(&vol_path, vol_name, wire_override, file_index, &shared, tx)
                            .await?;
                    }
                }
            }
            info!(
                elapsed_ms = t_par2_write.elapsed().as_millis(),
                phase = "par2_write",
                "phase done"
            );
        }
    }

    Ok(())
}

/// Posts a release whose PAR2 index/volumes were already fully generated by
/// an earlier `producer(metas, None, shared, 0)` call — see
/// `--par2-before-upload` in `post_files_with_progress_and_cancel`: that call
/// writes every index/volume file to `par2_dir` without posting anything
/// (`tx_opt: None` takes the `par2_only_ingest` path). This posts the data
/// files, then reads back and posts the already-written index and every
/// volume, so the whole release goes out back to back with no gap. Volume
/// file names/`file_index`es are recomputed from `recovery_count` alone
/// (via `layout::plan_volumes`), matching exactly what the generation call
/// already wrote — no I/O needed to know what's there.
async fn post_pregenerated_release(
    metas: &[Arc<FileMeta>],
    par2_dir: &Path,
    recovery_count: usize,
    tx: &TaskDispatcher<PostTask>,
    shared: &Arc<Shared>,
) -> Result<()> {
    if shared.cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    post_data_files(metas, tx, shared).await?;

    let index_name = layout::index_name(par2_release_base(&metas[0].real_name));
    let index_path = par2_dir.join(&index_name);
    let wire_override = shared.release_prefix.as_deref().map(layout::index_name);
    let file_index = metas.len() as u32 + 1;
    push_par2_file(
        &index_path,
        index_name,
        wire_override,
        file_index,
        shared,
        tx,
    )
    .await?;

    let volumes = layout::plan_volumes(recovery_count as u32);
    for (vol_idx, vol) in volumes.iter().enumerate() {
        let vol_name = layout::volume_name(par2_release_base(&metas[0].real_name), *vol);
        let vol_path = par2_dir.join(&vol_name);
        let wire_override = shared
            .release_prefix
            .as_deref()
            .map(|prefix| layout::volume_name(prefix, *vol));
        let file_index = metas.len() as u32 + 2 + vol_idx as u32;
        push_par2_file(&vol_path, vol_name, wire_override, file_index, shared, tx).await?;
    }
    Ok(())
}

/// One read article: byte offset, buffer, and (on the file's last article)
/// the whole-file CRC-32 needed for the `=yend` line.
type ReadArticle = (u64, Vec<u8>, Option<u32>);

/// Spawns the double-buffered reader task shared by the data-posting loop in
/// `producer` and `post_data_files`: reads `segments` from `path` into a
/// bounded channel of capacity 2 so the OS can fetch article N+1 while the
/// caller processes article N, accumulating the whole-file CRC-32 (needed on
/// the `=yend` line of the last segment) as it goes.
fn spawn_double_buffered_reader(
    path: PathBuf,
    segments: Vec<(u64, usize)>,
    shared: &Arc<Shared>,
) -> (
    tokio::sync::mpsc::Receiver<ReadArticle>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (read_tx, read_rx) = tokio::sync::mpsc::channel::<ReadArticle>(2);
    let reader_shared = shared.clone();
    let reader_handle = tokio::spawn(async move {
        let mut file = File::open(&path).await?;
        let mut crc = yenc::Crc32::new();
        let last_idx = segments.len().saturating_sub(1);
        for (idx, (offset, len)) in segments.into_iter().enumerate() {
            // Phase 12b: acquire a buffer from the shared pool if available,
            // otherwise allocate. Workers return buffers to the same pool
            // after yEnc encoding.
            let mut buf = reader_shared
                .try_acquire_buffer(len)
                .context("allocating article buffer")?;
            file.read_exact(&mut buf).await?;
            crc.update(&buf);
            let full_crc32 = (idx == last_idx).then(|| crc.finalize());
            if read_tx.send((offset, buf, full_crc32)).await.is_err() {
                break; // caller dropped its end (cancelled)
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    (read_rx, reader_handle)
}

/// Posts every data file's articles with no PAR2 involvement. Called from
/// `post_pregenerated_release` (`--par2-before-upload`, after PAR2
/// generation has already fully completed) to post the data files
/// immediately before the already-generated PAR2 index/volumes so the whole
/// release goes out back to back with no gap. Mirrors the data-posting half
/// of `producer`'s interleaved per-file loop, minus the PAR2 accumulation,
/// which is unnecessary here since PAR2 is already on disk.
async fn post_data_files(
    metas: &[Arc<FileMeta>],
    tx: &TaskDispatcher<PostTask>,
    shared: &Arc<Shared>,
) -> Result<()> {
    let article_size = shared.config.article_size;
    for meta in metas {
        let segments: Vec<(u64, usize)> = yenc::segments(meta.size, article_size);
        let total_parts = segments.len() as u32;
        const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
        let mut file_buf = None;
        let mut read_rx = None;
        let mut reader_handle = None;

        if meta.size <= CHUNK_SIZE {
            file_buf = Some(
                tokio::fs::read(&meta.path)
                    .await
                    .with_context(|| format!("reading `{}`", meta.path.display()))?,
            );
        } else {
            let (rx, handle) =
                spawn_double_buffered_reader(meta.path.clone(), segments.clone(), shared);
            read_rx = Some(rx);
            reader_handle = Some(handle);
        }

        let mut crc = yenc::Crc32::new();
        let last_idx = segments.len().saturating_sub(1);

        let mut i: u32 = 0;
        for (idx, &(offset, len)) in segments.iter().enumerate() {
            if shared.cancelled.load(Ordering::Relaxed) {
                if let Some(handle) = reader_handle {
                    let _ = handle.await;
                }
                return Ok(());
            }

            let (buf, file_crc32) = if let Some(fb) = &file_buf {
                let mut buf = shared
                    .try_acquire_buffer(len)
                    .context("allocating article buffer")?;
                let start = offset as usize;
                buf.copy_from_slice(&fb[start..start + len]);
                crc.update(&buf);
                let full_crc32 = (idx == last_idx).then(|| crc.finalize());
                (buf, full_crc32)
            } else {
                match read_rx.as_mut().unwrap().recv().await {
                    Some((_, buf, file_crc32)) => (buf, file_crc32),
                    None => break,
                }
            };

            i += 1;
            if tx
                .send(make_task(
                    meta.clone(),
                    i,
                    total_parts,
                    offset,
                    buf,
                    file_crc32,
                    &shared.config,
                ))
                .await
                .is_err()
            {
                if let Some(handle) = reader_handle {
                    let _ = handle.await;
                }
                return Ok(());
            }
        }
        if let Some(handle) = reader_handle {
            let _ = handle.await?;
        }
    }
    Ok(())
}

async fn push_par2_file(
    path: &PathBuf,
    real_name: String,
    wire_override: Option<String>,
    file_index: u32,
    shared: &Arc<Shared>,
    tx: &TaskDispatcher<PostTask>,
) -> Result<()> {
    let size = tokio::fs::metadata(path).await?.len();
    let segments = yenc::segments(size, shared.config.article_size);
    let total = segments.len() as u32;

    shared.emit(ProgressEvent::QueueExtended {
        file: real_name.clone(),
        segments: total as u64,
        bytes: size,
    });

    let (subject_name, yenc_name, from) = if let Some(name) = wire_override {
        // `name` carries the release's shared prefix (FullShared/Light) —
        // keep it on the subject for indexer grouping. Under `light`, the
        // yEnc body name= is that same string verbatim; under `full-shared`
        // it starts with that same prefix but adds its own random suffix
        // instead — see the main FullShared/Light branch above for why
        // (issue #106).
        let prefix = shared.release_prefix.as_deref().unwrap_or_default();
        let yenc = if shared.config.obfuscate == ObfuscateMode::Light {
            name.clone()
        } else {
            obfuscated_name_with_prefix(prefix)
        };
        (name, yenc, shared.release_from.clone().unwrap_or_default())
    } else {
        match shared.config.obfuscate {
            ObfuscateMode::None => {
                let wn = wire_name(&real_name).to_string();
                (wn.clone(), wn, shared.config.from.clone())
            }
            ObfuscateMode::Full | ObfuscateMode::Paranoid | ObfuscateMode::FullShared => {
                (obfuscated_name(), obfuscated_name(), random_from())
            }
            ObfuscateMode::Light => {
                let name = obfuscated_name();
                (name.clone(), name, random_from())
            }
        }
    };
    let date = resolve_date(shared.config.date.as_deref());

    let meta = Arc::new(FileMeta {
        path: path.clone(),
        real_name,
        subject_name,
        yenc_name,
        from,
        date,
        size,
        file_index: if shared.config.file_counter {
            file_index
        } else {
            0
        },
    });

    // Whole-file CRC-32 accumulated as this same loop reads the file for
    // upload, rather than in a separate pre-pass — see the reader task in
    // `producer` for the equivalent path used by the main input files.
    let mut crc = yenc::Crc32::new();
    let last_idx = total.saturating_sub(1);
    let mut file = tokio::fs::File::open(path).await?;
    for (i, (offset, len)) in segments.into_iter().enumerate() {
        let mut buf = shared
            .try_acquire_buffer(len)
            .context("allocating PAR2 file buffer")?;
        file.read_exact(&mut buf).await?;
        crc.update(&buf);
        let file_crc32 = (i as u32 == last_idx).then(|| crc.finalize());
        if tx
            .send(make_task(
                meta.clone(),
                i as u32 + 1,
                total,
                offset,
                buf,
                file_crc32,
                &shared.config,
            ))
            .await
            .is_err()
        {
            break;
        }
    }
    Ok(())
}

/// Per-worker token-bucket rate limiter.
struct RateLimiter {
    /// Bytes per second; 0 = unlimited.
    rate: u64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    fn new(rate: u64) -> Self {
        RateLimiter {
            rate,
            tokens: rate as f64,
            last: Instant::now(),
        }
    }

    /// Wait until `bytes` tokens are available, then consume them.
    async fn acquire(&mut self, bytes: usize) {
        if self.rate == 0 {
            return;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate as f64).min(self.rate as f64);
        self.last = now;

        let bytes_f = bytes as f64;
        if self.tokens >= bytes_f {
            self.tokens -= bytes_f;
        } else {
            let needed = bytes_f - self.tokens;
            let wait = Duration::from_secs_f64(needed / self.rate as f64);
            tokio::time::sleep(wait).await;
            self.tokens = 0.0;
            self.last = Instant::now();
        }
    }
}

async fn encode_worker(
    shared: Arc<Shared>,
    mut rx: tokio::sync::mpsc::Receiver<PostTask>,
    post_tx: Arc<TaskDispatcher<ReadyArticle>>,
) {
    while let Some(task) = rx.recv().await {
        if shared.cancelled.load(Ordering::Relaxed) {
            break;
        }
        if let Some(ready) = prepare_ready(&shared, task).await {
            if post_tx.send(ready).await.is_err() {
                break;
            }
        }
    }
}

/// Resume skip / spool / yEnc. `None` means the segment is already done.
async fn prepare_ready(shared: &Arc<Shared>, task: PostTask) -> Option<ReadyArticle> {
    if let Some(resume) = &shared.resume {
        let existing = resume
            .lock()
            .unwrap()
            .get(&task.meta.real_name, task.part)
            .cloned();
        if let Some(existing) = existing {
            shared.results.lock().unwrap().push(PostedSegment {
                file_name: task.meta.real_name.clone(),
                file_path: Arc::from(task.meta.path.as_path()),
                subject_name: Arc::from(task.meta.real_name.as_str()),
                wire_name: Arc::from(task.subject_name.as_str()),
                file_size: task.meta.size,
                part: task.part,
                total: task.total,
                message_id: existing.message_id,
                bytes: existing.bytes,
                from: Arc::from(task.from.as_str()),
                date: task.date.clone(),
                full_crc32: task.file_crc32.unwrap_or(0),
                server_idx: 0,
                file_index: task.meta.file_index,
                total_files: shared.total_files,
            });
            let raw_bytes = task.data.len() as u64;
            shared.release_buffer(task.data);
            shared.emit(ProgressEvent::SegmentDone {
                file: task.meta.real_name.clone(),
                bytes: raw_bytes,
                ok: true,
            });
            return None;
        }
    }

    let spooled = shared
        .spool_dir
        .as_ref()
        .and_then(|dir| crate::spool::read(dir, &task.meta.real_name, task.part));

    let (message_id, headers, encoded, encode_time) = if let Some(spooled) = spooled {
        let encoded = yenc::EncodedPart {
            number: task.part,
            total: task.total,
            begin: 0,
            end: 0,
            crc32: 0,
            body: spooled.body,
        };
        (spooled.message_id, spooled.headers, encoded, Duration::ZERO)
    } else {
        let t_enc = Instant::now();
        let file_crc32 = task.file_crc32;
        let mut encode_buf = shared.acquire_encode_buf();
        let encoded = yenc::encode_part_into(
            &task.meta.yenc_name,
            task.meta.size,
            yenc::PartSpec {
                number: task.part,
                total: task.total,
                offset: task.offset,
            },
            &task.data,
            shared.config.line_length,
            file_crc32,
            &mut encode_buf,
        );
        let encode_time = t_enc.elapsed();
        let message_id = generate_message_id(shared.config.message_id_domain.as_deref());
        let (rfc_date, _ts) = &task.date;
        if let Some(d) = &rfc_date {
            debug!(segment = %message_id, date = %d, "article date");
        }
        let article = Article {
            message_id: message_id.clone(),
            from: task.from.clone(),
            newsgroups: shared.post_group.clone(),
            subject: default_subject(
                &task.subject_name,
                task.part,
                task.total,
                (shared.total_files > 0).then_some((task.meta.file_index, shared.total_files)),
            ),
            date: rfc_date.clone(),
            no_archive: shared.config.no_archive,
        };
        let headers = article.build_headers();
        if let Some(dir) = &shared.spool_dir {
            if let Err(e) = crate::spool::write(
                dir,
                &task.meta.real_name,
                task.part,
                &message_id,
                &headers,
                &encoded.body,
            )
            .await
            {
                warn!(error = %e, "resume: failed to write spool entry; continuing without it");
            }
        }
        (message_id, headers, encoded, encode_time)
    };
    let date = task.date.clone();
    Some(ReadyArticle {
        task,
        message_id,
        headers,
        encoded,
        encode_time,
        date,
    })
}

async fn worker(
    shared: Arc<Shared>,
    mut rx: tokio::sync::mpsc::Receiver<ReadyArticle>,
    conn_id: usize,
    mut slot: ConnectionSlot,
    check_tx: Option<tokio::sync::mpsc::UnboundedSender<PostedSegment>>,
    broker: Option<Arc<ConnectionBroker>>,
) {
    let mut rate_limiter = RateLimiter::new(
        // Divide the global rate across all workers proportionally.
        if shared.config.upload_rate > 0 {
            let total = shared.config.total_connections().max(1);
            (shared.config.upload_rate / total as u64).max(1)
        } else {
            0
        },
    );

    // pipeline_depth == 0 means adaptive: measure RTT on the first article and
    // compute depth = ceil(post_time / encode_time), capped at MAX_AUTO_PIPELINE_DEPTH.
    let cfg_depth = shared.config.pipeline_depth;
    let is_adaptive = cfg_depth == 0;
    // Effective depth used for batch-filling; starts at 1 until warm-up is done.
    let mut effective_depth: usize = if is_adaptive || cfg_depth == 1 {
        1
    } else {
        cfg_depth
    };
    let mut warmup_done = !is_adaptive; // true from the start when not adaptive

    // Track when the connection was last used so we can send periodic keepalives
    // on idle connections (prevents servers from closing them during long PAR2
    // computations, check-phase waits, and --each transitions).
    let keepalive_interval = shared.config.keepalive_interval;
    let keepalive_enabled = keepalive_interval > 0;
    // Short wakeup period while idle: cycle through all workers quickly enough
    // that every connection gets its keepalive before the server's idle timeout.
    // 2 s × 30 workers = 60 s worst-case round-trip, well within a 2-min timeout.
    const IDLE_POLL: Duration = Duration::from_secs(2);
    // Wakeup period while paused — much shorter than `IDLE_POLL`, which is
    // tuned for keepalive fan-out across many workers, not for how quickly a
    // paused worker notices `cancelled`/resume. Cancelling must stay roughly
    // as responsive while paused as it already is everywhere else.
    const PAUSE_POLL: Duration = Duration::from_millis(100);
    let mut last_used = Instant::now();

    'worker: loop {
        if shared.cancelled.load(Ordering::Relaxed) {
            break;
        }

        if shared.paused.load(Ordering::Relaxed) {
            // Suspended at a segment-batch boundary: keep the connection
            // alive (the same MODE READER keepalive used for idle time
            // within a run) without consuming from the queue, so a producer
            // racing ahead applies natural back-pressure instead of the run
            // continuing underneath a "paused" UI that lied about it.
            while shared.paused.load(Ordering::Relaxed) && !shared.cancelled.load(Ordering::Relaxed)
            {
                if keepalive_enabled
                    && last_used.elapsed() >= Duration::from_secs(keepalive_interval)
                {
                    slot.keepalive().await;
                    last_used = Instant::now();
                }
                tokio::time::sleep(PAUSE_POLL).await;
            }
            continue;
        }

        let first = loop {
            if keepalive_enabled && last_used.elapsed() >= Duration::from_secs(keepalive_interval) {
                slot.keepalive().await;
                last_used = Instant::now();
            }
            tokio::select! {
                task = rx.recv() => match task {
                    Some(t) => {
                        last_used = Instant::now();
                        break t;
                    }
                    None => break 'worker,
                },
                _ = tokio::time::sleep(IDLE_POLL), if keepalive_enabled => {}
            }
        };
        let mut pending = vec![first];

        if effective_depth > 1 {
            while pending.len() < effective_depth {
                match rx.try_recv() {
                    Ok(t) => pending.push(t),
                    Err(_) => break,
                }
            }
        }

        for p in &pending {
            shared.emit(ProgressEvent::ConnectionBusy {
                conn: conn_id,
                file: p.task.meta.real_name.clone(),
            });
        }

        if pending.is_empty() {
            continue;
        }

        if shared.config.dry_run {
            for p in pending {
                shared.results.lock().unwrap().push(PostedSegment {
                    file_name: p.task.meta.real_name.clone(),
                    file_path: Arc::from(p.task.meta.path.as_path()),
                    // NZB uses the real filename, not wire subject (may be obfuscated).
                    subject_name: Arc::from(p.task.meta.real_name.as_str()),
                    wire_name: Arc::from(p.task.subject_name.as_str()),
                    file_size: p.task.meta.size,
                    part: p.task.part,
                    total: p.task.total,
                    message_id: p.message_id,
                    bytes: (p.headers.len() + p.encoded.body.len()) as u64,
                    from: Arc::from(p.task.from.as_str()),
                    date: p.date.clone(),
                    full_crc32: p.task.file_crc32.unwrap_or(0),
                    // Nothing was actually posted in dry-run mode, so there's
                    // no real server and no check queue — see the field doc.
                    server_idx: 0,
                    file_index: p.task.meta.file_index,
                    total_files: shared.total_files,
                });
                let bytes = p.task.data.len() as u64;
                shared.release_buffer(p.task.data);
                shared.emit(ProgressEvent::SegmentDone {
                    file: p.task.meta.real_name.clone(),
                    bytes,
                    ok: true,
                });
            }
            continue;
        }

        // Rate-limit on total bytes for the whole batch.
        let total_bytes: usize = pending
            .iter()
            .map(|p| p.headers.len() + p.encoded.body.len())
            .sum();
        rate_limiter.acquire(total_bytes).await;

        let max_attempts = shared.config.retries;

        if pending.len() == 1 {
            // ── Sequential path (depth 1 or only one task left) ──────────────
            let mut p = pending.remove(0);
            let mut posted = false;
            let mut last_err = String::from("unknown error");

            for attempt in 1..=max_attempts {
                let conn = match slot.ensure_connected().await {
                    Ok(c) => c,
                    Err(e) => {
                        last_err = format!("{e:#}");
                        warn!(segment = %p.message_id, attempt, max_attempts,
                              error = %last_err, "connection failed; will retry");
                        shared.total_retries.fetch_add(1, Ordering::Relaxed);
                        if attempt < max_attempts {
                            tokio::time::sleep(slot.retry_delay()).await;
                        }
                        continue;
                    }
                };
                let t_post = Instant::now();
                match conn.post_parts(&p.headers, &p.encoded.body).await {
                    Ok(returned_id) => {
                        // Some servers substitute their own Message-ID at
                        // accept time and echo it back in the 240 response
                        // instead of the one we sent — nyuu has handled this
                        // since 2016. Tracking our own ID after that would
                        // mean STAT (and the .nzb) reference an ID the
                        // server never actually stored anything under.
                        if let Some(server_id) = returned_id {
                            if server_id != p.message_id {
                                warn!(
                                    sent = %p.message_id,
                                    returned = %server_id,
                                    "server returned a different Message-ID than sent; adopting it"
                                );
                                p.message_id = server_id;
                            }
                        }
                        // Adaptive warm-up: compute pipeline depth from the
                        // ratio of post time (send + RTT) to encode time.
                        if is_adaptive && !warmup_done {
                            let post_us = t_post.elapsed().as_micros().max(1);
                            let enc_us = p.encode_time.as_micros().max(1);
                            let ratio = post_us.saturating_div(enc_us);
                            let depth = (ratio as usize).clamp(1, MAX_AUTO_PIPELINE_DEPTH);
                            effective_depth = depth;
                            warmup_done = true;
                            info!(
                                conn = conn_id,
                                depth,
                                post_ms = t_post.elapsed().as_millis(),
                                encode_us = enc_us,
                                "adaptive pipeline depth computed"
                            );
                        }
                        debug!(segment = %p.message_id, "posted");
                        posted = true;
                        break;
                    }
                    Err(e) => {
                        last_err = format!("{e:#}");
                        warn!(segment = %p.message_id, attempt, max_attempts,
                              error = %last_err, "post failed; rotating server");
                        shared.total_retries.fetch_add(1, Ordering::Relaxed);
                        slot.invalidate("post_err");
                    }
                }
                if attempt < max_attempts {
                    tokio::time::sleep(slot.retry_delay()).await;
                }
            }

            let wire = p.headers.len() + p.encoded.body.len();
            commit_result(
                &shared,
                check_tx.as_ref(),
                p.task,
                p.message_id,
                wire,
                posted,
                &last_err,
                p.date,
                slot.server_idx(),
            );
            shared.release_encode_buf(p.encoded.body);
        } else {
            // ── Pipelined path ───────────────────────────────────────────────
            // Send all articles back-to-back, flush once, then read all
            // responses. On any connection error the entire batch is retried.
            //
            // All conn usage is confined to the labeled block `'use_conn` so
            // that `slot.invalidate()` can be called after the block ends,
            // satisfying the borrow checker (conn borrows slot mutably).
            let n = pending.len();
            let mut pipeline_ok = false;
            let mut pipe_results: Vec<Result<(), String>> = (0..n).map(|_| Ok(())).collect();

            'pipeline: for attempt in 1..=max_attempts {
                // `(needs_invalidate, error_message)` — conn is dropped when
                // the labeled block expression completes.
                let (needs_invalidate, pipe_err) = 'use_conn: {
                    let conn = match slot.ensure_connected().await {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(attempt, max_attempts, error = %e,
                                  "connection failed during pipeline; will retry");
                            shared.total_retries.fetch_add(1, Ordering::Relaxed);
                            if attempt < max_attempts {
                                tokio::time::sleep(slot.retry_delay()).await;
                            }
                            continue 'pipeline;
                        }
                    };

                    // Enqueue all articles without flushing.
                    for p in &pending {
                        if let Err(e) = conn.enqueue_post(&p.headers, &p.encoded.body).await {
                            break 'use_conn (true, format!("{e:#}"));
                        }
                    }

                    // One flush covers all enqueued articles.
                    if let Err(e) = conn.flush_pipeline().await {
                        break 'use_conn (true, format!("{e:#}"));
                    }

                    // Read one (340, 240) pair per article. On error: record the
                    // failure index, break out of the for loop (dropping the
                    // iter_mut borrow), then mark remaining entries as failed.
                    let mut fail_at: Option<(usize, String)> = None;
                    for (i, result) in pipe_results.iter_mut().enumerate() {
                        match conn.read_post_response().await {
                            Ok(returned_id) => {
                                // See the sequential path above for why: some
                                // servers substitute their own Message-ID at
                                // accept time.
                                if let Some(server_id) = returned_id {
                                    if server_id != pending[i].message_id {
                                        warn!(
                                            sent = %pending[i].message_id,
                                            returned = %server_id,
                                            "server returned a different Message-ID than sent; adopting it"
                                        );
                                        pending[i].message_id = server_id;
                                    }
                                }
                                debug!(segment = %pending[i].message_id, "posted (pipelined)");
                                *result = Ok(());
                            }
                            Err(e) => {
                                *result = Err(format!("{e:#}"));
                                fail_at = Some((i + 1, format!("{e:#}")));
                                break;
                            }
                        }
                    }
                    // iter_mut borrow is dropped here; safe to index pipe_results.
                    if let Some((from, msg)) = fail_at {
                        for r in pipe_results[from..].iter_mut() {
                            // Remaining articles in the batch never received a
                            // response — the connection was lost after the first
                            // rejection. Use a distinct message so the log does
                            // not falsely repeat the first article's message-id.
                            *r = Err("pipeline interrupted after previous failure".into());
                        }
                        break 'use_conn (true, msg);
                    }

                    (false, String::new())
                }; // conn dropped; slot methods are safe to call again.

                if needs_invalidate {
                    warn!(attempt, max_attempts, error = %pipe_err,
                          "pipeline failed; rotating server");
                    shared.total_retries.fetch_add(1, Ordering::Relaxed);
                    slot.invalidate("post_err");
                    if attempt < max_attempts {
                        tokio::time::sleep(slot.retry_delay()).await;
                    }
                    continue;
                }

                pipeline_ok = true;
                break;
            }

            // The whole batch shares one connection/flush, so every article in
            // it — success or failure — was attempted against the same server.
            let batch_server_idx = slot.server_idx();
            for (p, result) in pending.into_iter().zip(pipe_results) {
                let posted = pipeline_ok && result.is_ok();
                let last_err = result.err().unwrap_or_else(|| "pipeline failed".into());
                let wire = p.headers.len() + p.encoded.body.len();
                commit_result(
                    &shared,
                    check_tx.as_ref(),
                    p.task,
                    p.message_id,
                    wire,
                    posted,
                    &last_err,
                    p.date,
                    batch_server_idx,
                );
                shared.release_encode_buf(p.encoded.body);
            }
        }
    }

    shared.emit(ProgressEvent::ConnectionIdle { conn: conn_id });
    match broker {
        Some(broker) => broker.checkin(slot).await,
        None => slot.quit().await,
    }
}

/// Build a `PostTask`, generating per-article subject and From when in
/// `ObfuscateMode::Paranoid`; otherwise copies them from `FileMeta`.
fn make_task(
    meta: Arc<FileMeta>,
    part: u32,
    total: u32,
    offset: u64,
    data: Vec<u8>,
    file_crc32: Option<u32>,
    config: &Config,
) -> PostTask {
    let (subject_name, from, date) = if config.obfuscate == ObfuscateMode::Paranoid {
        let date = resolve_date(config.date.as_deref());
        (obfuscated_name(), random_from(), date)
    } else {
        (
            meta.subject_name.clone(),
            meta.from.clone(),
            meta.date.clone(),
        )
    };
    PostTask {
        meta,
        part,
        total,
        offset,
        data,
        subject_name,
        from,
        date,
        file_crc32,
    }
}

/// Choose the newsgroup(s) for a whole run.
///
/// When several groups are configured, one is picked at random (once per run)
/// rather than cross-posting every article to all of them. The whole upload
/// then stays together in a single group, while the footprint still spreads
/// across the configured groups over many runs. Each entry in `groups` is a
/// "target" that may itself be several newsgroup names joined with `+` (or
/// the deprecated `,` alias) for a simultaneous cross-post (see
/// [`crate::config::validation::validate_groups`] for the syntax this
/// assumes has already been validated); the chosen target is split into the
/// flat list every caller expects. With zero or one configured entry
/// there's nothing to pick between, but the split still applies.
///
/// `pub` so a `--season` batch (`bin/pesto.rs`'s `run_batch`) can call this
/// once up front and force every episode's `Config::groups` to the same
/// pre-picked single-entry target. Otherwise each episode's own internal
/// call (inside [`post_files`]) re-rolls independently, scattering a
/// season's episodes across different newsgroups — the merged season NZB
/// then needs a `<groups>` list wide enough to cover all of them, and any
/// one episode's actual group may not even be among the ones another
/// episode's segments were checked against.
pub fn pick_post_group(groups: &[String]) -> Vec<String> {
    let target = match groups {
        [] => return Vec::new(),
        [one] => one.as_str(),
        many => {
            let idx = (rand_u64() % many.len() as u64) as usize;
            many[idx].as_str()
        }
    };
    target
        .split(['+', ','])
        .map(|s| s.trim().to_string())
        .collect()
}

/// Compute the `Date:` header value and its Unix timestamp from the config
/// `date` option.
///
/// - `None` → `(None, None)` — header omitted, server fills it in.
/// - `"now"` → current UTC time formatted as RFC 2822.
/// - `"random"` → random time within the last 2 hours.
/// - any other string → used verbatim (caller-supplied RFC 2822 timestamp).
///
/// Returns `(rfc_2822_string, unix_timestamp_secs)`.
fn resolve_date(mode: Option<&str>) -> (Option<String>, Option<u64>) {
    match mode {
        None => (None, None),
        Some("now") => {
            let now = SystemTime::now();
            let ts = now
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            (Some(format_rfc2822(now)), Some(ts))
        }
        Some("random") => {
            // Pick a random offset in [0, 2h) before now.
            // This breaks the obvious same-timestamp pattern that reveals
            // articles belong to the same upload batch, while staying well
            // inside the acceptance window of servers that reject articles
            // whose Date is too far in the past (e.g. blocknews returns
            // `441 437 ... TooOld`). A wider window (24h) tripped that limit
            // for a small random subset of articles on every obfuscated run.
            use std::collections::hash_map::RandomState;
            use std::hash::{BuildHasher, Hasher};
            let r = RandomState::new().build_hasher().finish();
            let offset_secs = r % (2 * 3600);
            let t = SystemTime::now()
                .checked_sub(Duration::from_secs(offset_secs))
                .unwrap_or(UNIX_EPOCH);
            let ts = t
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            (Some(format_rfc2822(t)), Some(ts))
        }
        Some(fixed) => {
            // For fixed dates we don't parse back to unix; the NZB will fall
            // back to SystemTime::now() if the caller needs a timestamp.
            (Some(fixed.to_string()), None)
        }
    }
}

/// Persist a successfully posted segment or record a failure, then emit the
/// corresponding progress event and release the article buffer back to the pool.
#[allow(clippy::too_many_arguments)]
fn commit_result(
    shared: &Shared,
    check_tx: Option<&tokio::sync::mpsc::UnboundedSender<PostedSegment>>,
    task: PostTask,
    message_id: String,
    wire_bytes: usize,
    posted: bool,
    last_err: &str,
    date: (Option<String>, Option<u64>),
    server_idx: usize,
) {
    if posted {
        if let Some(resume) = &shared.resume {
            // In-memory only — no disk write here. Every commit used to
            // rewrite the entire state file while holding this lock, which
            // serialized all workers through one lock and turned state
            // tracking into an O(n^2) hot-path cost on large uploads. Now
            // that resume state is tracked unconditionally (not just when
            // --resume is passed), persisting had to move off this path
            // regardless — the whole point of resume is to survive the *end*
            // of a run being incomplete, not every individual segment, so a
            // single persist decided by the final outcome (see the
            // still_missing handling and `run_single_upload`'s cleanup)
            // covers the same guarantee at a fraction of the cost.
            resume.lock().unwrap().record(
                &task.meta.real_name,
                task.part,
                &message_id,
                wire_bytes as u64,
            );
        }
        // Confirmed posted — any spooled copy has served its purpose.
        if let Some(dir) = &shared.spool_dir {
            crate::spool::remove(dir, &task.meta.real_name, task.part);
        }
        let seg = PostedSegment {
            file_name: task.meta.real_name.clone(),
            file_path: Arc::from(task.meta.path.as_path()),
            // NZB uses the real filename for proper client-side renaming.
            subject_name: Arc::from(task.meta.real_name.as_str()),
            wire_name: Arc::from(task.subject_name.as_str()),
            file_size: task.meta.size,
            part: task.part,
            total: task.total,
            message_id,
            bytes: wire_bytes as u64,
            from: Arc::from(task.from.as_str()),
            date,
            full_crc32: task.file_crc32.unwrap_or(0),
            server_idx,
            file_index: task.meta.file_index,
            total_files: shared.total_files,
        };
        if let Some(tx) = check_tx {
            let _ = tx.send(seg.clone());
        }
        shared.results.lock().unwrap().push(seg);
    } else {
        record_failure(shared, &task.meta, &task, message_id, last_err);
    }
    let article_bytes = task.data.len() as u64;
    shared.release_buffer(task.data);
    shared.emit(ProgressEvent::SegmentDone {
        file: task.meta.real_name.clone(),
        bytes: article_bytes,
        ok: posted,
    });
}

/// Add ±50 % jitter to `base` to prevent synchronized reconnect bursts.
///
/// Uses `slot_id` mixed with the current nanosecond timestamp as a cheap
/// pseudo-random seed — no external crate required.
fn jittered(base: Duration, slot_id: usize) -> Duration {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    // 0..=999 range → [1.0, 1.5) multiplier
    let noise = (ns.wrapping_add(slot_id as u64 * 2_654_435_761) % 1000) as u32;
    let extra_ms = (base.as_millis() as u64 * noise as u64 / 2000) as u32;
    base + Duration::from_millis(extra_ms as u64)
}

/// Whether an automatic final recovery pass (see `check::recover_missing`)
/// is worth attempting for `missing` still-unconfirmed articles out of
/// `total` posted this run. Gated by *both* an absolute cap
/// (`check_recover_max`) and a percentage of the release
/// (`check_recover_percent`) — whichever is smaller wins, so behaviour
/// scales sanely from a small release (where even a large fraction missing
/// is only a handful of articles) to a huge one (where 15% could still be
/// thousands of articles, no longer "cheap" to retry automatically).
fn is_cheap_to_recover(missing: usize, total: usize, config: &Config) -> bool {
    if missing == 0 || config.check_recover_max == 0 {
        return false;
    }
    if missing > config.check_recover_max {
        return false;
    }
    let percent_cap = (total as f64 * config.check_recover_percent as f64 / 100.0).ceil() as usize;
    missing <= percent_cap.max(1)
}

/// Build the "→ ..." label shown in the live panel header and the `Started`
/// progress event's `target` field. Lists every configured server (not just
/// the primary) so a multi-server run doesn't look single-server for its
/// entire duration — see the call site's comment for why this is knowable
/// up front, unlike `groups`.
fn target_label(servers: &[crate::config::ServerEntry], total_connections: usize) -> String {
    match servers {
        [] => String::new(),
        [only] => format!("{}:{}", only.host, only.port),
        _ if servers.len() <= 3 => servers
            .iter()
            .map(|s| s.host.as_str())
            .collect::<Vec<_>>()
            .join(" + "),
        _ => format!("{} servers ({total_connections} conn)", servers.len()),
    }
}

fn record_failure(
    shared: &Shared,
    meta: &FileMeta,
    task: &PostTask,
    message_id: String,
    error: &str,
) {
    let description = format!(
        "{} part {}/{}: {error}",
        meta.real_name, task.part, task.total
    );
    shared.emit(ProgressEvent::Failed {
        description: description.clone(),
    });
    shared.failures.lock().unwrap().push(description);
    shared.failed_tasks.lock().unwrap().push(FailedTask {
        file_name: meta.real_name.clone(),
        file_path: meta.path.clone(),
        message_id,
        subject_name: task.subject_name.clone(),
        yenc_name: meta.yenc_name.clone(),
        file_size: meta.size,
        part: task.part,
        total: task.total,
        from: task.from.clone(),
        date: task.date.clone(),
        full_crc32: task.file_crc32.unwrap_or(0),
        file_index: meta.file_index,
        total_files: shared.total_files,
    });
}

/// Post a fresh copy of each segment in `failed`, re-posting under the
/// *same* `Message-ID` the in-run attempt used (see the comment on
/// `message_id` below for why). Returns the `PostedSegment`s that were
/// successfully posted; tasks that exhaust all retries are silently dropped
/// (the caller can compare lengths to detect persistent failures).
pub async fn repost_failed_tasks(
    config: &Config,
    failed: &[FailedTask],
    groups: &[String],
    events: Option<&ProgressSender>,
    cancel: Option<&Arc<AtomicBool>>,
) -> Result<Vec<PostedSegment>> {
    if failed.is_empty() {
        return Ok(Vec::new());
    }

    let server = config
        .all_servers()
        .next()
        .expect("at least one server is configured");
    let mut slot = ConnectionSlot::new(Arc::new(vec![server]), 0);

    let article_size = config.article_size as u64;
    let max_retries = config.retries.max(1);
    let mut recovered: Vec<PostedSegment> = Vec::new();

    for (i, task) in failed.iter().enumerate() {
        if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
            break;
        }
        let offset = (task.part as u64 - 1) * article_size;
        let read_len = (task.file_size - offset).min(article_size) as usize;

        // Re-read from the preserved absolute path, not `file_name` (which is
        // only the published/relative name and would resolve against the CWD).
        let path = task.file_path.clone();
        let mut file = match File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                warn!(file = %task.file_name, path = %path.display(), "retry: cannot open file: {e}");
                continue;
            }
        };

        use tokio::io::AsyncSeekExt;
        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
            warn!(file = %task.file_name, offset, "retry: seek failed: {e}");
            continue;
        }

        let mut buf = vec![0u8; read_len];
        if let Err(e) = file.read_exact(&mut buf).await {
            warn!(file = %task.file_name, "retry: read failed: {e}");
            continue;
        }

        let spec = yenc::PartSpec {
            number: task.part,
            total: task.total,
            offset,
        };
        let file_crc32 = (task.part == task.total).then_some(task.full_crc32);
        let encoded = yenc::encode_part(
            &task.yenc_name,
            task.file_size,
            spec,
            &buf,
            config.line_length,
            file_crc32,
        );
        // Re-post with the *same* Message-ID the in-run attempts used, so a
        // server that already has the article (lost `240` ack) deduplicates it
        // via `435 Already exists` instead of accepting a duplicate under a
        // fresh ID. See [`FailedTask::message_id`].
        let mut message_id = task.message_id.clone();
        let (rfc_date, _ts) = &task.date;
        let article = Article {
            message_id: message_id.clone(),
            from: task.from.clone(),
            newsgroups: groups.to_vec(),
            subject: default_subject(
                &task.subject_name,
                task.part,
                task.total,
                (task.total_files > 0).then_some((task.file_index, task.total_files)),
            ),
            date: rfc_date.clone(),
            no_archive: config.no_archive,
        };
        let headers = article.build_headers();
        let wire_bytes = (headers.len() + encoded.body.len()) as u64;

        let mut ok = false;
        for attempt in 1..=max_retries {
            match slot.ensure_connected().await {
                Ok(conn) => match conn.post_parts(&headers, &encoded.body).await {
                    Ok(returned_id) => {
                        // See the main post path for why: some servers
                        // substitute their own Message-ID at accept time.
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
                        ok = true;
                        break;
                    }
                    Err(e) => {
                        slot.invalidate("post_err");
                        warn!(file = %task.file_name, part = task.part, attempt, "retry attempt failed: {e}");
                        if attempt < max_retries {
                            if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
                                break;
                            }
                            tokio::time::sleep(Duration::from_secs(config.retry_delay)).await;
                        }
                    }
                },
                Err(e) => {
                    warn!(attempt, "retry: connect failed: {e}");
                    if attempt < max_retries {
                        if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(config.retry_delay)).await;
                    }
                }
            }
        }

        if ok {
            recovered.push(PostedSegment {
                file_name: task.file_name.clone(),
                file_path: Arc::from(task.file_path.as_path()),
                // NZB uses the real filename, not obfuscated wire subject.
                subject_name: Arc::from(task.file_name.as_str()),
                wire_name: Arc::from(task.subject_name.as_str()),
                file_size: task.file_size,
                part: task.part,
                total: task.total,
                message_id,
                bytes: wire_bytes,
                // `slot` only ever targets the primary server (index 0 in
                // `config.all_servers()` too — see its "primary first" order),
                // since this blind end-of-run retry doesn't fail over.
                server_idx: slot.server_idx(),
                from: Arc::from(task.from.as_str()),
                date: task.date.clone(),
                full_crc32: task.full_crc32,
                file_index: task.file_index,
                total_files: task.total_files,
            });
            if let Some(tx) = events {
                let _ = tx.send(ProgressEvent::Status {
                    text: format!("retry: {}/{} segment(s) recovered", recovered.len(), i + 1),
                });
            }
        } else {
            warn!(
                file = %task.file_name,
                part = task.part,
                "retry: gave up after all attempts"
            );
        }
    }

    if let Some(tx) = events {
        let _ = tx.send(ProgressEvent::Status {
            text: String::new(),
        });
    }

    Ok(recovered)
}

/// One episode's identity within a season-wide PAR2 recovery set: enough to
/// emit a File Description + IFSC packet pair for it. `name` is the bare
/// file name (no directory components) — the season equivalent of
/// [`wire_name`] for a single-file entry, since each episode path here is
/// already one standalone top-level entry, never a release subdirectory.
struct SeasonFileEntry {
    file_id: [u8; 16],
    name: String,
    hashes: FileHashes,
    slice_checksums: Vec<SliceChecksum>,
}

/// A season-wide PAR2 recovery set, ready to be serialized to disk by
/// [`write_season_par2_volumes`]. Carries one [`SeasonFileEntry`] per
/// episode so the written volumes include real File Description/IFSC
/// packets — see [`generate_season_par2`]'s doc comment for why that matters.
struct SeasonPar2Set {
    /// Number of recovery blocks written (or that would be written). The
    /// slice *bodies* are streamed to disk per pass — holding them here
    /// is what OOM-killed a 120 GB `--season` pack (#110).
    recovery_count: usize,
    par2_slice_size: usize,
    files: Vec<SeasonFileEntry>,
}

impl SeasonPar2Set {
    fn empty() -> Self {
        Self {
            recovery_count: 0,
            par2_slice_size: 0,
            files: Vec::new(),
        }
    }
}

/// Packet prefix every season volume carries: Main (with every episode's
/// File ID), Creator, and one File Description + IFSC pair per episode.
/// Without this the recovery set describes no files at all (see
/// `generate_season_par2`'s doc comment).
fn season_base_packets(files: &[SeasonFileEntry], par2_slice_size: usize) -> ([u8; 16], Vec<u8>) {
    let file_ids: Vec<[u8; 16]> = files.iter().map(|f| f.file_id).collect();
    let main_b = packet::main_body(par2_slice_size as u64, &file_ids);
    let rsid = packet::recovery_set_id(&main_b);

    let pkt_main = packet::serialize_packet(&rsid, &packet::TYPE_MAIN, &main_b);
    let pkt_creator =
        packet::serialize_packet(&rsid, &packet::TYPE_CREATOR, &packet::creator_body("pesto"));
    let mut base_packets = pkt_main;
    base_packets.extend(pkt_creator);

    for file in files {
        let pkt_file_desc = packet::serialize_packet(
            &rsid,
            &packet::TYPE_FILE_DESC,
            &packet::file_description_body(
                &file.file_id,
                &file.hashes.md5_full,
                &file.hashes.md5_16k,
                file.hashes.length,
                &file.name,
            ),
        );
        let pkt_ifsc = packet::serialize_packet(
            &rsid,
            &packet::TYPE_IFSC,
            &packet::ifsc_body(&file.file_id, &file.slice_checksums),
        );
        base_packets.extend(pkt_file_desc);
        base_packets.extend(pkt_ifsc);
    }
    (rsid, base_packets)
}

/// Append one pass of recovery slices to the on-disk volume files, then drop
/// the slice bodies. Same append-as-we-go pattern as `producer`: peak RAM is
/// one pass of recovery data, not the whole set (#110).
async fn append_season_recovery_slices(
    slices: Vec<parmesan::encoder::RecoverySlice>,
    volumes: &[layout::VolumeChunk],
    release_name: &str,
    output_dir: &Path,
    rsid: &[u8; 16],
    base_packets: &[u8],
) -> Result<()> {
    for slice in slices {
        let vol = volumes
            .iter()
            .find(|v| slice.exponent >= v.first && slice.exponent < v.first + v.count)
            .ok_or_else(|| anyhow::anyhow!("recovery slice exponent out of range"))?;

        let vol_name = layout::volume_name(release_name, *vol);
        let vol_path = output_dir.join(&vol_name);

        let mut file = tokio::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .append(true)
            .open(&vol_path)
            .await?;

        if slice.exponent == vol.first {
            file.write_all(base_packets).await?;
        }

        let pkt = packet::serialize_packet(
            rsid,
            &packet::TYPE_RECOVERY,
            &packet::recovery_body(slice.exponent, &slice.data),
        );
        file.write_all(&pkt).await?;
    }
    Ok(())
}

/// Read every episode into `worker` as PAR2 input slices. Empty files
/// contribute no slices (the hasher never sees an `is_last_of_file` for
/// them). Returns `(names, slices_per_episode, total_slices_added)`.
async fn feed_season_episodes(
    ordered: &[(PathBuf, String, u64)],
    worker: &Par2Worker,
    par2_slice_size: usize,
) -> Result<(Vec<String>, Vec<usize>, usize)> {
    let mut episode_names = Vec::with_capacity(ordered.len());
    let mut slices_per_episode = Vec::with_capacity(ordered.len());
    let mut total_slices_added = 0;

    for (ep_idx, (episode_path, name, file_size)) in ordered.iter().enumerate() {
        let file_size = *file_size;
        episode_names.push(name.clone());

        if file_size == 0 {
            slices_per_episode.push(0);
            continue;
        }

        let input = Par2InputFile {
            path: episode_path.clone(),
            display_name: name.clone(),
            size: file_size,
        };
        ingest_files_with(
            std::slice::from_ref(&input),
            worker,
            par2_slice_size,
            None,
            |_| Ok(()),
        )
        .await?;

        let slices_for_episode = (file_size as usize).div_ceil(par2_slice_size);
        total_slices_added += slices_for_episode;
        slices_per_episode.push(slices_for_episode);
        debug!(
            episode_idx = ep_idx + 1,
            total_episodes = ordered.len(),
            file_size,
            slices_for_episode,
            expected_slices = (file_size as usize).div_ceil(par2_slice_size),
            "finished reading episode"
        );
    }

    Ok((episode_names, slices_per_episode, total_slices_added))
}

/// Generate a global PAR2 recovery set that covers all episodes in a season.
///
/// Reads the episode files (once per memory-budget pass), feeds them into a
/// PAR2 encoder, and streams each pass's recovery slices to `output_dir`
/// immediately — the same append-as-we-go pattern as `producer`. Holding
/// every recovery block until the end is what OOM-killed a 120 GB `--season`
/// pack even after the encoder itself was split into passes (#110). Also
/// returns per-episode File IDs/hashes so each volume's base packets carry
/// real File Description + IFSC data.
///
/// This enables a coherent PAR2 recovery set ID (rsid) that covers the entire
/// season, rather than multiple independent rsids for individual episodes —
/// while still describing every episode file by name, exactly like the
/// per-file PAR2 path does. Earlier versions of this function only produced
/// anonymous recovery data (a Main packet with an empty File ID list, no
/// File Description/IFSC packets at all): syntactically valid PAR2, but with
/// no file association whatsoever, so no downloader could verify, repair, or
/// — under `--obfuscate` — de-obfuscate a season pack's episodes against it.
/// Per-episode PAR2 sets *did* carry the real name correctly, but got
/// discarded once merged into the season NZB in favor of this global set.
async fn generate_season_par2(
    episode_paths: &[PathBuf],
    config: &Config,
    release_name: &str,
    output_dir: &Path,
) -> Result<SeasonPar2Set> {
    if episode_paths.is_empty() {
        return Ok(SeasonPar2Set::empty());
    }

    if config.par2 == 0 {
        return Ok(SeasonPar2Set::empty());
    }

    debug!(episodes = episode_paths.len(), "generating season PAR2");

    // PAR2 numbers its input blocks by walking the recovery-set files in
    // File-ID order (par2 spec, Main packet) — third-party tools (par2cmdline,
    // MultiPar, SABnzbd) assume this canonical order when mapping
    // Reed-Solomon coefficients back to input slices, regardless of the order
    // files happen to be fed to the encoder. `episode_paths` arrives in
    // argument/directory order, so it must be re-sorted by File ID before any
    // slice is fed — exactly like the per-file PAR2 path above does for
    // `metas` (see its own comment, `keyed.sort_by_key`). Skipping this once
    // produced PAR2 volumes that verified/repaired against pesto's own
    // encoder but failed real repair against par2cmdline, since its Main
    // packet lists File IDs in sorted order while the recovery blocks were
    // computed against filesystem-listing order.
    let mut ordered: Vec<(PathBuf, String, u64)> = Vec::with_capacity(episode_paths.len());
    for path in episode_paths {
        let size = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("reading metadata of episode `{}`", path.display()))?
            .len();
        let name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| path.display().to_string());
        ordered.push((path.clone(), name, size));
    }
    if ordered.len() > 1 {
        let mut keyed = Vec::with_capacity(ordered.len());
        for (path, name, size) in ordered {
            let md5_16k = file_md5_16k(&path, size).await?;
            let file_id = packet::compute_file_id(&md5_16k, size, &name);
            keyed.push((file_id, path, name, size));
        }
        keyed.sort_by_key(|(file_id, ..)| *file_id);
        ordered = keyed
            .into_iter()
            .map(|(_, path, name, size)| (path, name, size))
            .collect();
    }

    let sizes: Vec<u64> = ordered.iter().map(|(_, _, size)| *size).collect();
    let (par2_slice_size, total_slices, recovery_count) = par2_geometry_from_sizes(&sizes, config);
    debug!(
        par2_slice_size,
        total_slices, recovery_count, "season PAR2 geometry"
    );

    // Validate PAR2 spec limits — same check `producer()` does for the
    // per-file path. Sharing `par2_geometry_from_sizes` between the two
    // paths means an explicit `--par2-slice-count`/`--par2-recovery-count`
    // can equally overflow the GF(2^16) exponent space here.
    if total_slices > 32768 {
        anyhow::bail!("too many input slices: {total_slices} (max 32768). Increase --slice-size or decrease --slice-count.");
    }
    if recovery_count > 65535 {
        anyhow::bail!("too many recovery blocks: {recovery_count} (max 65535). Increase --slice-size or decrease --par2/--recovery-count.");
    }

    if total_slices == 0 {
        return Ok(SeasonPar2Set::empty());
    }

    if recovery_count == 0 {
        return Ok(SeasonPar2Set::empty());
    }

    info!(
        episodes = episode_paths.len(),
        par2_slice_size, total_slices, recovery_count, "season PAR2 configuration"
    );

    // Same budget/pass split as the per-file producer. Connection reserve
    // is 0: season generation runs before (or without) the NNTP pool, like
    // `--par2-before-upload`.
    let (memory_limit, passes) = par2_memory_plan(config, par2_slice_size, recovery_count, 0)?;
    info!(
        memory_limit,
        passes = passes.len(),
        "season PAR2 memory plan"
    );

    let volumes = layout::plan_volumes(recovery_count as u32);
    let mut files = Vec::new();
    let mut rsid = [0u8; 16];
    let mut base_packets = Vec::new();
    let mut written = 0usize;

    tokio::fs::create_dir_all(output_dir)
        .await
        .with_context(|| format!("creating season PAR2 output dir `{}`", output_dir.display()))?;

    for (pass_idx, (exp_start, rec_count)) in passes.iter().copied().enumerate() {
        if rec_count == 0 {
            continue;
        }
        let mut enc =
            RecoveryEncoder::try_new_smart(par2_slice_size, total_slices, exp_start, rec_count)
                .context("allocating season PAR2 recovery buffers")?
                .with_simd_path(config.simd);
        if pass_idx == 0 {
            enc = enc.with_checksums();
        }
        let queue_limit = (memory_limit / 4).clamp(256 * 1024 * 1024, 2 * 1024 * 1024 * 1024);
        let enc = enc.with_flush_limit(queue_limit);
        let worker = Par2Worker::spawn(enc, pass_idx == 0, parmesan::worker::DEFAULT_CHANNEL_DEPTH);

        let (names, slices, added) =
            feed_season_episodes(&ordered, &worker, par2_slice_size).await?;
        debug!(
            pass = pass_idx,
            calculated_total_slices = total_slices,
            actual_slices_added = added,
            "season PAR2 pass fed"
        );

        let (recovery, checksums, pass_hashes) = tokio::task::block_in_place(|| worker.finish());
        written += recovery.len();

        if pass_idx == 0 {
            // Reassemble per-episode File ID/hash/checksum data — same
            // reconstruction the per-file path uses, including empty files.
            let md5_empty: [u8; 16] = packet::md5(b"");
            let mut hashes_iter = pass_hashes.into_iter();
            let mut checksums_cursor = 0usize;
            files = Vec::with_capacity(names.len());
            for (name, slice_count) in names.into_iter().zip(slices) {
                let fh = if slice_count == 0 {
                    FileHashes {
                        md5_full: md5_empty,
                        md5_16k: md5_empty,
                        length: 0,
                    }
                } else {
                    hashes_iter
                        .next()
                        .expect("par2 worker returned fewer hashes than non-empty episodes")
                };
                let file_checksums =
                    checksums[checksums_cursor..checksums_cursor + slice_count].to_vec();
                checksums_cursor += slice_count;
                let file_id = packet::compute_file_id(&fh.md5_16k, fh.length, &name);
                files.push(SeasonFileEntry {
                    file_id,
                    name,
                    hashes: fh,
                    slice_checksums: file_checksums,
                });
            }
            (rsid, base_packets) = season_base_packets(&files, par2_slice_size);
        }

        append_season_recovery_slices(
            recovery,
            &volumes,
            release_name,
            output_dir,
            &rsid,
            &base_packets,
        )
        .await?;
    }

    info!(
        recovery_slices = written,
        passes = passes.len(),
        "season PAR2 generation complete"
    );

    Ok(SeasonPar2Set {
        recovery_count: written,
        par2_slice_size,
        files,
    })
}

/// Generate and write global PAR2 volumes for season consolidation.
///
/// High-level wrapper that:
/// 1. Generates recovery slices (and per-episode File ID/hash data) covering all episodes
/// 2. Writes volumes to output directory
/// 3. Returns path to output directory
///
/// Used by season consolidation to create a single, coherent PAR2 set
/// that covers the entire season at once.
pub async fn generate_and_write_season_par2(
    episode_paths: &[PathBuf],
    release_name: &str,
    output_dir: &Path,
    config: &Config,
) -> Result<PathBuf> {
    if episode_paths.is_empty() || config.par2 == 0 {
        return Ok(output_dir.to_path_buf());
    }

    debug!(episodes = episode_paths.len(), "generating season PAR2");

    let season = generate_season_par2(episode_paths, config, release_name, output_dir).await?;

    if season.recovery_count == 0 {
        return Ok(output_dir.to_path_buf());
    }

    info!(
        episodes = season.files.len(),
        recovery_slices = season.recovery_count,
        slice_size = season.par2_slice_size,
        output_dir = %output_dir.display(),
        "season PAR2 volumes written"
    );

    Ok(output_dir.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, FileConfig, Overrides};
    use crate::walk::InputFile;
    use tempfile::TempDir;

    #[test]
    fn message_id_domain_is_random() {
        let a = crate::article::generate_message_id(None);
        let b = crate::article::generate_message_id(None);
        assert_ne!(a, b);
        assert!(a.contains('@'));
        assert!(!a.contains("blocknews") && !a.contains("pesto"));
    }

    // ── address_space_limit / connection_overhead_reserve ────────────────────

    #[test]
    fn address_space_limit_does_not_panic() {
        // Value is whatever the test host's `ulimit -v` happens to be — just
        // assert the call is sane, not a specific number.
        if let Some(limit) = address_space_limit() {
            assert!(limit > 0);
        }
    }

    #[test]
    fn connection_overhead_reserve_scales_with_connections_and_threads() {
        let base = connection_overhead_reserve(0, 0);
        assert_eq!(base, 1024 * 1024 * 1024);

        let with_200_conns = connection_overhead_reserve(200, 0);
        assert_eq!(with_200_conns, base + 200 * 8 * 1024 * 1024);
        assert!(with_200_conns > base);

        let with_threads = connection_overhead_reserve(0, 128);
        assert_eq!(with_threads, base + 128 * 4 * 1024 * 1024);
        assert!(with_threads > base);
    }

    #[test]
    fn per_thread_reserve_reflects_measured_stack_size() {
        // Regression guard for the constant that made the old formula wrong:
        // 128 threads used to reserve 4 GiB for stacks that measure 1 MiB
        // each. Anyone raising this again should have measurements in hand.
        let threads = 128usize;
        let per_thread = (connection_overhead_reserve(0, threads)
            - connection_overhead_reserve(0, 0))
            / threads as u64;
        assert_eq!(per_thread, 4 * 1024 * 1024);
        assert!(
            per_thread <= 8 * 1024 * 1024,
            "per-thread reserve drifted back up; it directly costs PAR2 budget"
        );
    }

    /// Reproduce `address_space_budget`'s arithmetic for a given ceiling.
    ///
    /// The real function reads `RLIMIT_AS`, which cannot be faked without
    /// mutating process-global state shared with every other test, so the
    /// model is exercised here against the same constants.
    fn budget_for(ceiling: u64, reserve: u64, slice_size: usize, recovery: usize) -> u64 {
        let headroom = (ceiling as f64 * CEILING_TARGET) - reserve as f64;
        if headroom <= 0.0 {
            return 0;
        }
        let single = headroom / PASS_WORKING_SET_FACTOR;
        if slice_size == 0
            || recovery == 0
            || (single as u64) / (slice_size as u64) >= recovery as u64
        {
            return single as u64;
        }
        (headroom / (PASS_WORKING_SET_FACTOR * (1.0 + CROSS_PASS_RETENTION))) as u64
    }

    #[test]
    fn single_pass_budget_beats_multi_pass_budget() {
        let ceiling = 10 * 1024 * 1024 * 1024u64;
        let reserve = 1024 * 1024 * 1024u64;
        let slice = 40 * 1024 * 1024usize;

        // 4 recovery blocks fit in one pass; 4096 cannot.
        let single = budget_for(ceiling, reserve, slice, 4);
        let multi = budget_for(ceiling, reserve, slice, 4096);
        assert!(
            single > multi,
            "a single-pass run pays no retention cost and must get the larger budget"
        );
        // The multi-pass branch is exactly the retention discount.
        let expected = (single as f64 / (1.0 + CROSS_PASS_RETENTION)) as u64;
        assert!(multi.abs_diff(expected) < 1024 * 1024);
    }

    #[test]
    fn multi_pass_peak_stays_under_the_ceiling() {
        // The whole point of the model: budget x working-set x (1 + retention),
        // plus the reserve, must land under the ceiling. This is the assertion
        // the old flat-50% formula could not make, because it had no term for
        // what a finished pass leaves behind.
        let ceiling = 10 * 1024 * 1024 * 1024u64;
        let reserve = 1024 * 1024 * 1024u64;
        let budget = budget_for(ceiling, reserve, 40 * 1024 * 1024, 4096);

        let predicted_peak =
            reserve as f64 + budget as f64 * PASS_WORKING_SET_FACTOR * (1.0 + CROSS_PASS_RETENTION);
        assert!(
            predicted_peak <= ceiling as f64 * CEILING_TARGET + 1.0,
            "predicted peak {predicted_peak} exceeds the {CEILING_TARGET} target of {ceiling}"
        );
        assert!(predicted_peak < ceiling as f64);
    }

    #[test]
    fn budget_is_zero_when_reserve_swallows_the_ceiling() {
        // Degenerate case: a tiny `ulimit -v` with many threads/connections.
        // Must not wrap around or return a microscopic budget that would imply
        // thousands of passes — the caller surfaces zero as an error.
        let ceiling = 512 * 1024 * 1024u64;
        let reserve = 4 * 1024 * 1024 * 1024u64;
        assert_eq!(budget_for(ceiling, reserve, 40 * 1024 * 1024, 100), 0);
    }

    // ── target_label ──────────────────────────────────────────────────────────

    fn test_server(host: &str) -> crate::config::ServerEntry {
        crate::config::ServerEntry {
            host: host.to_string(),
            port: 563,
            ssl: true,
            connections: 50,
            username: None,
            password: None,
            retry_delay: 1,
            timeout: 60,
        }
    }

    #[test]
    fn target_label_single_server_shows_host_and_port() {
        let servers = vec![test_server("news.example.com")];
        assert_eq!(target_label(&servers, 50), "news.example.com:563");
    }

    #[test]
    fn target_label_two_servers_lists_both_hosts() {
        let servers = vec![
            test_server("usnews.blocknews.net"),
            test_server("news.newshosting.com"),
        ];
        assert_eq!(
            target_label(&servers, 100),
            "usnews.blocknews.net + news.newshosting.com"
        );
    }

    #[test]
    fn target_label_many_servers_falls_back_to_a_count() {
        let servers = vec![
            test_server("a.example.com"),
            test_server("b.example.com"),
            test_server("c.example.com"),
            test_server("d.example.com"),
        ];
        assert_eq!(target_label(&servers, 200), "4 servers (200 conn)");
    }

    // ── wire_name ─────────────────────────────────────────────────────────────

    #[test]
    fn wire_name_strips_single_directory_prefix() {
        assert_eq!(wire_name("Season01/ep01.mkv"), "ep01.mkv");
    }

    #[test]
    fn wire_name_strips_only_first_component() {
        assert_eq!(wire_name("Release/VIDEO_TS/file.vob"), "VIDEO_TS/file.vob");
    }

    #[test]
    fn wire_name_no_slash_unchanged() {
        assert_eq!(wire_name("movie.mkv"), "movie.mkv");
        assert_eq!(wire_name("Release.par2"), "Release.par2");
    }

    #[test]
    fn wire_name_empty_string() {
        assert_eq!(wire_name(""), "");
    }

    // ── par2_base ─────────────────────────────────────────────────────────────

    #[test]
    fn par2_base_single_component() {
        assert_eq!(par2_base("movie.mkv"), "movie.mkv");
    }

    #[test]
    fn par2_base_relative_path_returns_root_folder() {
        assert_eq!(par2_base("Season01/ep01.mkv"), "Season01");
        assert_eq!(par2_base("a/b/c.bin"), "a");
    }

    #[test]
    fn par2_base_empty_string() {
        // Should not panic; returns the whole (empty) string.
        assert_eq!(par2_base(""), "");
    }

    // ── par2_release_base ────────────────────────────────────────────────────

    #[test]
    fn par2_release_base_strips_rar_volume_suffix() {
        assert_eq!(
            par2_release_base("archive.part01.rar"),
            "archive",
            "PAR2 set for a volume-split rar archive must not be named after \
             one specific volume"
        );
        assert_eq!(par2_release_base("archive.part1.rar"), "archive");
    }

    #[test]
    fn par2_release_base_strips_sevenzip_volume_suffix() {
        assert_eq!(par2_release_base("archive.7z.001"), "archive");
    }

    #[test]
    fn par2_release_base_leaves_non_volume_names_untouched() {
        assert_eq!(par2_release_base("movie.mkv"), "movie.mkv");
        assert_eq!(par2_release_base("archive.rar"), "archive.rar");
        assert_eq!(par2_release_base("archive.7z"), "archive.7z");
    }

    #[test]
    fn par2_release_base_still_roots_season_packs_at_the_folder() {
        assert_eq!(par2_release_base("Season01/ep01.mkv"), "Season01");
    }

    // ── PAR2 geometry ────────────────────────────────────────────────────────

    fn optimal_par2_slice_size(
        per_file_articles: &[usize],
        article_size: usize,
        redundancy_pct: u8,
    ) -> (usize, usize) {
        if per_file_articles.is_empty() || per_file_articles.iter().all(|&n| n == 0) {
            return (article_size, 0);
        }
        let sizes: Vec<u64> = per_file_articles
            .iter()
            .map(|&n| n as u64 * article_size as u64)
            .collect();
        let mut config = dry_run_config();
        config.article_size = article_size;
        config.par2 = redundancy_pct;
        let (sz, slices, _) = par2_geometry_from_sizes(&sizes, &config);
        (sz, slices)
    }

    #[test]
    fn optimal_slice_single_file_within_target() {
        // 500 articles with 10% redundancy: well within limits.
        let (sz, slices) = optimal_par2_slice_size(&[500], 750_000, 10);
        assert!(slices <= 32768);
        assert!((slices * 10 / 100) <= 65535);
        assert!(sz >= 64);
    }

    #[test]
    fn optimal_slice_no_redundancy_respects_32768_limit() {
        // 5000 files × 1 article: well within 32768, should satisfy the limit.
        let per_file = vec![1usize; 5_000];
        let (sz, slices) = optimal_par2_slice_size(&per_file, 100, 0);
        assert!(slices <= 32768, "slices={slices}");
        assert!(sz >= 100);
    }

    #[test]
    fn optimal_slice_too_many_files_returns_best_effort() {
        // 50 000 files × 1 article each: minimum possible is 50 000 slices > 32 768.
        // The function must not panic and should return the minimum achievable.
        let per_file = vec![1usize; 50_000];
        let (_sz, slices) = optimal_par2_slice_size(&per_file, 100, 0);
        assert!(slices >= 50_000, "slices={slices}");
    }

    #[test]
    fn optimal_slice_high_redundancy_respects_65535_recovery_limit() {
        // 200% redundancy: max input slices = 65535 * 100 / 200 = 32767.
        // 100 files × 400 articles each = 40 000 total articles.
        // Grouping can reduce to ~1000 slices, well within 32767.
        let per_file = vec![400usize; 100];
        let (sz, slices) = optimal_par2_slice_size(&per_file, 100, 200);
        let recovery = slices * 200 / 100;
        assert!(slices <= 32767, "slices={slices}");
        assert!(recovery <= 65535, "recovery={recovery}");
        assert!(sz >= 100);
    }

    #[test]
    fn optimal_slice_mixed_sizes() {
        // One large file (10 000 articles) and many tiny files (1 article each).
        let mut per_file = vec![1usize; 5_000];
        per_file.push(10_000);
        let (sz, slices) = optimal_par2_slice_size(&per_file, 750_000, 10);
        assert!(slices <= 32768, "slices={slices}");
        assert!((slices * 10 / 100) <= 65535);
        assert!(sz >= 64);
    }

    #[test]
    fn many_small_files_do_not_inflate_slice_to_article_groups() {
        // The many-small corpus: 2000 × 256 KiB files, 768 KiB articles.
        // Grouping *articles* as if they could be merged across files used to
        // pick a 3 MiB slice (4 articles) and still emit 2000 slices — 12×
        // padding. Slice size must stay near the file size.
        let file_size = 256 * 1024u64;
        let sizes = vec![file_size; 2000];
        let mut config = dry_run_config();
        config.article_size = 768_000;
        config.par2 = 10;
        let (slice_size, slices, recovery) = par2_geometry_from_sizes(&sizes, &config);
        assert_eq!(slices, 2000, "slices={slices}");
        assert!(
            slice_size <= file_size as usize,
            "slice_size={slice_size} padded each 256 KiB file"
        );
        let padded = slices * slice_size;
        let actual = 2000 * file_size as usize;
        assert!(
            padded as f64 / actual as f64 <= 1.15,
            "padding {} / {actual}",
            padded
        );
        assert_eq!(recovery, 200);
    }

    #[test]
    fn optimal_slice_empty_input() {
        let (sz, slices) = optimal_par2_slice_size(&[], 750_000, 10);
        assert_eq!(slices, 0);
        assert_eq!(sz, 750_000);
    }

    #[test]
    fn optimal_slice_single_article() {
        let (_sz, slices) = optimal_par2_slice_size(&[1], 750_000, 5);
        assert!(slices >= 1);
    }

    // ── resolve_date ──────────────────────────────────────────────────────────

    #[test]
    fn resolve_date_none_omits_header() {
        assert_eq!(resolve_date(None), (None, None));
    }

    #[test]
    fn resolve_date_now_returns_rfc2822() {
        let (d, ts) = resolve_date(Some("now"));
        let d = d.unwrap();
        // Should look like "Mon, 01 Jan 2024 00:00:00 +0000".
        assert!(d.ends_with("+0000"));
        assert!(d.contains(':'));
        assert!(ts.unwrap() > 0);
    }

    #[test]
    fn resolve_date_random_returns_rfc2822() {
        let (d, ts) = resolve_date(Some("random"));
        let d = d.unwrap();
        assert!(d.ends_with("+0000"));
        assert!(ts.unwrap() > 0);
    }

    #[test]
    fn resolve_date_random_within_2h() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (_, ts) = resolve_date(Some("random"));
        let ts = ts.unwrap();
        assert!(ts <= now, "random date must not be in the future");
        assert!(
            now - ts < 2 * 3600 + 1,
            "random date must be within the last 2 hours"
        );
    }

    #[test]
    fn resolve_date_fixed_is_returned_verbatim() {
        let fixed = "Tue, 14 Jan 2025 10:00:00 +0000";
        let (d, ts) = resolve_date(Some(fixed));
        assert_eq!(d.as_deref(), Some(fixed));
        assert!(ts.is_none());
    }

    // ── RateLimiter ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn rate_limiter_zero_rate_never_sleeps() {
        let mut rl = RateLimiter::new(0);
        let start = Instant::now();
        rl.acquire(1_000_000).await;
        // Should return almost instantly (< 10 ms).
        assert!(start.elapsed() < Duration::from_millis(10));
    }

    #[tokio::test]
    async fn rate_limiter_large_bucket_does_not_sleep_for_small_request() {
        // 10 MiB/s bucket, request 1 KiB — tokens are available immediately.
        let mut rl = RateLimiter::new(10 * 1024 * 1024);
        let start = Instant::now();
        rl.acquire(1024).await;
        assert!(start.elapsed() < Duration::from_millis(10));
    }

    // ── dry-run integration ───────────────────────────────────────────────────

    fn dry_run_config() -> Config {
        let mut file = FileConfig::default();
        file.posting.groups = Some(vec!["alt.test".into()]);
        Config::resolve(
            file,
            Overrides {
                dry_run: Some(true),
                par2: Some(0),
                ..Default::default()
            },
        )
        .unwrap()
    }

    // ── automatic recovery threshold (is_cheap_to_recover) ────────────────────

    fn recover_config(check_recover_percent: u8, check_recover_max: usize) -> Config {
        let mut config = dry_run_config();
        config.check_recover_percent = check_recover_percent;
        config.check_recover_max = check_recover_max;
        config
    }

    #[test]
    fn small_release_within_both_caps_is_cheap() {
        let config = recover_config(15, 50);
        // 3 missing out of 20 (15%) — right at the percent cap, under the max.
        assert!(is_cheap_to_recover(3, 20, &config));
    }

    #[test]
    fn huge_release_capped_by_absolute_max_even_under_percent() {
        let config = recover_config(15, 50);
        // 15% of 100,000 is 15,000 — nowhere near cheap, even though it's
        // exactly the configured percentage.
        assert!(!is_cheap_to_recover(15_000, 100_000, &config));
    }

    #[test]
    fn small_absolute_count_rejected_when_it_is_most_of_the_release() {
        let config = recover_config(15, 50);
        // 5 missing out of 10 (50%) — small in absolute terms, but a large
        // fraction of a tiny release looks systemic, not incidental.
        assert!(!is_cheap_to_recover(5, 10, &config));
    }

    #[test]
    fn zero_missing_is_never_worth_recovering() {
        let config = recover_config(15, 50);
        assert!(!is_cheap_to_recover(0, 1000, &config));
    }

    #[test]
    fn max_zero_disables_recovery_entirely() {
        let config = recover_config(100, 0);
        assert!(!is_cheap_to_recover(1, 2, &config));
    }

    // ── connection splitting (upload vs. streaming check) ─────────────────────

    #[test]
    fn split_connections_carves_auto_check_pool_out_of_the_total() {
        let mut config = dry_run_config();
        config.connections = 50;
        config.check_connections = 0; // auto
        let (check, upload) = split_connections(&config, true);
        assert_eq!(check, 4);
        assert_eq!(upload, 46);
        assert_eq!(check + upload, 50);
    }

    #[test]
    fn split_connections_disabled_uses_the_whole_total_for_upload() {
        let mut config = dry_run_config();
        config.connections = 50;
        let (check, upload) = split_connections(&config, false);
        assert_eq!(check, 0);
        assert_eq!(upload, 50);
    }

    #[test]
    fn split_connections_never_starves_upload_of_its_last_connection() {
        let mut config = dry_run_config();
        config.connections = 1;
        config.check_connections = 0; // auto
        let (check, upload) = split_connections(&config, true);
        assert_eq!(check, 0, "no connection left to spare for checking");
        assert_eq!(upload, 1);
    }

    #[test]
    fn split_connections_explicit_check_connections_is_additive() {
        let mut config = dry_run_config();
        config.connections = 1;
        config.check_connections = 1; // explicit, deliberate
        let (check, upload) = split_connections(&config, true);
        assert_eq!(check, 1);
        assert_eq!(
            upload, 1,
            "explicit --check-connections must not shrink upload"
        );
    }

    #[test]
    fn split_connections_small_total_leaves_upload_at_least_one() {
        let mut config = dry_run_config();
        config.connections = 2;
        config.check_connections = 0; // auto
        let (check, upload) = split_connections(&config, true);
        assert_eq!(check, 1);
        assert_eq!(upload, 1);
    }

    #[test]
    fn split_connections_low_max_favors_upload_not_check() {
        // Regression guard: a flat "up to 4" auto check pool used to try to
        // reserve 3 out of a 4-connection total for checking, leaving
        // upload — the operation that actually matters — with just 1. The
        // auto pool must scale down with the total instead of staying flat.
        let mut config = dry_run_config();
        config.connections = 4;
        config.check_connections = 0; // auto
        let (check, upload) = split_connections(&config, true);
        assert_eq!(check, 1);
        assert_eq!(upload, 3);
    }

    #[test]
    fn split_connections_high_max_caps_the_auto_check_pool() {
        let mut config = dry_run_config();
        config.connections = 200;
        config.check_connections = 0; // auto
        let (check, upload) = split_connections(&config, true);
        assert_eq!(check, 4);
        assert_eq!(upload, 196);
    }

    #[tokio::test]
    async fn dry_run_produces_segments_without_network() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("sample.bin");
        std::fs::write(&f, vec![0u8; 1500]).unwrap();

        let files = vec![InputFile {
            path: f.clone(),
            name: "sample.bin".into(),
        }];

        let config = dry_run_config();
        let outcome = post_files(&config, &files).await.unwrap();

        // Two segments (1500 bytes / 768 000 default = 1 here, but article_size
        // default is 768 000 so 1500 bytes → 1 segment).
        assert!(!outcome.segments.is_empty());
        assert!(outcome.failures.is_empty());
        assert!(!outcome.cancelled);
        assert_eq!(outcome.segments[0].file_name, "sample.bin");
    }

    #[tokio::test]
    async fn dry_run_multi_segment_file() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("big.bin");
        // Use a tiny article_size to force multiple segments.
        std::fs::write(&f, vec![0u8; 300]).unwrap();

        let files = vec![InputFile {
            path: f,
            name: "big.bin".into(),
        }];

        let mut config = dry_run_config();
        config.article_size = 100;
        let outcome = post_files(&config, &files).await.unwrap();

        // 300 bytes / 100 = 3 segments.
        assert_eq!(outcome.segments.len(), 3);
        for (i, seg) in outcome.segments.iter().enumerate() {
            assert_eq!(seg.part, (i + 1) as u32);
            assert_eq!(seg.total, 3);
        }
    }

    // ── par2_output_dir ───────────────────────────────────────────────────────

    fn meta_with_name(path: &std::path::Path, name: &str) -> FileMeta {
        FileMeta {
            path: path.to_path_buf(),
            real_name: name.into(),
            subject_name: name.into(),
            yenc_name: name.into(),
            from: String::new(),
            date: (None, None),
            size: 0,
            file_index: 0,
        }
    }

    #[test]
    fn par2_output_dir_loose_file_is_parent_dir() {
        // A single-component name like "movie.mkv" lives directly next to the file.
        let path = std::path::PathBuf::from("/data/movie.mkv");
        let meta = meta_with_name(&path, "movie.mkv");
        assert_eq!(par2_output_dir(&meta), std::path::Path::new("/data"));
    }

    #[test]
    fn par2_output_dir_nested_file_strips_depth() {
        // "Season01/ep01.mkv" has depth 2, so par2 dir is 2 levels up.
        let path = std::path::PathBuf::from("/data/Season01/ep01.mkv");
        let meta = meta_with_name(&path, "Season01/ep01.mkv");
        assert_eq!(par2_output_dir(&meta), std::path::Path::new("/data"));
    }

    #[test]
    fn par2_output_dir_three_levels_deep() {
        let path = std::path::PathBuf::from("/srv/a/b/c.bin");
        let meta = meta_with_name(&path, "a/b/c.bin");
        assert_eq!(par2_output_dir(&meta), std::path::Path::new("/srv"));
    }

    // ── pick_post_group ───────────────────────────────────────────────────────

    #[test]
    fn pick_post_group_empty_is_empty() {
        assert!(pick_post_group(&[]).is_empty());
    }

    #[test]
    fn pick_post_group_single_returns_that_group() {
        let groups = vec!["alt.binaries.test".to_string()];
        assert_eq!(pick_post_group(&groups), groups);
    }

    #[test]
    fn pick_post_group_picks_one_member_of_the_list() {
        let groups = vec![
            "alt.binaries.a".to_string(),
            "alt.binaries.b".to_string(),
            "alt.binaries.c".to_string(),
        ];
        // Always a single group, and always one drawn from the configured list.
        for _ in 0..100 {
            let picked = pick_post_group(&groups);
            assert_eq!(picked.len(), 1);
            assert!(groups.contains(&picked[0]));
        }
    }

    #[test]
    fn pick_post_group_splits_single_entry_cross_post_target() {
        let groups = vec!["alt.binaries.a+alt.binaries.b".to_string()];
        assert_eq!(
            pick_post_group(&groups),
            vec!["alt.binaries.a".to_string(), "alt.binaries.b".to_string()]
        );
    }

    #[test]
    fn pick_post_group_trims_whitespace_around_plus() {
        let groups = vec!["alt.binaries.a  +  alt.binaries.b".to_string()];
        assert_eq!(
            pick_post_group(&groups),
            vec!["alt.binaries.a".to_string(), "alt.binaries.b".to_string()]
        );
    }

    #[test]
    fn pick_post_group_splits_the_chosen_target_from_a_pool() {
        let groups = vec![
            "alt.binaries.a+alt.binaries.b".to_string(),
            "alt.binaries.c".to_string(),
        ];
        for _ in 0..100 {
            let picked = pick_post_group(&groups);
            let picked_set: Vec<&str> = picked.iter().map(String::as_str).collect();
            assert!(
                picked_set == ["alt.binaries.a", "alt.binaries.b"]
                    || picked_set == ["alt.binaries.c"],
                "unexpected pick: {picked:?}"
            );
        }
    }

    // ── physical_core_count ───────────────────────────────────────────────────

    #[test]
    fn physical_core_count_is_at_least_one() {
        assert!(parmesan::physical_core_count() >= 1);
    }

    #[test]
    fn encode_concurrency_is_min_of_cores_and_connections() {
        assert_eq!(encode_concurrency(4, 8), 4);
        assert_eq!(encode_concurrency(4, 50), 4);
        assert_eq!(encode_concurrency(2, 8), 2);
        assert_eq!(encode_concurrency(1, 1), 1);
        assert_eq!(encode_concurrency(6, 8), 6);
        assert_eq!(encode_concurrency(16, 8), 8);
        assert_eq!(encode_concurrency(6, 2), 2);
    }

    #[test]
    fn ready_queue_matches_nyuu_article_buffer() {
        assert_eq!(ready_queue_depth(8), 6);
        assert_eq!(ready_queue_depth(50), 25);
        assert_eq!(ready_queue_depth(1), 4);
    }

    // ── Shared buffer pool ────────────────────────────────────────────────────

    fn minimal_shared(article_size: usize) -> Arc<Shared> {
        use crate::config::{FileConfig, Overrides};
        let mut file = FileConfig::default();
        file.posting.groups = Some(vec!["alt.test".into()]);
        let mut config = Config::resolve(
            file,
            Overrides {
                dry_run: Some(true),
                par2: Some(0),
                ..Default::default()
            },
        )
        .unwrap();
        config.article_size = article_size;
        let post_group = pick_post_group(&config.groups);
        Arc::new(Shared {
            config,
            servers: Arc::new(vec![]),
            results: Arc::new(Mutex::new(Vec::new())),
            failures: Mutex::new(Vec::new()),
            failed_tasks: Mutex::new(Vec::new()),
            events: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            paused: Arc::new(AtomicBool::new(false)),
            resume: None,
            resume_path: None,
            spool_dir: None,
            pool: Arc::new(Mutex::new(Vec::new())),
            encode_pool: Arc::new(Mutex::new(Vec::new())),
            total_retries: std::sync::atomic::AtomicUsize::new(0),
            post_group,
            release_prefix: None,
            release_from: None,
            run_id: 0,
            total_files: 0,
        })
    }

    #[test]
    fn buffer_pool_reuses_released_buffer() {
        let shared = minimal_shared(1024);
        let buf = shared.try_acquire_buffer(1024).unwrap();
        let cap = buf.capacity();
        shared.release_buffer(buf);
        let buf2 = shared.try_acquire_buffer(1024).unwrap();
        // Reused buffer has at least the same capacity as the released one.
        assert!(buf2.capacity() >= cap);
        assert_eq!(buf2.len(), 1024);
    }

    #[test]
    fn buffer_pool_drops_oversized_buffers() {
        // article_size = 100; a buffer with capacity > 200 must not be pooled.
        let shared = minimal_shared(100);
        let big = vec![0u8; 300]; // capacity >> article_size * 2
        shared.release_buffer(big);
        // Pool should be empty — allocates fresh on next acquire.
        assert!(shared.pool.lock().unwrap().is_empty());
    }

    #[test]
    fn buffer_pool_acquire_fresh_when_empty() {
        let shared = minimal_shared(512);
        let buf = shared.try_acquire_buffer(256).unwrap();
        assert_eq!(buf.len(), 256);
    }

    // ── record_failure ────────────────────────────────────────────────────────

    #[test]
    fn record_failure_appends_description() {
        let shared = minimal_shared(1024);
        let path = std::path::PathBuf::from("ep.mkv");
        let meta = meta_with_name(&path, "ep.mkv");
        let task = PostTask {
            meta: Arc::new(meta),
            part: 2,
            total: 5,
            offset: 0,
            data: vec![],
            subject_name: "ep.mkv".into(),
            from: String::new(),
            date: (None, None),
            file_crc32: None,
        };
        record_failure(&shared, &task.meta, &task, "<mid@host>".into(), "timeout");
        let failures = shared.failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("ep.mkv"));
        assert!(failures[0].contains("2/5"));
        assert!(failures[0].contains("timeout"));
        // The original Message-ID is preserved for the same-ID end-of-run retry.
        let tasks = shared.failed_tasks.lock().unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].message_id, "<mid@host>");
    }

    // ── multi-file dry-run ordering ───────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_segments_sorted_by_filename_then_part() {
        let dir = TempDir::new().unwrap();
        let f1 = dir.path().join("b.bin");
        let f2 = dir.path().join("a.bin");
        std::fs::write(&f1, vec![0u8; 100]).unwrap();
        std::fs::write(&f2, vec![0u8; 100]).unwrap();

        let files = vec![
            InputFile {
                path: f1,
                name: "b.bin".into(),
            },
            InputFile {
                path: f2,
                name: "a.bin".into(),
            },
        ];

        let config = dry_run_config();
        let outcome = post_files(&config, &files).await.unwrap();

        let names: Vec<&str> = outcome
            .segments
            .iter()
            .map(|s| s.file_name.as_str())
            .collect();
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(names, sorted, "segments should be sorted by file name");
    }

    // ── obfuscation in dry-run ────────────────────────────────────────────────

    #[tokio::test]
    async fn dry_run_subject_obfuscation_hides_real_name_in_subject() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("secret.mkv");
        std::fs::write(&f, vec![0u8; 100]).unwrap();

        let files = vec![InputFile {
            path: f,
            name: "secret.mkv".into(),
        }];

        let mut file_cfg = crate::config::FileConfig::default();
        file_cfg.posting.groups = Some(vec!["alt.test".into()]);
        let config = Config::resolve(
            file_cfg,
            Overrides {
                dry_run: Some(true),
                par2: Some(0),
                obfuscate: Some(crate::config::ObfuscateMode::Full),
                ..Default::default()
            },
        )
        .unwrap();

        let outcome = post_files(&config, &files).await.unwrap();
        assert_eq!(outcome.segments.len(), 1);
        // NZB subject_name: for Full/Paranoid obfuscation with hash subjects, nzb_subject_name
        // returns the real file_name (secret.mkv) so download clients can rename correctly.
        assert_eq!(outcome.segments[0].file_name, "secret.mkv");
        assert_eq!(outcome.segments[0].subject_name.as_ref(), "secret.mkv");
    }

    #[tokio::test]
    async fn dry_run_ignores_resume_state_by_design() {
        // Resume is explicitly disabled in dry_run mode (post_files_with_progress
        // only creates resume state when `!config.dry_run && !config.par2_only`).
        // Segments get fresh Message-IDs even when a state file with recorded
        // entries is present.
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("r.bin");
        std::fs::write(&f, vec![0u8; 100]).unwrap();

        let state_path = dir.path().join("r.bin.pesto-state");
        let mut state = crate::resume::ResumeState::default();
        state.record("r.bin", 1, "<stored-id@pesto>", 100);
        state.save(&state_path).unwrap();

        let files = vec![InputFile {
            path: f,
            name: "r.bin".into(),
        }];

        let mut config = dry_run_config();
        config.resume = true; // resume flag set but dry_run overrides it

        let outcome = post_files_with_progress(&config, &files, None, Some(&state_path), None)
            .await
            .unwrap();

        // Segment is present but Message-ID is a fresh one, not the stored one.
        assert_eq!(outcome.segments.len(), 1);
        assert_ne!(outcome.segments[0].message_id, "<stored-id@pesto>");
    }
}
