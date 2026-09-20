//! Producer: sequential file reading, PAR2 feeding and task production.

use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result};
use tokio::io::AsyncWriteExt;
use tracing::info;

use crate::progress::ProgressEvent;
use crate::yenc;
use parmesan::encoder::{FileHashes, RecoveryEncoder};
use parmesan::layout;
use parmesan::ops::ingest_files_with_progress;
use parmesan::ops::InputFile as Par2InputFile;
use parmesan::packet::{self, SliceChecksum};
use parmesan::worker::Par2Worker;

use super::par2::{
    address_space_limit, connection_overhead_reserve, par2_geometry, par2_memory_plan,
};
use super::shared::Shared;
use super::task::{PostTask, TaskDispatcher};
use super::{
    make_task, par2_output_dir, par2_release_base, par2_temp_dir, push_par2_file,
    spawn_double_buffered_reader, FileMeta,
};
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

    let slices_fed = std::sync::atomic::AtomicUsize::new(*par2_slices_fed);
    let file_bytes_read = std::sync::atomic::AtomicUsize::new(0usize);
    ingest_files_with_progress(
        &files,
        worker,
        par2_slice_size,
        Some(&shared.cancelled),
        |file| {
            let exact =
                slices_fed.load(Ordering::Relaxed) + (file.size as usize).div_ceil(par2_slice_size);
            slices_fed.store(exact.min(total_slices), Ordering::Relaxed);
            file_bytes_read.store(0, Ordering::Relaxed);
            shared.emit(ProgressEvent::Par2InputProgress {
                done: slices_fed.load(Ordering::Relaxed),
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
        |bytes| {
            let read = file_bytes_read.fetch_add(bytes, Ordering::Relaxed) + bytes;
            let done =
                (slices_fed.load(Ordering::Relaxed) + read / par2_slice_size).min(total_slices);
            shared.emit(ProgressEvent::Par2InputProgress {
                done,
                total: total_slices,
            });
            Ok(())
        },
    )
    .await?;
    *par2_slices_fed = slices_fed.load(Ordering::Relaxed);
    Ok(())
}

pub(super) async fn producer(
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
    let mut par2_materialize_started = None;
    let mut par2_materialized_files = 0usize;
    let mut par2_materialized_bytes = 0u64;

    for (pass_idx, (exp_start, rec_count)) in passes.iter().copied().enumerate() {
        if rec_count > 0 {
            shared.emit(ProgressEvent::Par2PassStarted {
                pass: pass_idx + 1,
                passes: passes.len(),
            });
        }
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
            shared.emit(ProgressEvent::Par2ComputeStarted {
                pass: pass_idx + 1,
                passes: passes.len(),
            });
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
                    let fid = packet::compute_file_id(&fh.md5_16k, fh.length, &meta.client_path);
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
                            &metas[idx].client_path,
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
                    info!(
                        path = %par2_dir.as_ref().unwrap().display(),
                        configured_scratch_base = shared.config.par2_temp_dir.is_some(),
                        "PAR2-only output directory selected; PAR2 scratch configuration is not used"
                    );
                } else {
                    par2_dir = Some(par2_temp_dir(
                        shared.config.par2_temp_dir.as_deref(),
                        shared.run_id,
                    ));
                    let dir = par2_dir.as_ref().unwrap();
                    tokio::fs::create_dir_all(dir).await.with_context(|| {
                        format!("creating PAR2 scratch directory `{}`", dir.display())
                    })?;
                    par2_materialize_started = Some(Instant::now());
                    info!(
                        path = %dir.display(),
                        configured_base = shared.config.par2_temp_dir.is_some(),
                        "PAR2 scratch directory created"
                    );
                }

                let index_name = layout::index_name(par2_release_base(&metas[0].real_name));
                let index_path = par2_dir.as_ref().unwrap().join(&index_name);
                tokio::fs::write(&index_path, &base_packets)
                    .await
                    .with_context(|| format!("writing PAR2 index `{}`", index_path.display()))?;
                par2_materialized_files += 1;
                par2_materialized_bytes += base_packets.len() as u64;
                if let Some(tx) = &tx_opt {
                    if shared.config.obfuscate.policy().publish_par2_index {
                        // In discovery modes the standalone index remains the
                        // cheapest way for indexers and clients to discover
                        // the recovery set.
                        let wire_override =
                            shared.release_prefix.as_deref().map(layout::index_name);
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
                    .await
                    .with_context(|| {
                        format!("opening PAR2 recovery volume `{}`", vol_path.display())
                    })?;

                if slice.exponent == vol.first {
                    file.write_all(&base_packets).await.with_context(|| {
                        format!("writing PAR2 recovery volume `{}`", vol_path.display())
                    })?;
                    par2_materialized_files += 1;
                    par2_materialized_bytes += base_packets.len() as u64;
                }

                let pkt = packet::serialize_packet(
                    &rsid,
                    &packet::TYPE_RECOVERY,
                    &packet::recovery_body(slice.exponent, &slice.data),
                );
                file.write_all(&pkt).await.with_context(|| {
                    format!("writing PAR2 recovery volume `{}`", vol_path.display())
                })?;
                par2_materialized_bytes += pkt.len() as u64;
                shared.emit(crate::progress::ProgressEvent::Par2SliceWritten);

                if slice.exponent == vol.first + vol.count - 1 {
                    if let Some(tx) = &tx_opt {
                        let wire_override = shared
                            .release_prefix
                            .as_deref()
                            .map(|prefix| layout::volume_name(prefix, *vol));
                        let index_offset =
                            u32::from(shared.config.obfuscate.policy().publish_par2_index);
                        let file_index = metas.len() as u32 + 1 + index_offset + vol_idx as u32;
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

    if let (Some(dir), Some(started)) = (&par2_dir, par2_materialize_started) {
        if !shared.config.par2_only {
            info!(
                path = %dir.display(),
                files = par2_materialized_files,
                bytes = par2_materialized_bytes,
                elapsed_ms = started.elapsed().as_millis(),
                "PAR2 scratch files ready"
            );
        }
    }

    Ok(())
}
