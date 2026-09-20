//! Season-wide PAR2 recovery sets: one coherent set covering every episode.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::{bail, Context, Result};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info};

use crate::config::Config;
use crate::progress::{ProgressEvent, ProgressSender};
use parmesan::encoder::{FileHashes, RecoveryEncoder};
use parmesan::layout;
use parmesan::ops::{ingest_files_with_progress, InputFile as Par2InputFile};
use parmesan::packet::{self, SliceChecksum};
use parmesan::worker::Par2Worker;

use super::super::file_md5_16k;
use super::geometry::par2_geometry_from_sizes;
use super::memory::par2_memory_plan;

/// One episode's identity within a season-wide PAR2 recovery set: enough to
/// emit a File Description + IFSC packet pair for it. `name` is the bare
/// file name (no directory components) — the season equivalent of
/// `wire_name` for a single-file entry, since each episode path here is
/// already one standalone top-level entry, never a release subdirectory.
struct SeasonFileEntry {
    file_id: [u8; 16],
    name: String,
    hashes: FileHashes,
    slice_checksums: Vec<SliceChecksum>,
}

/// A season-wide PAR2 recovery set, ready to be serialized to disk by
/// `write_season_par2_volumes`. Carries one [`SeasonFileEntry`] per
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
    events: Option<&ProgressSender>,
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
        if let Some(tx) = events {
            let _ = tx.send(ProgressEvent::Par2SliceWritten);
        }
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
    total_slices: usize,
    events: Option<&ProgressSender>,
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
        let slices_for_episode = (file_size as usize).div_ceil(par2_slice_size);
        let bytes_read = std::sync::atomic::AtomicUsize::new(0usize);
        let completed_before = total_slices_added;
        ingest_files_with_progress(
            std::slice::from_ref(&input),
            worker,
            par2_slice_size,
            None,
            |_| Ok(()),
            |bytes| {
                let read = bytes_read.fetch_add(bytes, Ordering::Relaxed) + bytes;
                let current = (read / par2_slice_size).min(slices_for_episode);
                if let Some(tx) = events {
                    let _ = tx.send(ProgressEvent::Par2InputProgress {
                        done: (completed_before + current).min(total_slices),
                        total: total_slices,
                    });
                }
                Ok(())
            },
        )
        .await?;

        total_slices_added += slices_for_episode;
        if let Some(tx) = events {
            let _ = tx.send(ProgressEvent::Par2InputProgress {
                done: total_slices_added.min(total_slices),
                total: total_slices,
            });
        }
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
    events: Option<&ProgressSender>,
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
        bail!("too many input slices: {total_slices} (max 32768). Increase --slice-size or decrease --slice-count.");
    }
    if recovery_count > 65535 {
        bail!("too many recovery blocks: {recovery_count} (max 65535). Increase --slice-size or decrease --par2/--recovery-count.");
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

    if let Some(tx) = events {
        let simd_method = if config.simd != parmesan::SimdPath::Auto {
            config.simd.to_string()
        } else {
            parmesan::detect_simd().to_string()
        };
        let threads = if config.threads > 0 {
            config.threads
        } else {
            parmesan::performance_core_count()
        };
        let _ = tx.send(ProgressEvent::Par2EncodeStarted {
            input_bytes: sizes.iter().sum(),
            input_slices: total_slices,
            input_files: ordered.len(),
            recovery_slices: recovery_count,
            slice_size: par2_slice_size,
            passes: passes.len(),
            chunk_size: 32 * 1024,
            simd_method,
            threads,
            memory_limit,
        });
        let _ = tx.send(ProgressEvent::Par2WriteStarted {
            total: recovery_count as u32,
        });
    }

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
        if let Some(tx) = events {
            let _ = tx.send(ProgressEvent::Par2PassStarted {
                pass: pass_idx + 1,
                passes: passes.len(),
            });
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
            feed_season_episodes(&ordered, &worker, par2_slice_size, total_slices, events).await?;
        debug!(
            pass = pass_idx,
            calculated_total_slices = total_slices,
            actual_slices_added = added,
            "season PAR2 pass fed"
        );

        if let Some(tx) = events {
            let _ = tx.send(ProgressEvent::Par2ComputeStarted {
                pass: pass_idx + 1,
                passes: passes.len(),
            });
        }
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
            events,
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
    generate_and_write_season_par2_with_progress(
        episode_paths,
        release_name,
        output_dir,
        config,
        None,
    )
    .await
}

/// Generate season PAR2 volumes while reporting read/compute/write progress.
pub async fn generate_and_write_season_par2_with_progress(
    episode_paths: &[PathBuf],
    release_name: &str,
    output_dir: &Path,
    config: &Config,
    events: Option<&ProgressSender>,
) -> Result<PathBuf> {
    if episode_paths.is_empty() || config.par2 == 0 {
        return Ok(output_dir.to_path_buf());
    }

    debug!(episodes = episode_paths.len(), "generating season PAR2");

    let season =
        generate_season_par2(episode_paths, config, release_name, output_dir, events).await?;

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
