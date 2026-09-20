//! Parallel posting: the orchestration that ties together file reading, yEnc
//! encoding, article assembly and the NNTP client.
//!
//! Files are read sequentially by a producer. yEnc runs on a small encode
//! pool (nyuu: one encoder filling a ready-article queue). NNTP workers only
//! POST. If PAR2 recovery exceeds a memory limit, the producer re-reads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{Context, Result};
use tokio::fs::File;
use tokio::io::AsyncReadExt;

use crate::article::{obfuscated_name, obfuscated_name_with_prefix, random_from};
use crate::config::{Config, ObfuscateMode};
use crate::nntp::pool::ConnectionBroker;
use crate::progress::{ProgressEvent, ProgressSender};
use crate::walk::InputFile;
use crate::yenc;
use parmesan::encoder::FileHasher;
use parmesan::layout;

mod check;
mod connections;
mod par2;
pub use par2::{generate_and_write_season_par2, generate_and_write_season_par2_with_progress};
mod identity;
pub use identity::pick_post_group;
use identity::{
    normalize_client_path, obfuscated_yenc_name, par2_release_base, persisted_identity,
    resolve_date,
};
mod outcome;
pub use outcome::{
    nzb_write_decision, should_write_season_nzb, FailedTask, NzbWriteDecision, PostOutcome,
    PostedSegment,
};
mod options;
mod orchestrator;
mod prepare;
pub use orchestrator::post_files_inner_with_release_prefix;
mod producer;
mod result;
pub use result::repost_failed_tasks;
mod shared;
use shared::Shared;
mod task;
use task::{PostTask, TaskDispatcher};
mod worker;

#[derive(Debug, Clone)]
struct FileMeta {
    path: PathBuf,
    real_name: String,
    client_path: String,
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
    post_files_inner_with_release_prefix(
        config,
        files,
        events,
        resume_state_path,
        external_cancel,
        entry_label,
        broker,
        external_pause,
        None,
    )
    .await
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

    if shared.config.obfuscate.policy().publish_par2_index {
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
    }

    let volumes = layout::plan_volumes(recovery_count as u32);
    for (vol_idx, vol) in volumes.iter().enumerate() {
        let vol_name = layout::volume_name(par2_release_base(&metas[0].real_name), *vol);
        let vol_path = par2_dir.join(&vol_name);
        let wire_override = shared
            .release_prefix
            .as_deref()
            .map(|prefix| layout::volume_name(prefix, *vol));
        let index_offset = u32::from(shared.config.obfuscate.policy().publish_par2_index);
        let file_index = metas.len() as u32 + 1 + index_offset + vol_idx as u32;
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
    let client_path = normalize_client_path(&real_name, None)?.to_owned();
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
            // NZBGet uses the yEnc name while collecting recovery volumes;
            // retain only the technical extension so it cleans them up after
            // repair without exposing the real volume name.
            format!("{}.par2", obfuscated_name_with_prefix(prefix))
        };
        (name, yenc, shared.release_from.clone().unwrap_or_default())
    } else {
        match shared.config.obfuscate {
            ObfuscateMode::None => {
                let wn = client_path.clone();
                (wn.clone(), wn, shared.config.from.clone())
            }
            ObfuscateMode::Full | ObfuscateMode::Article | ObfuscateMode::FullShared => (
                obfuscated_name(),
                obfuscated_yenc_name(&real_name),
                random_from(),
            ),
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
        client_path,
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

/// Build a `PostTask`, generating per-article identities for the two
/// article-level modes; otherwise copies them from `FileMeta`.
fn make_task(
    meta: Arc<FileMeta>,
    part: u32,
    total: u32,
    offset: u64,
    data: Vec<u8>,
    file_crc32: Option<u32>,
    config: &Config,
) -> PostTask {
    let (subject_name, yenc_name, from, date) = match config.obfuscate {
        ObfuscateMode::Full => (
            obfuscated_name(),
            meta.yenc_name.clone(),
            random_from(),
            meta.date.clone(),
        ),
        ObfuscateMode::Article => (
            obfuscated_name(),
            obfuscated_yenc_name(&meta.real_name),
            random_from(),
            meta.date.clone(),
        ),
        _ => {
            let date = if config.date.as_deref() == Some("now") {
                resolve_date(Some("now"))
            } else {
                meta.date.clone()
            };
            (
                meta.subject_name.clone(),
                meta.yenc_name.clone(),
                meta.from.clone(),
                date,
            )
        }
    };
    PostTask {
        meta,
        part,
        total,
        offset,
        data,
        subject_name,
        yenc_name,
        from,
        date,
        file_crc32,
    }
}

#[cfg(test)]
mod tests;
