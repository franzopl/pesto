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

use crate::article::{
    default_subject, format_rfc2822, generate_message_id, obfuscated_name, Article,
};
use crate::config::{Config, ObfuscateMode, ServerEntry};
use crate::nntp::Connection;
use crate::par2::encoder::{slice_checksum, FileHasher, RecoveryEncoder};
use crate::par2::layout;
use crate::par2::packet::{self, SliceChecksum};
use crate::progress::{FileEntry, ProgressEvent, ProgressSender, RunMode};
use crate::resume::ResumeState;
use crate::walk::InputFile;
use crate::yenc;

/// Maximum memory to use for PAR2 recovery slices (in bytes).
const MAX_PAR2_MEMORY: usize = 1_000_000_000; // 1 GB
/// Target number of PAR2 input slices. Reed-Solomon encoding cost grows with
/// `file_size² / par2_slice_size`, so tying the PAR2 slice to the (small)
/// article size makes large files quadratically expensive. Several articles
/// are grouped into one PAR2 slice to keep the input-block count near this
/// target, which is the dominant lever on PAR2 CPU cost.
const TARGET_PAR2_SLICES: usize = 1000;

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
    servers: Vec<ServerEntry>,

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

    let servers: Vec<ServerEntry> = config.all_servers().collect();
    let total_conns = config.total_connections();

    let worker_count = if config.par2_only {
        0
    } else {
        total_conns.max(1).min(initial_segments.max(1) as usize)
    };

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

    let mut handles = Vec::with_capacity(worker_count);
    let tx_opt = if worker_count > 0 {
        let (tx, rx) = tokio::sync::mpsc::channel(worker_count * 2);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        // Distribute workers across servers according to each server's
        // configured connection count. Workers beyond the total are assigned
        // round-robin so we never exceed `worker_count`.
        let server_assignments = build_server_assignments(&shared.servers, worker_count);
        for (idx, primary_server_idx) in server_assignments.into_iter().enumerate() {
            handles.push(tokio::spawn(worker(
                shared.clone(),
                rx.clone(),
                idx,
                primary_server_idx,
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
            .num_threads(physical_core_count())
            .build_global();
    });
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

/// Pad the accumulated real bytes to the full PAR2 slice size, hand the slice
/// to the recovery encoder and, when `checksums` is `Some` (first pass only),
/// record its IFSC checksum. Leaves `accum` empty for the next slice.
fn feed_par2_slice(
    accum: &mut Vec<u8>,
    par2_slice_size: usize,
    enc: &mut RecoveryEncoder,
    checksums: Option<&mut Vec<SliceChecksum>>,
) {
    let mut padded = std::mem::replace(accum, Vec::with_capacity(par2_slice_size));
    padded.resize(par2_slice_size, 0);
    if let Some(checksums) = checksums {
        checksums.push(slice_checksum(&padded));
    }
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
    let mut total_articles = 0usize;
    for meta in &metas {
        let n = yenc::segments(meta.size, article_size).len();
        per_file_articles.push(n);
        total_articles += n;
    }

    // A PAR2 input slice spans `articles_per_slice` consecutive articles, so
    // the input-block count stays near `TARGET_PAR2_SLICES`. Posting still
    // uses the unchanged per-article segmentation; only the encoder sees the
    // larger slice.
    //
    // For very large datasets, we increase the slice count up to the PAR2
    // limit of 32,768 to keep the memory footprint of each slice reasonable.
    let target_slices = (total_articles / 10).clamp(TARGET_PAR2_SLICES, 32768);
    let articles_per_slice = total_articles.div_ceil(target_slices).max(1);
    let par2_slice_size = articles_per_slice * article_size;

    // PAR2 splits each file independently; a file's last slice is zero-padded.
    let total_slices: usize = per_file_articles
        .iter()
        .map(|&n| n.div_ceil(articles_per_slice))
        .sum();

    // The sum might slightly exceed the block limit if many files have small
    // trailing fragments; if so, we must increase the slice size.
    let (par2_slice_size, total_slices) = if total_slices > 32768 {
        let mut articles = articles_per_slice;
        let mut count = total_slices;
        while count > 32768 {
            articles += 1;
            count = per_file_articles
                .iter()
                .map(|&n| n.div_ceil(articles))
                .sum();
        }
        (articles * article_size, count)
    } else {
        (par2_slice_size, total_slices)
    };

    let recovery_count = (total_slices * shared.config.par2 as usize) / 100;
    let slices_per_pass = (MAX_PAR2_MEMORY / par2_slice_size).max(1);

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

    let mut par2_files = Vec::new();
    let mut all_checksums: Vec<Vec<SliceChecksum>> = vec![Vec::new(); metas.len()];
    for _ in &metas {
        par2_files.push(FileHasher::new());
    }

    // Announce how many recovery slices will be written across all passes, so
    // the renderer can show a progress bar for the PAR2 write phase.
    if recovery_count > 0 {
        shared.emit(crate::progress::ProgressEvent::Par2WriteStarted {
            total: recovery_count as u32,
        });
    }

    let mut par2_dir = None;
    let mut base_packets = Vec::new();
    let mut rsid = [0u8; 16];

    for (pass_idx, (exp_start, rec_count)) in passes.iter().copied().enumerate() {
        let mut encoder = if rec_count > 0 {
            Some(RecoveryEncoder::new(
                par2_slice_size,
                total_slices,
                exp_start,
                rec_count,
            ))
        } else {
            None
        };

        for (file_idx, meta) in metas.iter().enumerate() {
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
            let mut par2_accum: Vec<u8> = if encoder.is_some() {
                Vec::with_capacity(par2_slice_size)
            } else {
                Vec::new()
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
                    if pass_idx == 0 {
                        par2_files[file_idx].update(&buf);
                    }
                    // Append the article to the current PAR2 slice. Every
                    // article but a file's last is exactly `article_size`, so
                    // the accumulator reaches `par2_slice_size` precisely on
                    // the K-th article; a short final article falls through to
                    // the partial-slice flush after the loop.
                    par2_accum.extend_from_slice(&buf);
                    if par2_accum.len() >= par2_slice_size {
                        let checksums = (pass_idx == 0).then_some(&mut all_checksums[file_idx]);
                        feed_par2_slice(&mut par2_accum, par2_slice_size, enc, checksums);
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
                    let checksums = (pass_idx == 0).then_some(&mut all_checksums[file_idx]);
                    feed_par2_slice(&mut par2_accum, par2_slice_size, enc, checksums);
                }
            }
        }

        if let Some(enc) = encoder {
            shared.emit(ProgressEvent::Status {
                text: "computing PAR2 recovery data".to_string(),
            });
            let recovery_slices = enc.finish();
            shared.emit(ProgressEvent::Status {
                text: String::new(),
            });

            if pass_idx == 0 {
                let mut file_ids = Vec::new();
                let mut hashes = Vec::new();

                for (idx, hasher) in par2_files.drain(..).enumerate() {
                    let fh = hasher.finish();
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

/// Assign workers to servers according to each server's `connections` count.
///
/// Returns a `Vec<usize>` of length `worker_count` where each element is the
/// index of the server that worker should connect to first.
fn build_server_assignments(servers: &[ServerEntry], worker_count: usize) -> Vec<usize> {
    let mut assignments = Vec::with_capacity(worker_count);
    let mut remaining = worker_count;
    'outer: for (si, server) in servers.iter().enumerate() {
        for _ in 0..server.connections {
            if remaining == 0 {
                break 'outer;
            }
            assignments.push(si);
            remaining -= 1;
        }
    }
    // If all server connection slots are exhausted before worker_count is
    // reached (e.g. worker_count was clamped to segment count), fill the
    // rest round-robin.
    while assignments.len() < worker_count {
        assignments.push(assignments.len() % servers.len());
    }
    assignments
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
    primary_server_idx: usize,
) {
    let mut conn: Option<Connection> = None;
    let mut server_idx = primary_server_idx;
    let server_count = shared.servers.len();
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
            // Try up to `max_attempts` times, rotating servers on each
            // connection failure so the next attempt uses a different server.
            for attempt in 1..=max_attempts {
                if conn.is_none() {
                    let server = &shared.servers[server_idx];
                    let backoff = Duration::from_secs(server.retry_delay);
                    match connect_and_auth_server(server).await {
                        Ok(c) => conn = Some(c),
                        Err(e) => {
                            last_err = format!(
                                "connect to {} (server {}): {e:#}",
                                server.host, server_idx
                            );
                            // Rotate to the next server for the next attempt.
                            server_idx = (server_idx + 1) % server_count;
                            if attempt < max_attempts {
                                tokio::time::sleep(backoff).await;
                            }
                            continue;
                        }
                    }
                }
                let connection = conn.as_mut().expect("connection established above");
                match connection.post(&payload).await {
                    Ok(()) => {
                        // Optionally verify the article was accepted by STAT.
                        if shared.config.verify {
                            match connection.stat(&message_id).await {
                                Ok(true) => {
                                    posted = true;
                                    break;
                                }
                                Ok(false) => {
                                    last_err = format!(
                                        "STAT: article {message_id} not found after posting"
                                    );
                                    conn = None;
                                    server_idx = (server_idx + 1) % server_count;
                                }
                                Err(e) => {
                                    last_err = format!("STAT: {e:#}");
                                    conn = None;
                                    server_idx = (server_idx + 1) % server_count;
                                }
                            }
                        } else {
                            posted = true;
                            break;
                        }
                    }
                    Err(e) => {
                        last_err = format!("{e:#}");
                        conn = None;
                        server_idx = (server_idx + 1) % server_count;
                    }
                }
                let backoff = Duration::from_secs(shared.servers[server_idx].retry_delay);
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

    if let Some(mut connection) = conn {
        connection.quit().await;
    }
}

async fn connect_and_auth_server(server: &ServerEntry) -> Result<Connection> {
    let mut conn = Connection::connect(&server.host, server.port, server.ssl).await?;
    if let Some(username) = &server.username {
        let password = server.password.as_deref().unwrap_or("");
        conn.authenticate(username, password).await?;
    }
    Ok(conn)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_domain_is_random() {
        let a = crate::article::generate_message_id(None);
        let b = crate::article::generate_message_id(None);
        // Two consecutive IDs must differ and must not contain a fixed domain.
        assert_ne!(a, b);
        assert!(a.contains('@'));
        assert!(!a.contains("blocknews") && !a.contains("pesto"));
    }
}
