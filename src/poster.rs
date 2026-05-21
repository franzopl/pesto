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
    default_subject, format_rfc2822, generate_message_id, obfuscated_name, Article,
};
use crate::config::{Config, ObfuscateMode};
use crate::nntp::pool::{ConnectionPool, ConnectionSlot};
use crate::par2::encoder::{FileHasher, RecoveryEncoder};
use crate::par2::layout;
use crate::par2::packet::{self, SliceChecksum};
use crate::progress::{FileEntry, ProgressEvent, ProgressSender, RunMode};
use crate::resume::ResumeState;
use crate::walk::InputFile;
use crate::yenc;

/// Target number of PAR2 input slices. Reed-Solomon encoding cost grows with
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

/// A posted segment, retained for later `.nzb` generation.
#[derive(Debug, Clone)]
pub struct PostedSegment {
    pub file_name: String,
    pub subject_name: String,
    pub file_size: u64,
    pub part: u32,
    pub total: u32,
    pub message_id: String,
    pub bytes: u64,
}

/// The result of a posting run.
#[derive(Debug)]
pub struct PostOutcome {
    pub segments: Vec<PostedSegment>,
    pub failures: Vec<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
struct FileMeta {
    path: PathBuf,
    real_name: String,
    subject_name: String,
    yenc_name: String,
    size: u64,
}

struct PostTask {
    meta: Arc<FileMeta>,
    part: u32,
    total: u32,
    offset: u64,
    data: Vec<u8>,
}

struct Shared {
    config: Config,
    /// Server list in failover order (primary first).
    servers: Arc<Vec<crate::config::ServerEntry>>,

    results: Mutex<Vec<PostedSegment>>,
    failures: Mutex<Vec<String>>,
    /// Progress channel; `None` keeps the poster silent (library default).
    events: Option<ProgressSender>,
    cancelled: AtomicBool,
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
    post_files_with_progress(config, files, None, None).await
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
) -> Result<PostOutcome> {
    configure_rayon();

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
        let (subject_name, yenc_name) = match config.obfuscate {
            ObfuscateMode::None => (real_name.clone(), real_name.clone()),
            ObfuscateMode::Subject => (obfuscated_name(), real_name.clone()),
            ObfuscateMode::Full => {
                let obfuscated = obfuscated_name();
                (obfuscated.clone(), obfuscated)
            }
        };
        metas.push(Arc::new(FileMeta {
            path: path.clone(),
            real_name,
            subject_name,
            yenc_name,
            size: md.len(),
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
            let file_id = packet::compute_file_id(&md5_16k, meta.size, &meta.real_name);
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
        files = metas.len(),
        segments = initial_segments,
        article_size = config.article_size,
        par2_pct = config.par2,
        "upload plan"
    );

    let servers: Arc<Vec<crate::config::ServerEntry>> = Arc::new(config.all_servers().collect());
    let total_conns = config.total_connections();

    let worker_count = if config.par2_only {
        0
    } else {
        total_conns.max(1).min(initial_segments.max(1) as usize)
    };
    info!(
        workers = worker_count,
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

        results: Mutex::new(Vec::new()),
        failures: Mutex::new(Vec::new()),
        events,
        cancelled: AtomicBool::new(false),
        resume: resume_arc,
        resume_path: resume_path_owned,
        pool: Arc::new(Mutex::new(initial_pool)),
        total_retries: std::sync::atomic::AtomicUsize::new(0),
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
    let total_source_bytes: u64 = metas.iter().map(|m| m.size).sum();
    // Rough PAR2 size estimate: recovery data ≈ par2% of source bytes, plus
    // a small fixed overhead per file for PAR2 packet headers (~1 KiB/file).
    let par2_bytes_hint = if config.par2 > 0 && !config.par2_only && !config.dry_run {
        let data_est = total_source_bytes * config.par2 as u64 / 100;
        let header_est = metas.len() as u64 * 1024;
        data_est + header_est
    } else {
        0
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
    });

    let cancel_handle = {
        let shared = shared.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                shared.cancelled.store(true, Ordering::Relaxed);
                shared.emit(ProgressEvent::Interrupted);
            }
        })
    };

    let t_post_start = std::time::Instant::now();
    let mut handles = Vec::with_capacity(worker_count);
    let tx_opt = if worker_count > 0 {
        let (tx, rx) = tokio::sync::mpsc::channel(worker_count * 2);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        let pool = ConnectionPool::build(shared.servers.clone(), worker_count);
        for (idx, slot) in pool.into_slots().into_iter().enumerate() {
            handles.push(tokio::spawn(worker(shared.clone(), rx.clone(), idx, slot)));
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

    // The PAR2 files posted in normal mode are written to a per-process temp
    // directory purely as an intermediate; remove it once posting is done.
    // (`--par2-only` writes next to the source files and must be kept.)
    if !config.par2_only {
        let _ = tokio::fs::remove_dir_all(par2_temp_dir()).await;
    }

    cancel_handle.abort();
    shared.emit(ProgressEvent::Finished);

    let mut segments = std::mem::take(&mut *shared.results.lock().unwrap());
    segments.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.part.cmp(&b.part)));
    let failures = std::mem::take(&mut *shared.failures.lock().unwrap());
    let cancelled = shared.cancelled.load(Ordering::Relaxed);

    // 26d/26g — network performance summary + post phase timing
    let total_retries = shared.total_retries.load(Ordering::Relaxed);
    info!(
        posted = segments.len(),
        failed = failures.len(),
        retries = total_retries,
        elapsed_ms = t_post_start.elapsed().as_millis(),
        phase = "post",
        "network summary"
    );

    Ok(PostOutcome {
        segments,
        failures,
        cancelled,
    })
}

/// Per-process temp directory holding the intermediate PAR2 files written
/// during a normal posting run. Removed once posting finishes.
fn par2_temp_dir() -> PathBuf {
    std::env::temp_dir().join(format!("pesto_par2_{}", std::process::id()))
}

/// Restrict the global Rayon pool to physical cores. The PAR2 encoder is pure
/// SIMD/ALU work; sibling hyperthreads contend for the same execution ports
/// and add almost nothing, so one worker per logical CPU only heats the
/// machine. Called once; a no-op if a global pool already exists.
fn configure_rayon() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(performance_core_count())
            .build_global();
    });
}

/// Returns the name of the SIMD path that the PAR2 encoder will use at runtime.
fn detect_par2_simd() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx512f")
        && std::is_x86_feature_detected!("avx512bw")
        && std::is_x86_feature_detected!("gfni")
    {
        return "AVX-512/GFNI";
    }
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        return "AVX2";
    }
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("ssse3") {
        return "SSSE3";
    }
    #[cfg(target_arch = "aarch64")]
    {
        return "NEON";
    }
    "scalar"
}

/// Number of performance-class cores. On hybrid CPUs (Intel 12th gen and later)
/// the E-cores execute SIMD at lower throughput and stretch wall-clock when a
/// rayon partition is scheduled there; restricting the pool to P-cores measured
/// +2.4% on 5G PAR2 encoding. Detects hybrid layout via Linux topology:
/// P-cores expose two `thread_siblings_list` entries (HT pair), E-cores stand
/// alone. When the layout is mixed, return the P-core count (one per SMT pair);
/// otherwise fall back to [`physical_core_count`].
fn performance_core_count() -> usize {
    use std::collections::HashSet;

    let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
        return physical_core_count();
    };

    let mut paired_leaders: HashSet<usize> = HashSet::new();
    let mut solo: HashSet<usize> = HashSet::new();

    for entry in entries.flatten() {
        let name = entry.file_name();
        let name_s = name.to_string_lossy();
        let Some(cpu_num) = name_s
            .strip_prefix("cpu")
            .and_then(|s| s.parse::<usize>().ok())
        else {
            continue;
        };
        let sib_path = entry.path().join("topology/thread_siblings_list");
        let Ok(sib) = std::fs::read_to_string(&sib_path) else {
            continue;
        };
        let leader: usize = sib
            .trim()
            .split([',', '-'])
            .next()
            .and_then(|s| s.parse().ok())
            .unwrap_or(cpu_num);
        let count = sib.trim().split([',', '-']).count();
        if count >= 2 {
            paired_leaders.insert(leader);
        } else {
            solo.insert(cpu_num);
        }
    }

    if !paired_leaders.is_empty() && !solo.is_empty() {
        paired_leaders.len()
    } else {
        physical_core_count()
    }
}

/// Number of physical CPU cores, derived from `/proc/cpuinfo` by counting
/// distinct `(physical id, core id)` pairs. Falls back to the logical CPU
/// count when that information is unavailable.
fn physical_core_count() -> usize {
    use std::collections::HashSet;
    if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
        let mut seen: HashSet<(String, String)> = HashSet::new();
        let (mut phys, mut core): (Option<String>, Option<String>) = (None, None);
        for line in info.lines() {
            if line.trim().is_empty() {
                if let (Some(p), Some(c)) = (phys.take(), core.take()) {
                    seen.insert((p, c));
                }
            } else if let Some((key, val)) = line.split_once(':') {
                match key.trim() {
                    "physical id" => phys = Some(val.trim().to_string()),
                    "core id" => core = Some(val.trim().to_string()),
                    _ => {}
                }
            }
        }
        if !seen.is_empty() {
            return seen.len();
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Pad the accumulated real bytes to the full PAR2 slice size and hand the
/// slice to the recovery encoder. Checksum computation is handled inside the
/// encoder (via `rayon::join`) when it was built with `.with_checksums()`.
/// Leaves `accum` empty for the next slice.
fn feed_par2_slice(accum: &mut Vec<u8>, par2_slice_size: usize, enc: &mut RecoveryEncoder) {
    // Recycle a buffer from the encoder's free-list when available so we don't
    // allocate a fresh page per slice; on the first batch the pool is empty
    // and `take_buffer` falls back to `Vec::with_capacity(par2_slice_size)`.
    let next = enc.take_buffer();
    let mut padded = std::mem::replace(accum, next);
    padded.resize(par2_slice_size, 0);
    tokio::task::block_in_place(|| enc.add_slice(padded));
}

/// Base name for the PAR2 set's on-disk files. A published name may be a
/// relative path (`season01/ep01.mkv`); the PAR2 index and volume files live
/// at a single level, so they take the top-level component (the root folder,
/// or the file's own name for a single-file upload) as their base.
fn par2_base(name: &str) -> &str {
    name.split('/').next().unwrap_or(name)
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
    let mut per_file_articles = Vec::with_capacity(metas.len());
    for meta in &metas {
        per_file_articles.push(yenc::segments(meta.size, article_size).len());
    }

    // Choose the PAR2 slice size: groups consecutive articles into larger slices
    // to keep input-block count near TARGET_PAR2_SLICES while satisfying both
    // PAR2 spec limits (32 768 input blocks, 65 535 recovery blocks).
    let (par2_slice_size, total_slices) =
        optimal_par2_slice_size(&per_file_articles, article_size, shared.config.par2);

    let recovery_count = (total_slices * shared.config.par2 as usize) / 100;

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
            let mut sys = sysinfo::System::new_all();
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

    // Melhoria 2: hash each source file on its own Tokio task, running
    // concurrently with the PAR2 encode pass so the ~13s MD5 cost overlaps
    // with Reed-Solomon computation instead of being sequential.
    let mut file_hash_tasks: Vec<_> = if recovery_count > 0 {
        metas
            .iter()
            .map(|meta| {
                let path = meta.path.clone();
                tokio::spawn(async move {
                    let mut hasher = FileHasher::new();
                    let mut file = File::open(&path).await?;
                    let mut buf = vec![0u8; 512 * 1024];
                    loop {
                        let n = file.read(&mut buf).await?;
                        if n == 0 {
                            break;
                        }
                        hasher.update(&buf[..n]);
                    }
                    Ok::<_, anyhow::Error>(hasher.finish())
                })
            })
            .collect()
    } else {
        Vec::new()
    };

    if recovery_count > 0 {
        let simd_method = detect_par2_simd();
        info!(
            simd = simd_method,
            threads = performance_core_count(),
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
            threads: performance_core_count(),
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
        let mut encoder = if rec_count > 0 {
            let enc = RecoveryEncoder::new(par2_slice_size, total_slices, exp_start, rec_count);
            // On passes with many recovery blocks, increasing the queue size
            // (cache blocking) amortizes the flush cost over more input data.
            // We use 1/4 of the available memory limit for the queue, capped
            // between 256MB and 2GB.
            let queue_limit = (memory_limit / 4).clamp(256 * 1024 * 1024, 2 * 1024 * 1024 * 1024);
            let enc = enc.with_flush_limit(queue_limit);

            // Melhoria 1: on pass 0 enable parallel checksum computation inside
            // the encoder so rayon::join overlaps MD5+CRC32 with RS work.
            let enc = if pass_idx == 0 {
                enc.with_checksums()
            } else {
                enc
            };
            Some(enc)
        } else {
            None
        };

        let mut par2_slices_fed: usize = 0;

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
            // Source the buffer from the encoder's free-list so subsequent files
            // reuse allocations from earlier flushes.
            let mut par2_accum: Vec<u8> = match encoder.as_mut() {
                Some(enc) => enc.take_buffer(),
                None => Vec::new(),
            };

            let mut i: u32 = 0;
            while let Some((offset, buf)) = read_rx.recv().await {
                if shared.cancelled.load(Ordering::Relaxed) {
                    drop(read_rx);
                    let _ = reader_handle.await;
                    return Ok(());
                }

                // The encoder is `Some` exactly when PAR2 is enabled, so all
                // PAR2 work — hashing, checksums, recovery — is gated on it.
                if let Some(enc) = &mut encoder {
                    // Append the article to the current PAR2 slice. Every
                    // article but a file's last is exactly `article_size`, so
                    // the accumulator reaches `par2_slice_size` precisely on
                    // the K-th article; a short final article falls through to
                    // the partial-slice flush after the loop.
                    par2_accum.extend_from_slice(&buf);
                    if par2_accum.len() >= par2_slice_size {
                        feed_par2_slice(&mut par2_accum, par2_slice_size, enc);
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
                            .send(PostTask {
                                meta: meta.clone(),
                                part: i,
                                total: total_parts,
                                offset,
                                data: buf,
                            })
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

            let _ = reader_handle.await;

            // Flush the file's final, partial PAR2 slice (zero-padded).
            if let Some(enc) = &mut encoder {
                if !par2_accum.is_empty() {
                    feed_par2_slice(&mut par2_accum, par2_slice_size, enc);
                }
            }
        }

        if let Some(enc) = encoder {
            shared.emit(ProgressEvent::Status {
                text: "computing PAR2 recovery data".to_string(),
            });
            let t_par2_compute = std::time::Instant::now();
            let (recovery_slices, slice_checksums) = enc.finish();
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

                // Melhoria 2: await the parallel file-hash tasks that have
                // been running concurrently with the encode pass.
                let mut file_ids = Vec::new();
                let mut hashes = Vec::new();

                for (idx, task) in file_hash_tasks.drain(..).enumerate() {
                    let fh = task.await??;
                    let fid =
                        packet::compute_file_id(&fh.md5_16k, fh.length, &metas[idx].real_name);
                    file_ids.push(fid);
                    hashes.push(fh);
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

                for (idx, fh) in hashes.iter().enumerate() {
                    let fid = &file_ids[idx];
                    let pkt_file_desc = packet::serialize_packet(
                        &rsid,
                        &packet::TYPE_FILE_DESC,
                        &packet::file_description_body(
                            fid,
                            &fh.md5_full,
                            &fh.md5_16k,
                            fh.length,
                            &metas[idx].real_name,
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

    let (subject_name, yenc_name) = match shared.config.obfuscate {
        ObfuscateMode::None => (real_name.clone(), real_name.clone()),
        ObfuscateMode::Subject => (obfuscated_name(), real_name.clone()),
        ObfuscateMode::Full => {
            let obfuscated = obfuscated_name();
            (obfuscated.clone(), obfuscated)
        }
    };

    let meta = Arc::new(FileMeta {
        path: path.clone(),
        real_name,
        subject_name,
        yenc_name,
        size,
    });

    let mut file = tokio::fs::File::open(path).await?;
    for (i, (offset, len)) in segments.into_iter().enumerate() {
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).await?;
        if tx
            .send(PostTask {
                meta: meta.clone(),
                part: i as u32 + 1,
                total,
                offset,
                data: buf,
            })
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

    loop {
        if shared.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let task = {
            let mut rx = rx.lock().await;
            match rx.recv().await {
                Some(t) => t,
                None => break,
            }
        };

        shared.emit(ProgressEvent::ConnectionBusy {
            conn: conn_id,
            file: task.meta.real_name.clone(),
        });

        // Check resume state: if this segment was already posted, reuse
        // the stored Message-ID and skip network posting entirely.
        if let Some(resume) = &shared.resume {
            if let Some(existing_id) = resume
                .lock()
                .unwrap()
                .get(&task.meta.real_name, task.part)
                .map(str::to_string)
            {
                shared.results.lock().unwrap().push(PostedSegment {
                    file_name: task.meta.real_name.clone(),
                    subject_name: task.meta.subject_name.clone(),
                    file_size: task.meta.size,
                    part: task.part,
                    total: task.total,
                    message_id: existing_id,
                    bytes: 0,
                });
                let bytes = task.data.len() as u64;
                shared.release_buffer(task.data); // Phase 12b
                shared.emit(ProgressEvent::SegmentDone {
                    file: task.meta.real_name.clone(),
                    bytes,
                    ok: true,
                });
                continue;
            }
        }

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
            None,
        );
        let message_id = generate_message_id(shared.config.message_id_domain.as_deref());
        let article = Article {
            message_id: message_id.clone(),
            from: shared.config.from.clone(),
            newsgroups: shared.config.groups.clone(),
            subject: default_subject(&task.meta.subject_name, task.part, task.total),
            date: resolve_date(shared.config.date.as_deref()),
            no_archive: shared.config.no_archive,
        };
        let payload = article.serialize(&encoded.body);

        let mut posted = false;
        let mut last_err = String::from("unknown error");

        if shared.config.dry_run {
            posted = true;
        } else {
            // Apply rate limiting before sending.
            rate_limiter.acquire(payload.len()).await;

            let max_attempts = shared.config.retries;
            // Try up to `max_attempts` times; on any failure `slot.invalidate()`
            // drops the bad connection and rotates to the next server so the
            // next attempt targets a different one.
            for attempt in 1..=max_attempts {
                let conn = match slot.ensure_connected().await {
                    Ok(c) => c,
                    Err(e) => {
                        last_err = format!("{e:#}");
                        warn!(
                            segment = %message_id,
                            attempt,
                            max_attempts,
                            error = %last_err,
                            "connection failed; will retry"
                        );
                        shared.total_retries.fetch_add(1, Ordering::Relaxed);
                        let backoff = slot.retry_delay();
                        if attempt < max_attempts {
                            tokio::time::sleep(backoff).await;
                        }
                        continue;
                    }
                };
                match conn.post(&payload).await {
                    Ok(()) => {
                        // Optionally verify the article was accepted by STAT.
                        if shared.config.verify {
                            match conn.stat(&message_id).await {
                                Ok(true) => {
                                    debug!(segment = %message_id, "posted and verified via STAT");
                                    posted = true;
                                    break;
                                }
                                Ok(false) => {
                                    last_err = format!(
                                        "STAT: article {message_id} not found after posting"
                                    );
                                    warn!(segment = %message_id, attempt, "STAT not found; retrying");
                                    slot.invalidate();
                                }
                                Err(e) => {
                                    last_err = format!("STAT: {e:#}");
                                    warn!(segment = %message_id, error = %e, "STAT error; retrying");
                                    slot.invalidate();
                                }
                            }
                        } else {
                            debug!(segment = %message_id, "posted");
                            posted = true;
                            break;
                        }
                    }
                    Err(e) => {
                        last_err = format!("{e:#}");
                        warn!(
                            segment = %message_id,
                            attempt,
                            max_attempts,
                            error = %last_err,
                            "post failed; rotating server"
                        );
                        shared.total_retries.fetch_add(1, Ordering::Relaxed);
                        slot.invalidate();
                    }
                }
                let backoff = slot.retry_delay();
                if attempt < max_attempts {
                    tokio::time::sleep(backoff).await;
                }
            }
        }

        if posted {
            // Persist the segment to the resume state before adding to results,
            // so a crash after this point still skips it on the next run.
            if let Some(resume) = &shared.resume {
                let mut state = resume.lock().unwrap();
                state.record(&task.meta.real_name, task.part, &message_id);
                if let Some(rp) = &shared.resume_path {
                    let _ = state.save(rp);
                }
            }
            shared.results.lock().unwrap().push(PostedSegment {
                file_name: task.meta.real_name.clone(),
                subject_name: task.meta.subject_name.clone(),
                file_size: task.meta.size,
                part: task.part,
                total: task.total,
                message_id,
                bytes: payload.len() as u64,
            });
        } else {
            record_failure(&shared, &task.meta, &task, &last_err);
        }
        let article_bytes = task.data.len() as u64;
        // Phase 12b: return the article buffer to the shared pool so the
        // producer's reader task can reuse it without allocating.
        shared.release_buffer(task.data);
        shared.emit(ProgressEvent::SegmentDone {
            file: task.meta.real_name.clone(),
            bytes: article_bytes,
            ok: posted,
        });
    }

    shared.emit(ProgressEvent::ConnectionIdle { conn: conn_id });
    slot.quit().await;
}

/// Compute the `Date:` header value from the config `date` option.
///
/// - `None` / `"now"` → current UTC time formatted as RFC 2822
/// - `"random"` → random time within the last 30 days
/// - any other string → used verbatim (caller-supplied RFC 2822 timestamp)
fn resolve_date(mode: Option<&str>) -> Option<String> {
    match mode {
        None => None,
        Some("now") => Some(format_rfc2822(SystemTime::now())),
        Some("random") => {
            // Pick a random offset in [0, 30 days) before now.
            use std::collections::hash_map::RandomState;
            use std::hash::{BuildHasher, Hasher};
            let r = RandomState::new().build_hasher().finish();
            let offset_secs = r % (30 * 24 * 3600);
            let t = SystemTime::now()
                .checked_sub(Duration::from_secs(offset_secs))
                .unwrap_or(UNIX_EPOCH);
            Some(format_rfc2822(t))
        }
        Some(fixed) => Some(fixed.to_string()),
    }
}

fn record_failure(shared: &Shared, meta: &FileMeta, task: &PostTask, error: &str) {
    let description = format!(
        "{} part {}/{}: {error}",
        meta.real_name, task.part, task.total
    );
    shared.emit(ProgressEvent::Failed {
        description: description.clone(),
    });
    shared.failures.lock().unwrap().push(description);
}

/// Check that every article in `segments` is retrievable via `STAT`.
///
/// Opens a single NNTP connection (using the primary server from `config`),
/// waits `config.check_delay_secs`, then queries each article. Each article
/// is retried up to `config.check_retries` times before being recorded as
/// missing. Returns the list of `Message-ID`s that could not be confirmed.
///
/// Progress events (`CheckStarted`, `CheckProgress`, `CheckDone`) are emitted
/// on `events` when provided.
pub async fn check_articles(
    config: &Config,
    segments: &[PostedSegment],
    events: Option<&ProgressSender>,
) -> Result<Vec<String>> {
    if segments.is_empty() {
        return Ok(Vec::new());
    }

    let total = segments.len() as u64;
    if let Some(tx) = events {
        let _ = tx.send(ProgressEvent::CheckStarted { total });
    }

    // Wait for server propagation before checking.
    if config.check_delay_secs > 0 {
        tokio::time::sleep(Duration::from_secs(config.check_delay_secs)).await;
    }

    // Open a single connection for the check pass — STAT is lightweight.
    let server = config
        .all_servers()
        .next()
        .expect("at least one server is configured");
    let mut slot = ConnectionSlot::new(Arc::new(vec![server]), 0);

    let mut missing = Vec::new();
    let max_attempts = config.check_retries.max(1) as usize;

    for (idx, seg) in segments.iter().enumerate() {
        // Strip angle brackets for the STAT command.
        let id = seg.message_id.trim_start_matches('<').trim_end_matches('>');

        let mut found = false;
        for attempt in 1..=max_attempts {
            match slot.ensure_connected().await {
                Ok(conn) => match conn.stat(id).await {
                    Ok(true) => {
                        found = true;
                        break;
                    }
                    Ok(false) => {
                        // Not found yet — wait a bit and retry.
                        if attempt < max_attempts {
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                    Err(_) => {
                        slot.invalidate();
                        if attempt < max_attempts {
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                },
                Err(_) => {
                    if attempt < max_attempts {
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                }
            }
        }

        if !found {
            missing.push(seg.message_id.clone());
        }

        let checked = idx as u64 + 1;
        if let Some(tx) = events {
            let _ = tx.send(ProgressEvent::CheckProgress { checked, ok: found });
        }
    }

    let failed = missing.len() as u64;
    if let Some(tx) = events {
        let _ = tx.send(ProgressEvent::CheckDone { failed });
    }

    Ok(missing)
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
        assert_eq!(resolve_date(None), None);
    }

    #[test]
    fn resolve_date_now_returns_rfc2822() {
        let d = resolve_date(Some("now")).unwrap();
        // Should look like "Mon, 01 Jan 2024 00:00:00 +0000".
        assert!(d.ends_with("+0000"));
        assert!(d.contains(':'));
    }

    #[test]
    fn resolve_date_random_returns_rfc2822() {
        let d = resolve_date(Some("random")).unwrap();
        assert!(d.ends_with("+0000"));
    }

    #[test]
    fn resolve_date_fixed_is_returned_verbatim() {
        let fixed = "Tue, 14 Jan 2025 10:00:00 +0000";
        assert_eq!(resolve_date(Some(fixed)).as_deref(), Some(fixed));
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
            size: 0,
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

    // ── physical_core_count ───────────────────────────────────────────────────

    #[test]
    fn physical_core_count_is_at_least_one() {
        assert!(physical_core_count() >= 1);
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
        Arc::new(Shared {
            config,
            servers: Arc::new(vec![]),
            results: Mutex::new(Vec::new()),
            failures: Mutex::new(Vec::new()),
            events: None,
            cancelled: AtomicBool::new(false),
            resume: None,
            resume_path: None,
            pool: Arc::new(Mutex::new(Vec::new())),
            total_retries: std::sync::atomic::AtomicUsize::new(0),
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
        };
        record_failure(&shared, &task.meta, &task, "timeout");
        let failures = shared.failures.lock().unwrap();
        assert_eq!(failures.len(), 1);
        assert!(failures[0].contains("ep.mkv"));
        assert!(failures[0].contains("2/5"));
        assert!(failures[0].contains("timeout"));
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
                obfuscate: Some(crate::config::ObfuscateMode::Subject),
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

        let outcome = post_files_with_progress(&config, &files, None, Some(&state_path))
            .await
            .unwrap();

        // Segment is present but Message-ID is a fresh one, not the stored one.
        assert_eq!(outcome.segments.len(), 1);
        assert_ne!(outcome.segments[0].message_id, "<stored-id@pesto>");
    }
}
