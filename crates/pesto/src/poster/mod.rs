//! Parallel posting: the orchestration that ties together file reading, yEnc
//! encoding, article assembly and the NNTP client.
//!
//! Files are read sequentially by a single producer task which computes PAR2
//! parity concurrently. The producer feeds segments to a pool of worker tasks
//! via a bounded channel; workers yEnc-encode and post them. If the required
//! PAR2 recovery data exceeds a memory limit, the producer will make multiple
//! read passes over the files.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{debug, info, warn};

use crate::article::{
    default_subject, format_rfc2822, generate_message_id, obfuscated_name, rand_u64, random_from,
    Article,
};
use crate::config::{types::MAX_AUTO_PIPELINE_DEPTH, Config, ObfuscateMode};
use crate::nntp::pool::{ConnectionPool, ConnectionSlot};
use crate::progress::{FileEntry, ProgressEvent, ProgressSender, RunMode};
use crate::resume::ResumeState;
use crate::walk::InputFile;
use crate::yenc;
use parmesan::encoder::{FileHasher, FileHashes, RecoveryEncoder};
use parmesan::layout;
use parmesan::packet::{self, SliceChecksum};

use parmesan::worker::Par2Worker;

mod check;
use check::spawn_check_coordinator;

/// Returns `(slice_size_bytes, total_input_slices)`.
/// `file_size² / par2_slice_size`, so tying the PAR2 slice to the (small)
/// article size makes large files quadratically expensive. Several articles
/// are grouped into one PAR2 slice to keep the input-block count near this
/// target, which is the dominant lever on PAR2 CPU cost.
const TARGET_PAR2_SLICES: usize = 1000;

/// Returns `(slice_size_bytes, total_input_slices)`.
///
/// Finds the smallest `articles_per_slice` multiplier that satisfies both
/// PAR2 spec limits:
///   - total input blocks ≤ 32 768
///   - recovery blocks = floor(input_blocks × redundancy_pct / 100) ≤ 65 535
///
/// A performance target of ~[`TARGET_PAR2_SLICES`] is used as the starting
/// point; a binary search corrects upward when either limit is exceeded.
fn optimal_par2_slice_size(
    per_file_articles: &[usize],
    article_size: usize,
    redundancy_pct: u8,
) -> (usize, usize) {
    let total_articles: usize = per_file_articles.iter().sum();
    if total_articles == 0 {
        return (article_size, 0);
    }

    // Combined input-block limit from both PAR2 spec constraints.
    // floor(65535 * 100 / pct) is the max total_slices such that
    // floor(total * pct / 100) <= 65535.
    let max_input_slices = if redundancy_pct > 0 {
        (65535usize * 100 / redundancy_pct as usize).min(32768)
    } else {
        32768
    };

    let count_for = |a: usize| -> usize { per_file_articles.iter().map(|&n| n.div_ceil(a)).sum() };

    // Minimum achievable slices: one per non-empty file (when articles_per_slice
    // is large enough to cover each file in a single slice).
    let min_slices: usize = per_file_articles.iter().filter(|&&n| n > 0).count();
    if min_slices > max_input_slices {
        // Cannot satisfy the spec limit; group all articles and return best effort.
        return (total_articles * article_size, min_slices);
    }

    // Target ~2.5 % of total articles as input slices (divisor 40), capped at
    // 2000. Scaling at 10 % (divisor 10, the old behavior) produced tens of
    // thousands of slices on large uploads, making the GF(2^16) RS multiply
    // O(slices²) and killing encoder throughput.
    let target = (total_articles / 40)
        .clamp(TARGET_PAR2_SLICES, 2000)
        .min(max_input_slices);
    let initial_a = total_articles.div_ceil(target).max(1);

    if count_for(initial_a) <= max_input_slices {
        return (initial_a * article_size, count_for(initial_a));
    }

    // Binary search: find the minimum `a` such that count_for(a) <= max_input_slices.
    // Invariant: count_for(lo) > limit, count_for(hi) <= limit.
    // Upper bound: total_articles guarantees count_for = min_slices <= limit (checked above).
    let mut lo = initial_a;
    let mut hi = total_articles;

    while lo + 1 < hi {
        let mid = lo + (hi - lo) / 2;
        if count_for(mid) <= max_input_slices {
            hi = mid;
        } else {
            lo = mid;
        }
    }

    let a = hi;
    (a * article_size, count_for(a))
}

/// Compute the PAR2 recovery-set geometry `(slice_size_bytes,
/// total_input_slices, recovery_block_count)` that `producer` will use for
/// this batch of files, given the current config. Pure and cheap — only
/// reads file sizes already collected in `metas`, no I/O — so it can be
/// called before encoding actually starts to seed an exact (not estimated)
/// progress total. Mirrors the geometry logic in `producer` exactly; keep
/// the two in sync.
fn par2_geometry(metas: &[Arc<FileMeta>], config: &Config) -> (usize, usize, usize) {
    let article_size = config.article_size;
    let per_file_articles: Vec<usize> = metas
        .iter()
        .map(|m| {
            if m.size == 0 {
                0
            } else {
                yenc::segments(m.size, article_size).len()
            }
        })
        .collect();

    let (par2_slice_size, total_slices) = if let Some(size) = config.par2_slice_size {
        let s = (size / 64 * 64).max(64);
        let n: usize = metas.iter().map(|m| (m.size as usize).div_ceil(s)).sum();
        (s, n)
    } else if let Some(count) = config.par2_slice_count {
        let total_bytes: u64 = metas.iter().map(|m| m.size).sum();
        let s = ((total_bytes as usize).div_ceil(count.max(1)) / 64 * 64).max(64);
        let n: usize = metas.iter().map(|m| (m.size as usize).div_ceil(s)).sum();
        (s, n)
    } else {
        optimal_par2_slice_size(&per_file_articles, article_size, config.par2)
    };

    let recovery_count = if let Some(n) = config.par2_recovery_count {
        n
    } else {
        (total_slices * config.par2 as usize) / 100
    };

    (par2_slice_size, total_slices, recovery_count)
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
#[derive(Debug, Clone)]
pub struct PostedSegment {
    pub file_name: String,
    /// Absolute filesystem path of the source file, preserved so a post-check
    /// repost can re-read the segment regardless of the current working
    /// directory. `file_name` alone (the published/relative name) is
    /// insufficient — see `FailedTask::file_path` (issue #23), which this
    /// mirrors for the `--check` repost path.
    pub file_path: PathBuf,
    pub subject_name: String,
    pub file_size: u64,
    pub part: u32,
    pub total: u32,
    pub message_id: String,
    pub bytes: u64,
    pub from: String,
    /// Date header as `(rfc_string, unix_timestamp)`. Both parts are preserved
    /// so fixed dates survive round-trips and retries.
    pub date: (Option<String>, Option<u64>),
    /// CRC-32 of the whole file this segment belongs to. Only meaningful (and
    /// only ever emitted on the `=yend` line) when `part == total` — see
    /// `FileMeta::full_crc32`.
    pub full_crc32: u32,
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
    pub file_size: u64,
    pub part: u32,
    pub total: u32,
    pub from: String,
    /// Date header as `(rfc_string, unix_timestamp)`. Both are preserved so
    /// fixed dates (which have `Some` RFC but `None` timestamp) are not lost.
    pub date: (Option<String>, Option<u64>),
    /// CRC-32 of the whole file this segment belongs to — see
    /// `PostedSegment::full_crc32`.
    pub full_crc32: u32,
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
    /// Message-IDs that were posted (`240`) but never confirmed retrievable
    /// via the streaming STAT check, even after every repost attempt. Empty
    /// when `config.check` is disabled. A non-empty list means the run
    /// produced content that is not fully confirmed on the server.
    pub still_missing: Vec<String>,
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
    /// CRC-32 of the whole file, appended (as `crc32=`) to the `=yend` line
    /// of a multi-part file's *last* segment, alongside the per-part
    /// `pcrc32=` every segment already carries — see the yEnc draft §4 and
    /// `nyuu`'s `MultiEncoder` (`lib/article.js`), which always includes it.
    full_crc32: u32,
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
    /// Resume state shared among workers. `None` when resume is disabled.
    resume: Option<Arc<Mutex<ResumeState>>>,
    /// Path of the resume state file; `None` when resume is disabled.
    resume_path: Option<PathBuf>,
    /// Reusable article byte buffers (Phase 12b). Workers return their buffer
    /// here after encoding so the producer and reader tasks can reuse it
    /// instead of allocating a fresh `Vec<u8>` for every article.
    pool: Arc<Mutex<Vec<Vec<u8>>>>,
    /// Total number of post attempts that failed and triggered a retry (26d).
    total_retries: std::sync::atomic::AtomicUsize,
    /// Newsgroup(s) every article in this run is posted to. When several groups
    /// are configured one is picked at random once per run (see
    /// [`pick_post_group`]), so a whole upload stays together in a single group
    /// while the footprint spreads across groups over many runs.
    post_group: Vec<String>,
}

impl Shared {
    /// Take a buffer from the pool, or allocate a fresh one. The returned
    /// buffer is always exactly `size` bytes long (content is uninitialised).
    fn acquire_buffer(&self, size: usize) -> Vec<u8> {
        let mut pool = self.pool.lock().unwrap();
        match pool.pop() {
            Some(mut buf) => {
                buf.resize(size, 0);
                buf
            }
            None => vec![0u8; size],
        }
    }

    /// Return a buffer to the pool. Oversized or empty buffers are dropped.
    fn release_buffer(&self, buf: Vec<u8>) {
        if buf.capacity() > 0 && buf.capacity() <= self.config.article_size * 2 {
            self.pool.lock().unwrap().push(buf);
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
/// When `config.resume` is `true` and the path is `Some`, already-posted
/// segments are skipped and the state is updated after each successful post.
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
    configure_rayon(config.threads);

    let mut metas = Vec::with_capacity(files.len());
    for input in files {
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
                    let obfuscated = obfuscated_name();
                    (obfuscated.clone(), obfuscated, random_from())
                }
            }
        };
        let date = resolve_date(config.date.as_deref());
        let full_crc32 = compute_file_crc32(path).await?;
        metas.push(Arc::new(FileMeta {
            path: path.clone(),
            real_name,
            subject_name,
            yenc_name,
            from,
            date,
            size: md.len(),
            full_crc32,
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

    // Load resume state when enabled and a state path is provided.
    let (resume_arc, resume_path_owned) = if config.resume && !config.dry_run && !config.par2_only {
        if let Some(rp) = resume_state_path {
            let state = ResumeState::load(rp)?;
            if !state.is_empty() {
                eprintln!(
                    "resuming: {} segment(s) already posted, skipping",
                    state.len()
                );
            }
            (Some(Arc::new(Mutex::new(state))), Some(rp.to_path_buf()))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    // Pre-seed the buffer pool with enough buffers to keep all workers and the
    // double-buffer reader supplied without allocating during the hot path.
    let pool_size = worker_count + 4;
    let initial_pool: Vec<Vec<u8>> = (0..pool_size)
        .map(|_| vec![0u8; config.article_size])
        .collect();

    let shared = Arc::new(Shared {
        config: config.clone(),
        servers,

        results: Arc::new(Mutex::new(Vec::new())),
        failures: Mutex::new(Vec::new()),
        failed_tasks: Mutex::new(Vec::new()),
        events,
        cancelled: Arc::new(AtomicBool::new(false)),
        resume: resume_arc,
        resume_path: resume_path_owned,
        pool: Arc::new(Mutex::new(initial_pool)),
        total_retries: std::sync::atomic::AtomicUsize::new(0),
        post_group: pick_post_group(&config.groups),
    });

    // Announce the work plan: one `FileEntry` per source file, with the
    // segment count posting will use. PAR2 files are added later, once the
    // data pass has computed them, via `ProgressEvent::QueueExtended`.
    let (mode, target) = if config.par2_only {
        (RunMode::Par2Only, None)
    } else if config.dry_run {
        (RunMode::DryRun, None)
    } else {
        (
            RunMode::Post,
            Some(format!("{}:{}", config.host, config.port)),
        )
    };
    let _ = &target; // used below
                     // Exact PAR2 recovery-set geometry, computed with the same formula
                     // `producer` will actually use — not an estimate. This lets the total
                     // segment/byte counts be seeded correctly up front instead of jumping
                     // once PAR2 encoding finishes and its volumes get queued for posting.
    let (par2_bytes_hint, par2_segments_hint) =
        if config.par2 > 0 && !config.par2_only && !config.dry_run {
            let (slice_size, _total_slices, recovery_count) = par2_geometry(&metas, config);
            let recovery_bytes = recovery_count as u64 * slice_size as u64;
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
            if let Some(ref flag) = external_cancel {
                loop {
                    if flag.load(Ordering::Relaxed) {
                        break;
                    }
                    tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
                }
                shared.cancelled.store(true, Ordering::Relaxed);
                shared.emit(ProgressEvent::Interrupted);
            } else {
                std::future::pending::<()>().await;
            }
        })
    };

    // Streaming check: every segment that gets a clean `240` is queued here
    // and STAT-checked a few seconds later, concurrently with the rest of
    // the upload, instead of waiting for the whole run to finish.
    let check_coordinator = if check_enabled && check_conns > 0 {
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

    let t_post_start = std::time::Instant::now();
    let mut handles = Vec::with_capacity(worker_count);
    let tx_opt = if worker_count > 0 {
        let (tx, rx) = tokio::sync::mpsc::channel(worker_count * 2);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let pool = ConnectionPool::build(shared.servers.clone(), worker_count);
        for (idx, slot) in pool.into_slots().into_iter().enumerate() {
            handles.push(tokio::spawn(worker(
                shared.clone(),
                rx.clone(),
                idx,
                slot,
                check_tx.clone(),
            )));
        }
        Some(tx)
    } else {
        None
    };

    // Producer runs in this thread
    if let Err(e) = producer(metas, tx_opt, shared.clone()).await {
        shared.cancelled.store(true, Ordering::Relaxed);
        shared.emit(ProgressEvent::Failed {
            description: format!("producer error: {e:#}"),
        });
    }

    for handle in handles {
        let _ = handle.await;
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
    let still_missing = if let Some(coordinator) = check_coordinator {
        coordinator.finish_and_drain().await
    } else {
        Vec::new()
    };

    shared.emit(ProgressEvent::Finished);

    let mut segments = std::mem::take(&mut *shared.results.lock().unwrap());
    segments.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.part.cmp(&b.part)));

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

    Ok(PostOutcome {
        segments,
        failures,
        failed_tasks,
        cancelled,
        groups: shared.post_group.clone(),
        still_missing,
    })
}

/// Per-process temp directory holding the intermediate PAR2 files written
/// during a normal posting run. Callers should remove it (when
/// `!config.par2_only`) once the *entire* run is done — including any
/// `--check` repost pass or end-of-run failed-task retry — not right after
/// the main post loop finishes, since both of those may still need to
/// re-read a PAR2 file's bytes from disk.
pub fn par2_temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("parmesan_{}", std::process::id()))
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
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
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
) {
    if accum.len() == par2_slice_size {
        // Zero-copy optimization for the common case (slice size matches accumulation).
        let next = worker.take_buffer(par2_slice_size);
        let padded = std::mem::replace(accum, next);
        tokio::task::block_in_place(|| worker.send_slice(padded, par2_slice_size, is_last_of_file));
    } else if accum.len() > par2_slice_size {
        // Splitting case (manual slice size < article size): take exactly one slice.
        let mut slice_buf = worker.take_buffer(par2_slice_size);
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
/// Emits `SegmentDone` events in `article_size` increments so the progress bar
/// advances at the same cadence as the standard path.
async fn par2_only_ingest(
    metas: &[Arc<FileMeta>],
    worker: &Par2Worker,
    par2_slice_size: usize,
    article_size: usize,
    total_slices: usize,
    par2_slices_fed: &mut usize,
    shared: &Arc<Shared>,
) -> Result<()> {
    for meta in metas {
        if shared.cancelled.load(Ordering::Relaxed) {
            return Ok(());
        }

        // Empty files contribute zero PAR2 input slices.
        // Hash alignment is maintained by the caller (which inserts a known
        // empty-file hash entry for any meta with size == 0 in final_hashes).
        if meta.size == 0 {
            continue;
        }

        let mut file = File::open(&meta.path)
            .await
            .with_context(|| format!("opening `{}`", meta.path.display()))?;

        let mut slice_buf = worker.take_buffer(par2_slice_size);
        slice_buf.clear();

        let mut remaining = meta.size as usize;
        let mut credited: usize = 0; // bytes emitted via SegmentDone so far

        while remaining > 0 {
            if shared.cancelled.load(Ordering::Relaxed) {
                return Ok(());
            }

            let space = par2_slice_size - slice_buf.len();
            let to_read = space.min(remaining);

            // Read directly into spare capacity, bypassing the zero-init that
            // `resize` would impose. Using spare capacity avoids writing 10 GiB
            // of zeros (one full pass over memory per 5 GiB input) that would
            // otherwise evict RS recovery buffers from LLC.
            //
            // SAFETY: `read_exact` either fills every byte in the slice or
            // returns an error. `set_len` is only reached on success, so no
            // byte is ever observed uninitialised.
            let base = slice_buf.len();
            slice_buf.reserve(to_read);
            let dst = unsafe {
                std::slice::from_raw_parts_mut(slice_buf.as_mut_ptr().add(base), to_read)
            };
            file.read_exact(dst)
                .await
                .with_context(|| format!("reading `{}`", meta.path.display()))?;
            unsafe { slice_buf.set_len(base + to_read) };
            remaining -= to_read;

            // Emit SegmentDone for each complete article worth of bytes consumed.
            let consumed = meta.size as usize - remaining;
            while credited + article_size <= consumed {
                shared.emit(ProgressEvent::SegmentDone {
                    file: meta.real_name.clone(),
                    bytes: article_size as u64,
                    ok: true,
                });
                credited += article_size;
            }

            if slice_buf.len() >= par2_slice_size {
                let is_last = remaining == 0;
                feed_par2_slice(&mut slice_buf, par2_slice_size, worker, is_last);
                *par2_slices_fed += 1;
                shared.emit(ProgressEvent::Par2InputProgress {
                    done: *par2_slices_fed,
                    total: total_slices,
                });
            }
        }

        // Credit the last partial article of this file.
        let leftover = meta.size as usize - credited;
        if leftover > 0 {
            shared.emit(ProgressEvent::SegmentDone {
                file: meta.real_name.clone(),
                bytes: leftover as u64,
                ok: true,
            });
        }

        // Flush the final partial slice for this file (zero-padded inside
        // feed_par2_slice).
        if !slice_buf.is_empty() {
            feed_par2_slice(&mut slice_buf, par2_slice_size, worker, true);
            *par2_slices_fed += 1;
            shared.emit(ProgressEvent::Par2InputProgress {
                done: *par2_slices_fed,
                total: total_slices,
            });
        }
    }

    Ok(())
}

fn par2_base(name: &str) -> &str {
    name.split('/').next().unwrap_or(name)
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

async fn producer(
    metas: Vec<Arc<FileMeta>>,
    tx_opt: Option<tokio::sync::mpsc::Sender<PostTask>>,
    shared: Arc<Shared>,
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

    // Choose the PAR2 slice size: groups consecutive articles into larger slices
    // to keep input-block count near TARGET_PAR2_SLICES while satisfying both
    // PAR2 spec limits (32 768 input blocks, 65 535 recovery blocks). Same
    // geometry `par2_geometry` already computed to seed the progress totals
    // at `Started` — kept in sync by sharing this one function.
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

    // Auto-detect safe RAM limit if not specified (70% of available RAM)
    let memory_limit = match shared.config.par2_memory_limit {
        Some(limit) => limit,
        None => {
            let mut sys = sysinfo::System::new();
            sys.refresh_memory();
            let available_ram = sys.available_memory();
            let safe_limit = (available_ram as f64 * 0.70) as usize;

            // At least 256MB as a bare minimum fallback
            safe_limit.max(256 * 1024 * 1024)
        }
    };

    let slices_per_pass = (memory_limit / par2_slice_size).max(1);

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

    if passes.len() > 1 {
        shared.emit(crate::progress::ProgressEvent::Status {
            text: format!(
                "PAR2 recovery data split into {} passes (memory limit: {} MiB)",
                passes.len(),
                memory_limit / (1024 * 1024),
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
                RecoveryEncoder::new_smart(par2_slice_size, total_slices, exp_start, rec_count);
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
            Some(Par2Worker::spawn(enc, pass_idx == 0))
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

                // Double-buffered reader task (Phase 12a): reads articles from
                // disk into a bounded channel of capacity 2. This lets the OS
                // begin fetching article N+1 while the producer is processing
                // article N (PAR2 accumulation, channel send, or block_in_place).
                let (read_tx, mut read_rx) = tokio::sync::mpsc::channel::<(u64, Vec<u8>)>(2);

                let reader_path = meta.path.clone();
                let reader_shared = shared.clone();
                let reader_segs = segments.clone();
                let reader_handle = tokio::spawn(async move {
                    let mut file = File::open(&reader_path).await?;
                    for (offset, len) in reader_segs {
                        // Phase 12b: acquire a buffer from the shared pool if
                        // available, otherwise allocate. Workers return buffers
                        // to the same pool after yEnc encoding.
                        let buf = reader_shared.acquire_buffer(len);
                        let mut buf = buf;
                        file.read_exact(&mut buf).await?;
                        if read_tx.send((offset, buf)).await.is_err() {
                            break; // producer dropped its end (cancelled)
                        }
                    }
                    Ok::<_, anyhow::Error>(())
                });

                // Real bytes of the PAR2 input slice currently being assembled.
                // Source the buffer from the worker's recycled-buffer pool so
                // subsequent files reuse allocations from earlier flushes.
                let mut par2_accum: Vec<u8> = match worker_opt.as_ref() {
                    Some(w) => w.take_buffer(par2_slice_size),
                    None => Vec::new(),
                };

                let mut i: u32 = 0;
                while let Some((offset, buf)) = read_rx.recv().await {
                    if shared.cancelled.load(Ordering::Relaxed) {
                        drop(read_rx);
                        let _ = reader_handle.await;
                        return Ok(());
                    }

                    // PAR2 work is gated on the worker being active.
                    if let Some(worker) = &worker_opt {
                        // Append the article to the current PAR2 slice.
                        par2_accum.extend_from_slice(&buf);
                        while par2_accum.len() >= par2_slice_size {
                            feed_par2_slice(&mut par2_accum, par2_slice_size, worker, false);
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
                                    &shared.config,
                                ))
                                .await
                                .is_err()
                            {
                                drop(read_rx);
                                let _ = reader_handle.await;
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

                let _ = reader_handle.await?;

                // Flush the file's final, partial PAR2 slice (zero-padded).
                if let Some(worker) = &worker_opt {
                    if !par2_accum.is_empty() {
                        feed_par2_slice(&mut par2_accum, par2_slice_size, worker, true);
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
                let articles_per_slice = par2_slice_size / article_size;
                let mut cs_iter = slice_checksums.into_iter();
                for (file_idx, &articles) in per_file_articles.iter().enumerate() {
                    let file_slices = articles.div_ceil(articles_per_slice);
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
                    par2_dir = Some(par2_temp_dir());
                    tokio::fs::create_dir_all(par2_dir.as_ref().unwrap()).await?;
                }

                let index_name = layout::index_name(par2_base(&metas[0].real_name));
                let index_path = par2_dir.as_ref().unwrap().join(&index_name);
                tokio::fs::write(&index_path, &base_packets).await?;
                if let Some(tx) = &tx_opt {
                    push_par2_file(&index_path, index_name, &shared, tx).await?;
                }
            }

            let t_par2_write = std::time::Instant::now();
            let volumes = layout::plan_volumes(recovery_count as u32);
            for slice in recovery_slices {
                let vol = volumes
                    .iter()
                    .find(|v| slice.exponent >= v.first && slice.exponent < v.first + v.count)
                    .unwrap();
                let vol_name = layout::volume_name(par2_base(&metas[0].real_name), *vol);
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
                        push_par2_file(&vol_path, vol_name, &shared, tx).await?;
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

async fn push_par2_file(
    path: &PathBuf,
    real_name: String,
    shared: &Arc<Shared>,
    tx: &tokio::sync::mpsc::Sender<PostTask>,
) -> Result<()> {
    let size = tokio::fs::metadata(path).await?.len();
    let segments = yenc::segments(size, shared.config.article_size);
    let total = segments.len() as u32;

    shared.emit(ProgressEvent::QueueExtended {
        file: real_name.clone(),
        segments: total as u64,
        bytes: size,
    });

    let (subject_name, yenc_name, from) = match shared.config.obfuscate {
        ObfuscateMode::None => {
            let wn = wire_name(&real_name).to_string();
            (wn.clone(), wn, shared.config.from.clone())
        }
        ObfuscateMode::Full | ObfuscateMode::Paranoid => {
            let obfuscated = obfuscated_name();
            (obfuscated.clone(), obfuscated, random_from())
        }
    };
    let date = resolve_date(shared.config.date.as_deref());
    let full_crc32 = compute_file_crc32(path).await?;

    let meta = Arc::new(FileMeta {
        path: path.clone(),
        real_name,
        subject_name,
        yenc_name,
        from,
        date,
        size,
        full_crc32,
    });

    let mut file = tokio::fs::File::open(path).await?;
    for (i, (offset, len)) in segments.into_iter().enumerate() {
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).await?;
        if tx
            .send(make_task(
                meta.clone(),
                i as u32 + 1,
                total,
                offset,
                buf,
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

async fn worker(
    shared: Arc<Shared>,
    rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<PostTask>>>,
    conn_id: usize,
    mut slot: ConnectionSlot,
    check_tx: Option<tokio::sync::mpsc::UnboundedSender<PostedSegment>>,
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
    let mut last_used = Instant::now();

    loop {
        if shared.cancelled.load(Ordering::Relaxed) {
            break;
        }

        // Send keepalive if the connection has been idle past the configured
        // interval. This fires before competing for the receive lock so each
        // worker sends its own keepalive independently.
        if keepalive_enabled && last_used.elapsed() >= Duration::from_secs(keepalive_interval) {
            slot.keepalive().await;
            last_used = Instant::now();
        }

        // Blocking receive for the first task, with a short wakeup so the
        // keepalive check above can fire while the channel is empty.
        enum Recv {
            Task(PostTask),
            Idle,
            Closed,
        }
        let recv = {
            let mut rx_guard = rx.lock().await;
            tokio::select! {
                task = rx_guard.recv() => match task {
                    Some(t) => Recv::Task(t),
                    None => Recv::Closed,
                },
                _ = tokio::time::sleep(IDLE_POLL), if keepalive_enabled => Recv::Idle,
            }
        };
        let first = match recv {
            Recv::Task(t) => {
                last_used = Instant::now();
                t
            }
            Recv::Closed => break,
            Recv::Idle => continue,
        };
        let mut batch = vec![first];

        // Non-blocking: try to fill the rest of the pipeline slot.
        if effective_depth > 1 {
            let mut rx = rx.lock().await;
            while batch.len() < effective_depth {
                match rx.try_recv() {
                    Ok(t) => batch.push(t),
                    Err(_) => break,
                }
            }
        }

        // Process each task in the batch. Tasks already in resume state are
        // resolved immediately without touching the network.
        //
        // `pending` collects tasks that still need to be posted, along with
        // their pre-computed headers and encoded bodies.
        struct Pending {
            task: PostTask,
            message_id: String,
            headers: Vec<u8>,
            encoded: yenc::EncodedPart,
            encode_time: Duration,
            /// Date header as (rfc_string, unix_timestamp) for this article.
            date: (Option<String>, Option<u64>),
        }
        let mut pending: Vec<Pending> = Vec::with_capacity(batch.len());

        for task in batch {
            shared.emit(ProgressEvent::ConnectionBusy {
                conn: conn_id,
                file: task.meta.real_name.clone(),
            });

            if let Some(resume) = &shared.resume {
                if let Some(existing_id) = resume
                    .lock()
                    .unwrap()
                    .get(&task.meta.real_name, task.part)
                    .map(str::to_string)
                {
                    shared.results.lock().unwrap().push(PostedSegment {
                        file_name: task.meta.real_name.clone(),
                        file_path: task.meta.path.clone(),
                        subject_name: task.subject_name.clone(),
                        file_size: task.meta.size,
                        part: task.part,
                        total: task.total,
                        message_id: existing_id,
                        bytes: 0,
                        from: task.from.clone(),
                        date: task.date.clone(),
                        full_crc32: task.meta.full_crc32,
                    });
                    let bytes = task.data.len() as u64;
                    shared.release_buffer(task.data);
                    shared.emit(ProgressEvent::SegmentDone {
                        file: task.meta.real_name.clone(),
                        bytes,
                        ok: true,
                    });
                    continue;
                }
            }

            let t_enc = Instant::now();
            let file_crc32 = (task.part == task.total).then_some(task.meta.full_crc32);
            let encoded = yenc::encode_part(
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
                subject: default_subject(&task.meta.subject_name, task.part, task.total),
                date: rfc_date.clone(),
                no_archive: shared.config.no_archive,
            };
            let headers = article.build_headers();
            let task_date = task.date.clone();
            pending.push(Pending {
                task,
                message_id,
                headers,
                encoded,
                encode_time,
                date: task_date,
            });
        }

        if pending.is_empty() {
            continue;
        }

        if shared.config.dry_run {
            for p in pending {
                shared.results.lock().unwrap().push(PostedSegment {
                    file_name: p.task.meta.real_name.clone(),
                    file_path: p.task.meta.path.clone(),
                    subject_name: p.task.subject_name.clone(),
                    file_size: p.task.meta.size,
                    part: p.task.part,
                    total: p.task.total,
                    message_id: p.message_id,
                    bytes: (p.headers.len() + p.encoded.body.len()) as u64,
                    from: p.task.from.clone(),
                    date: p.date.clone(),
                    full_crc32: p.task.meta.full_crc32,
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

            commit_result(
                &shared,
                check_tx.as_ref(),
                p.task,
                p.message_id,
                p.headers.len() + p.encoded.body.len(),
                posted,
                &last_err,
                p.date,
            );
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

            for (p, result) in pending.into_iter().zip(pipe_results) {
                let posted = pipeline_ok && result.is_ok();
                let last_err = result.err().unwrap_or_else(|| "pipeline failed".into());
                commit_result(
                    &shared,
                    check_tx.as_ref(),
                    p.task,
                    p.message_id,
                    p.headers.len() + p.encoded.body.len(),
                    posted,
                    &last_err,
                    p.date,
                );
            }
        }
    }

    shared.emit(ProgressEvent::ConnectionIdle { conn: conn_id });
    slot.quit().await;
}

/// Choose the newsgroup(s) for a whole run.
///
/// When several groups are configured, one is picked at random (once per run)
/// rather than cross-posting every article to all of them. The whole upload
/// then stays together in a single group, while the footprint still spreads
/// Build a `PostTask`, generating per-article subject and From when in
/// `ObfuscateMode::Paranoid`; otherwise copies them from `FileMeta`.
fn make_task(
    meta: Arc<FileMeta>,
    part: u32,
    total: u32,
    offset: u64,
    data: Vec<u8>,
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
    }
}

/// across the configured groups over many runs. With zero or one configured
/// group the slice is returned as-is.
fn pick_post_group(groups: &[String]) -> Vec<String> {
    match groups {
        [] | [_] => groups.to_vec(),
        _ => {
            let idx = (rand_u64() % groups.len() as u64) as usize;
            vec![groups[idx].clone()]
        }
    }
}

/// Compute the CRC-32 of an entire file, streamed in fixed-size chunks so
/// memory use stays flat regardless of file size.
///
/// Used once per file, up front, so every segment's `encode_part` call can
/// pass it for the file's *last* part — see [`FileMeta::full_crc32`].
async fn compute_file_crc32(path: &std::path::Path) -> Result<u32> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("opening `{}`", path.display()))?;
    let mut hasher = yenc::Crc32::new();
    let mut buf = vec![0u8; 1 << 20]; // 1 MiB chunks
    loop {
        let n = file
            .read(&mut buf)
            .await
            .with_context(|| format!("reading `{}`", path.display()))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hasher.finalize())
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
) {
    if posted {
        if let Some(resume) = &shared.resume {
            let mut state = resume.lock().unwrap();
            state.record(&task.meta.real_name, task.part, &message_id);
            if let Some(rp) = &shared.resume_path {
                let _ = state.save(rp);
            }
        }
        let seg = PostedSegment {
            file_name: task.meta.real_name.clone(),
            file_path: task.meta.path.clone(),
            subject_name: task.subject_name.clone(),
            file_size: task.meta.size,
            part: task.part,
            total: task.total,
            message_id,
            bytes: wire_bytes as u64,
            from: task.from.clone(),
            date,
            full_crc32: task.meta.full_crc32,
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
        file_size: meta.size,
        part: task.part,
        total: task.total,
        from: task.from.clone(),
        date: task.date.clone(),
        full_crc32: meta.full_crc32,
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
            &task.subject_name,
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
            subject: default_subject(&task.subject_name, task.part, task.total),
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
                file_path: task.file_path.clone(),
                subject_name: task.subject_name.clone(),
                file_size: task.file_size,
                part: task.part,
                total: task.total,
                message_id,
                bytes: wire_bytes,
                from: task.from.clone(),
                date: task.date.clone(),
                full_crc32: task.full_crc32,
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

    // ── optimal_par2_slice_size ───────────────────────────────────────────────

    #[test]
    fn optimal_slice_single_file_within_target() {
        // 500 articles with 10% redundancy: well within limits.
        let (sz, slices) = optimal_par2_slice_size(&[500], 750_000, 10);
        assert!(slices <= 32768);
        assert!((slices * 10 / 100) <= 65535);
        assert_eq!(
            sz % 750_000,
            0,
            "slice size must be a multiple of article_size"
        );
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
        assert_eq!(slices, 50_000, "slices={slices}");
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
        assert_eq!(sz % 750_000, 0);
    }

    #[test]
    fn optimal_slice_empty_input() {
        let (sz, slices) = optimal_par2_slice_size(&[], 750_000, 10);
        assert_eq!(slices, 0);
        assert_eq!(sz, 750_000);
    }

    #[test]
    fn optimal_slice_single_article() {
        let (sz, slices) = optimal_par2_slice_size(&[1], 750_000, 5);
        assert_eq!(slices, 1);
        assert_eq!(sz, 750_000);
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
            full_crc32: 0,
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

    // ── physical_core_count ───────────────────────────────────────────────────

    #[test]
    fn physical_core_count_is_at_least_one() {
        assert!(parmesan::physical_core_count() >= 1);
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
            resume: None,
            resume_path: None,
            pool: Arc::new(Mutex::new(Vec::new())),
            total_retries: std::sync::atomic::AtomicUsize::new(0),
            post_group,
        })
    }

    #[test]
    fn buffer_pool_reuses_released_buffer() {
        let shared = minimal_shared(1024);
        let buf = shared.acquire_buffer(1024);
        let cap = buf.capacity();
        shared.release_buffer(buf);
        let buf2 = shared.acquire_buffer(1024);
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
        let buf = shared.acquire_buffer(256);
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
        // file_name keeps the real name; subject_name is randomised.
        assert_eq!(outcome.segments[0].file_name, "secret.mkv");
        assert_ne!(outcome.segments[0].subject_name, "secret.mkv");
    }

    #[tokio::test]
    async fn dry_run_ignores_resume_state_by_design() {
        // Resume is explicitly disabled in dry_run mode (post_files_with_progress
        // checks `config.resume && !config.dry_run`). Segments get fresh
        // Message-IDs even when a state file with recorded entries is present.
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("r.bin");
        std::fs::write(&f, vec![0u8; 100]).unwrap();

        let state_path = dir.path().join("r.bin.pesto-state");
        let mut state = crate::resume::ResumeState::default();
        state.record("r.bin", 1, "<stored-id@pesto>");
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
