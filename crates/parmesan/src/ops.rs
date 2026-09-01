use crate::packet;
use crate::worker::Par2Worker;
use crate::SimdPath;
use anyhow::{Context, Result};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

/// High-level PAR2 creation parameters.
#[derive(Debug, Clone)]
pub struct CreateOptions {
    pub slice_size: Option<usize>,
    pub slice_count: Option<usize>,
    pub recovery_count: Option<usize>,
    pub recovery_pct: u8,
    pub memory_limit: usize,
    pub threads: usize,
    pub simd: SimdPath,
}

impl Default for CreateOptions {
    fn default() -> Self {
        Self {
            slice_size: None,
            slice_count: None,
            recovery_count: None,
            recovery_pct: 10,
            memory_limit: 1024 * 1024 * 1024, // 1 GiB
            threads: 0,                       // auto
            simd: SimdPath::Auto,
        }
    }
}

/// How to fit `recovery_count × slice_size` into `memory_limit`.
///
/// Parpar can cut the **slice** into windows (`chunks`) so every recovery
/// block stays in the working set; we used to only cut **recovery** into
/// passes, which re-reads the input once per pass and never overlaps dests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryLayout {
    /// Recovery blocks held at once.
    pub recovery_per_pass: usize,
    /// Bytes of each logical slice processed per encoder (equals `slice_size`
    /// when no windowing).
    pub slice_chunk: usize,
}

/// Byte window of each logical PAR2 slice sent to the encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SliceWindow {
    pub offset: usize,
    pub len: usize,
}

/// File hashes and per-slice checksums collected while ingesting.
#[derive(Debug, Default)]
pub struct IngestHashes {
    pub hashes: Vec<crate::encoder::FileHashes>,
    pub checksums: Vec<packet::SliceChecksum>,
}

/// Choose recovery-pass vs slice-chunk so the RS buffers fit `memory_limit`.
///
/// Prefers holding **all** recovery blocks with a 4 KiB-aligned slice window
/// when that window is at least 4 KiB. Otherwise falls back to full-slice
/// recovery passes (the previous behaviour).
pub fn plan_memory_layout(
    slice_size: usize,
    recovery_count: usize,
    memory_limit: usize,
) -> MemoryLayout {
    let slice_size = slice_size.max(32);
    if recovery_count == 0 {
        return MemoryLayout {
            recovery_per_pass: 0,
            slice_chunk: slice_size,
        };
    }
    let limit = memory_limit.max(slice_size);
    if recovery_count.saturating_mul(slice_size) <= limit {
        return MemoryLayout {
            recovery_per_pass: recovery_count,
            slice_chunk: slice_size,
        };
    }
    let raw = (limit / recovery_count).max(32);
    let chunk = (raw / 32) * 32;
    if chunk < slice_size && chunk >= 4096 {
        return MemoryLayout {
            recovery_per_pass: recovery_count,
            slice_chunk: chunk,
        };
    }
    let per = (limit / slice_size).max(1).min(recovery_count);
    MemoryLayout {
        recovery_per_pass: per,
        slice_chunk: slice_size,
    }
}

/// Metadata for an input file to be protected by PAR2.
#[derive(Debug, Clone)]
pub struct InputFile {
    pub path: PathBuf,
    pub display_name: String,
    pub size: u64,
}

/// Reorder `files` into the canonical order the PAR2 spec requires for
/// Reed-Solomon block indices: ascending numeric order of File ID.
///
/// Per the Parity Volume Set Specification, the Main packet lists File IDs
/// sorted as 16-byte unsigned integers, and "the first source block from the
/// first file in this sorted list receives the first valid constant, the
/// second block receives the second constant, and so on" — i.e. this order,
/// not the order files were passed on the command line, determines which
/// Reed-Solomon coefficient each input slice gets. Any reader that follows
/// the spec (par2cmdline included) computes coefficients this way, so the
/// encoder must feed slices in this order for multi-file recovery sets to be
/// repairable by anything other than this exact build of `parmesan`.
///
/// File ID only needs the first 16 KiB of each file (`compute_file_id`
/// hashes the 16k head, not the whole file), so this is a cheap pre-pass —
/// full-file hashing still happens once, later, during encoding.
pub fn sort_files_by_file_id(files: &mut Vec<InputFile>) -> Result<()> {
    use std::io::Read;

    let mut keyed: Vec<([u8; 16], InputFile)> = Vec::with_capacity(files.len());
    for f in files.drain(..) {
        let mut file = std::fs::File::open(&f.path)
            .with_context(|| format!("opening `{}` to compute its File ID", f.path.display()))?;
        let mut head = vec![0u8; 16 * 1024];
        let mut read = 0usize;
        while read < head.len() {
            match file.read(&mut head[read..])? {
                0 => break,
                n => read += n,
            }
        }
        head.truncate(read);
        let md5_16k = packet::md5(&head);
        let file_id = packet::compute_file_id(&md5_16k, f.size, &f.display_name);
        keyed.push((file_id, f));
    }
    keyed.sort_by_key(|(id, _)| *id);
    *files = keyed.into_iter().map(|(_, f)| f).collect();
    Ok(())
}

/// Computes total padded bytes for a given slice size across all files.
fn total_padded_bytes(files: &[InputFile], slice_size: usize) -> usize {
    files
        .iter()
        .map(|f| (f.size as usize).div_ceil(slice_size) * slice_size)
        .sum()
}

/// Computes total input slice count for a given slice size.
fn total_slice_count(files: &[InputFile], slice_size: usize) -> usize {
    files
        .iter()
        .map(|f| (f.size as usize).div_ceil(slice_size))
        .sum()
}

/// Calculates slice size and slice/recovery counts for a PAR2 recovery set.
///
/// When no explicit slice size or count is given, the heuristic targets ~2000
/// slices for reasonable throughput, then detects padding inflation caused by
/// many small files (common on Blu-ray/DVD disc structures). If the effective
/// parity overhead would exceed the requested percentage by more than 15%, the
/// slice size is halved repeatedly until the ratio is acceptable or the slice
/// count approaches `MAX_SLICES_PADDING_OPT`.
///
/// Reducing slice size does not increase peak memory usage: recovery buffer
/// memory ≈ recovery_count × slice_size ≈ total_padded × pct/100, which is
/// invariant to slice size for a fixed data set.
pub fn calculate_geometry(
    files: &[InputFile],
    options: &CreateOptions,
) -> Result<(usize, usize, usize)> {
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();

    // Upper bound on slices we allow when optimising for padding. Keeps GF
    // computation time bounded on slow machines (CPU cost ∝ total_slices).
    const MAX_SLICES_PADDING_OPT: usize = 6_000;
    // Trigger refinement when padded/actual ratio exceeds this.
    const PADDING_RATIO_THRESHOLD: f64 = 1.15;

    let (slice_size, total_slices) = if let Some(s) = options.slice_size {
        let s = (s / 64 * 64).max(64);
        let n = total_slice_count(files, s);
        (s, n)
    } else if let Some(count) = options.slice_count {
        let s = ((total_bytes as usize).div_ceil(count.max(1)) / 64 * 64).max(64);
        let n = total_slice_count(files, s);
        (s, n)
    } else {
        // Start with a heuristic targeting ~2000 slices.
        let target = 2000usize;
        let mut s = ((total_bytes as usize).div_ceil(target).max(64) / 64 * 64).max(64);
        let mut n = total_slice_count(files, s);

        // Grow slice size if we'd exceed the PAR2 hard limit.
        while n > 32768 {
            let next = s.saturating_mul(2);
            if next == s {
                break;
            }
            let n2 = total_slice_count(files, next);
            // One slice per non-empty file is the floor — growing the slice
            // cannot help a set with more files than the spec allows.
            if n2 >= n {
                break;
            }
            s = next;
            n = n2;
        }

        // Detect small-file padding inflation: many files smaller than slice_size
        // each consume a full slice, inflating the effective parity ratio well
        // beyond what the user requested. Halve the slice size until the ratio
        // is acceptable or we hit MAX_SLICES_PADDING_OPT.
        if total_bytes > 0 {
            let padded = total_padded_bytes(files, s);
            let ratio = padded as f64 / total_bytes as f64;

            if ratio > PADDING_RATIO_THRESHOLD {
                loop {
                    // Halve, keeping alignment to 64 bytes.
                    let s2 = ((s / 2) / 64 * 64).max(64);
                    if s2 >= s {
                        break; // already at minimum granularity
                    }
                    let n2 = total_slice_count(files, s2);
                    if n2 > MAX_SLICES_PADDING_OPT.min(32768) {
                        break; // would cost too much CPU
                    }
                    s = s2;
                    n = n2;
                    let ratio2 = total_padded_bytes(files, s) as f64 / total_bytes as f64;
                    if ratio2 <= PADDING_RATIO_THRESHOLD {
                        break; // good enough
                    }
                }
            }
        }

        (s, n)
    };

    let recovery_count = if let Some(n) = options.recovery_count {
        n
    } else {
        let proportional = (total_slices * options.recovery_pct as usize) / 100;
        if options.recovery_pct > 0 && total_slices > 0 {
            proportional.max(1)
        } else {
            proportional
        }
    };

    if total_slices > 32768 {
        anyhow::bail!("too many input slices: {total_slices} (max 32768)");
    }
    if recovery_count > 65535 {
        anyhow::bail!("too many recovery blocks: {recovery_count} (max 65535)");
    }

    Ok((slice_size, total_slices, recovery_count))
}

/// Chunk size for the streaming reader below, and the cutoff under which a
/// file is read as a single blocking call instead — see `ingest_files`.
const CHUNK_SIZE: usize = 8 * 1024 * 1024; // 8 MiB

/// Split `chunk` into `slice_size` pieces, handing each completed-and-since-
/// superseded slice to `worker` (holding the newest one back — see
/// `ingest_files`'s doc comment for why). Pure and synchronous: callers doing
/// this from async context are responsible for wrapping the call in
/// `tokio::task::block_in_place`, since `Par2Worker::send_slice` blocks.
struct HashSink {
    out: IngestHashes,
    current: crate::encoder::FileHasher,
}

fn emit_logical_slice(
    worker: &Par2Worker,
    logical: Vec<u8>,
    actual_len: usize,
    is_last: bool,
    window: Option<SliceWindow>,
    hash: Option<&mut HashSink>,
) {
    if let Some(h) = hash {
        let checksum = h.current.update_and_hash_slice(&logical, actual_len);
        h.out.checksums.push(checksum);
        if is_last {
            let finished = std::mem::take(&mut h.current);
            h.out.hashes.push(finished.finish());
        }
    }
    if let Some(w) = window {
        let mut part = worker.take_buffer(w.len);
        part.clear();
        if w.offset < logical.len() {
            let end = (w.offset + w.len).min(logical.len());
            part.extend_from_slice(&logical[w.offset..end]);
        }
        part.resize(w.len, 0);
        let win_actual = actual_len.saturating_sub(w.offset).min(w.len);
        worker.send_slice(part, win_actual, is_last);
    } else {
        worker.send_slice(logical, actual_len, is_last);
    }
}

fn feed_chunk(
    chunk: &[u8],
    worker: &Par2Worker,
    slice_size: usize,
    slice_accum: &mut Vec<u8>,
    held: &mut Option<Vec<u8>>,
    window: Option<SliceWindow>,
    mut hash: Option<&mut HashSink>,
) {
    let mut chunk_pos = 0;
    while chunk_pos < chunk.len() {
        let space = slice_size - slice_accum.len();
        let take = space.min(chunk.len() - chunk_pos);
        slice_accum.extend_from_slice(&chunk[chunk_pos..chunk_pos + take]);
        chunk_pos += take;

        if slice_accum.len() >= slice_size {
            let next = worker.take_buffer(slice_size);
            let padded = std::mem::replace(slice_accum, next);
            if let Some(previous) = held.replace(padded) {
                emit_logical_slice(
                    worker,
                    previous,
                    slice_size,
                    false,
                    window,
                    hash.as_deref_mut(),
                );
            }
        }
    }
}

/// Ingests files into a PAR2 worker.
///
/// Exactly one slice per file is flagged `is_last_of_file`, and it must be the
/// file's genuine final slice: [`Par2Worker`]'s hasher thread finalizes and
/// pushes a file's MD5/CRC only on that flag. Miss it and the hash bleeds into
/// the next file — or, for the last file in the set, the worker returns fewer
/// hashes than there are files and the caller indexes past the end.
///
/// Getting that right is why the last slice is held back rather than sent as
/// soon as it is full: only once the reader has hit EOF is it known whether
/// the slice just filled was the last one. Flagging only a *partial* trailing
/// slice — the obvious shortcut — silently breaks every file whose size is an
/// exact multiple of the slice size, which is not an edge case at all once a
/// caller passes an explicit `--slice-size` (see the regression test in
/// `tests/exact_multiple_slice_size.rs`, and the same bug's earlier sighting
/// in `pesto`'s poster, `crates/pesto/tests/par2_exact_multiple_of_slice_size.rs`).
///
/// See [`ingest_files_with`] for cancellation and per-file hooks.
pub async fn ingest_files(
    files: &[InputFile],
    worker: &Par2Worker,
    slice_size: usize,
) -> Result<()> {
    ingest_files_with(files, worker, slice_size, None, |_| Ok(())).await
}

/// [`ingest_files`] with optional cancel and a per-file hook so callers such
/// as `pesto` can drive progress without reimplementing the reader.
pub async fn ingest_files_with<F>(
    files: &[InputFile],
    worker: &Par2Worker,
    slice_size: usize,
    cancelled: Option<&AtomicBool>,
    after_file: F,
) -> Result<()>
where
    F: FnMut(&InputFile) -> Result<()>,
{
    ingest_files_ex(files, worker, slice_size, cancelled, after_file, None, None).await
}

/// [`ingest_files_with`] plus a callback after every input chunk is read.
///
/// The callback is intentionally byte-based: it stays responsive even when a
/// single logical PAR2 slice is very large.
pub async fn ingest_files_with_progress<F, P>(
    files: &[InputFile],
    worker: &Par2Worker,
    slice_size: usize,
    cancelled: Option<&AtomicBool>,
    after_file: F,
    on_read: P,
) -> Result<()>
where
    F: FnMut(&InputFile) -> Result<()>,
    P: FnMut(usize) -> Result<()>,
{
    ingest_files_ex_impl(
        files, worker, slice_size, cancelled, after_file, None, None, on_read,
    )
    .await
}

/// Like [`ingest_files_with`], optionally sending only a window of each
/// logical slice (P1b) and/or collecting file hashes + slice checksums in
/// the reader so the encoder can run on chunk-sized buffers.
pub async fn ingest_files_ex<F>(
    files: &[InputFile],
    worker: &Par2Worker,
    slice_size: usize,
    cancelled: Option<&AtomicBool>,
    after_file: F,
    window: Option<SliceWindow>,
    hash_out: Option<&mut IngestHashes>,
) -> Result<()>
where
    F: FnMut(&InputFile) -> Result<()>,
{
    ingest_files_ex_impl(
        files,
        worker,
        slice_size,
        cancelled,
        after_file,
        window,
        hash_out,
        |_| Ok(()),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn ingest_files_ex_impl<F, P>(
    files: &[InputFile],
    worker: &Par2Worker,
    slice_size: usize,
    cancelled: Option<&AtomicBool>,
    mut after_file: F,
    window: Option<SliceWindow>,
    mut hash_out: Option<&mut IngestHashes>,
    mut on_read: P,
) -> Result<()>
where
    F: FnMut(&InputFile) -> Result<()>,
    P: FnMut(usize) -> Result<()>,
{
    let mut hash = hash_out.as_mut().map(|_| HashSink {
        out: IngestHashes::default(),
        current: crate::encoder::FileHasher::new(),
    });
    for file_info in files {
        if cancelled.is_some_and(|c| c.load(Ordering::Relaxed)) {
            return Ok(());
        }
        // Empty files contribute no slices; the hasher only finalizes on
        // `is_last_of_file`, and callers insert the known empty-file hash.
        if file_info.size == 0 {
            continue;
        }
        let mut slice_accum = worker.take_buffer(slice_size);
        slice_accum.clear();
        // The most recently filled slice, not yet handed to the worker: it
        // cannot be sent until we know whether another slice follows it.
        let mut held: Option<Vec<u8>> = None;

        // A file that fits in one chunk has nothing for the double-buffered
        // path below to overlap — reading it is one syscall either way — so
        // route it through a single blocking read instead of paying a
        // spawn_blocking task and an mpsc channel for that one syscall.
        // Skipping this matters at scale: a corpus of thousands of
        // sub-slice files (see `bench/FINDINGS.md` §5, issue #131) pays that
        // task-spawn/channel-handoff cost once per file regardless of size,
        // and profiling showed it dominating wall time — most cores sitting
        // idle between the rare flushes big enough to need them — even
        // though the per-file work is tiny.
        if (file_info.size as usize) <= CHUNK_SIZE {
            let path = file_info.path.clone();
            tokio::task::block_in_place(|| -> Result<()> {
                let buf = std::fs::read(&path)
                    .with_context(|| format!("reading `{}`", path.display()))?;
                on_read(buf.len())?;
                feed_chunk(
                    &buf,
                    worker,
                    slice_size,
                    &mut slice_accum,
                    &mut held,
                    window,
                    hash.as_mut(),
                );
                Ok(())
            })?;
        } else {
            let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
            let path = file_info.path.clone();

            // Double-buffered reader task: fetch data while we process previous chunks.
            let reader_handle = tokio::task::spawn_blocking(move || {
                use std::fs::File;
                use std::io::Read;
                let mut file = File::open(&path)?;
                loop {
                    let mut buf = vec![0u8; CHUNK_SIZE];
                    let n = file.read(&mut buf)?;
                    if n == 0 {
                        break;
                    }
                    buf.truncate(n);
                    if tx.blocking_send(buf).is_err() {
                        break;
                    }
                }
                Ok::<_, anyhow::Error>(())
            });

            while let Some(chunk) = rx.recv().await {
                if cancelled.is_some_and(|c| c.load(Ordering::Relaxed)) {
                    drop(rx);
                    let _ = reader_handle.await;
                    return Ok(());
                }
                on_read(chunk.len())?;
                tokio::task::block_in_place(|| {
                    feed_chunk(
                        &chunk,
                        worker,
                        slice_size,
                        &mut slice_accum,
                        &mut held,
                        window,
                        hash.as_mut(),
                    );
                });
            }

            reader_handle.await??;
        }

        if !slice_accum.is_empty() {
            // A partial trailing slice: it is the last one, and anything held
            // back is not.
            if let Some(previous) = held.take() {
                tokio::task::block_in_place(|| {
                    emit_logical_slice(worker, previous, slice_size, false, window, hash.as_mut());
                });
            }
            let actual_len = slice_accum.len();
            slice_accum.resize(slice_size, 0);
            tokio::task::block_in_place(|| {
                emit_logical_slice(worker, slice_accum, actual_len, true, window, hash.as_mut());
            });
        } else if let Some(last) = held.take() {
            tokio::task::block_in_place(|| {
                emit_logical_slice(worker, last, slice_size, true, window, hash.as_mut());
            });
        }

        after_file(file_info)?;
    }

    if let (Some(dst), Some(src)) = (hash_out.as_mut(), hash) {
        **dst = src.out;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_files(sizes: &[u64]) -> Vec<InputFile> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| InputFile {
                path: format!("file{i}").into(),
                display_name: format!("file{i}"),
                size,
            })
            .collect()
    }

    fn opts() -> CreateOptions {
        CreateOptions::default()
    }

    #[test]
    fn memory_layout_fits_in_one_pass() {
        let plan = plan_memory_layout(1 << 20, 200, 1 << 30);
        assert_eq!(plan.recovery_per_pass, 200);
        assert_eq!(plan.slice_chunk, 1 << 20);
    }

    #[test]
    fn memory_layout_prefers_slice_chunk_when_all_dests_fit() {
        // 1000 × 6 MiB = 6 GiB; 1 GiB limit → 1 MiB windows, all dests.
        let slice = 6 * 1024 * 1024;
        let plan = plan_memory_layout(slice, 1000, 1024 * 1024 * 1024);
        assert_eq!(plan.recovery_per_pass, 1000);
        assert!(plan.slice_chunk < slice);
        assert!(plan.slice_chunk >= 4096);
        assert!(plan.slice_chunk.is_multiple_of(32));
        assert!(1000 * plan.slice_chunk <= 1024 * 1024 * 1024);
    }

    #[test]
    fn memory_layout_falls_back_to_recovery_passes_when_chunk_tiny() {
        // Tiny limit: cannot hold 4 KiB × all dests.
        let plan = plan_memory_layout(1 << 20, 10_000, 8 * 1024);
        assert_eq!(plan.slice_chunk, 1 << 20);
        assert!(plan.recovery_per_pass < 10_000);
        assert!(plan.recovery_per_pass >= 1);
    }

    #[test]
    fn geometry_stays_within_par2_limits() {
        // 100 files of 1 GiB each — stresses the 32k slice ceiling.
        let files = make_files(&vec![1024 * 1024 * 1024; 100]);
        let (_, total_slices, recovery_count) = calculate_geometry(&files, &opts()).unwrap();
        assert!(total_slices <= 32768, "total_slices={total_slices}");
        assert!(recovery_count <= 65535, "recovery_count={recovery_count}");
    }

    #[test]
    fn geometry_reduces_slice_for_many_small_files() {
        // Simulates a Blu-ray disc: 3 large .m2ts + 200 tiny support files.
        let mut sizes: Vec<u64> = vec![5 * 1024 * 1024 * 1024; 3]; // 3 × 5 GiB
        sizes.extend(vec![50 * 1024; 200]); // 200 × 50 KiB

        let files = make_files(&sizes);
        let total_actual: u64 = sizes.iter().sum();

        let (slice_size, total_slices, _) = calculate_geometry(&files, &opts()).unwrap();

        let total_padded: u64 = files
            .iter()
            .map(|f| (f.size as usize).div_ceil(slice_size) as u64 * slice_size as u64)
            .sum();
        let ratio = total_padded as f64 / total_actual as f64;

        assert!(total_slices <= 32768, "total_slices={total_slices}");
        // Padding overhead should be within 15% of actual data.
        assert!(
            ratio <= 1.15,
            "padding ratio {ratio:.3} exceeds threshold (slice_size={slice_size}, slices={total_slices})"
        );
    }

    #[test]
    fn geometry_does_not_over_optimize_clean_sets() {
        // A single large file has negligible padding at the heuristic slice size —
        // the optimiser should leave the slice size alone.
        let files = make_files(&[8 * 1024 * 1024 * 1024]);
        let (_, total_slices, _) = calculate_geometry(&files, &opts()).unwrap();
        // Should stay near ~2000 slices, not balloon to 32768.
        assert!(total_slices <= 6_000, "total_slices={total_slices}");
    }

    #[test]
    fn explicit_slice_size_is_respected() {
        let files = make_files(&[100 * 1024 * 1024]);
        let mut o = opts();
        o.slice_size = Some(512 * 1024);
        let (slice_size, _, _) = calculate_geometry(&files, &o).unwrap();
        assert_eq!(slice_size, 512 * 1024);
    }

    #[test]
    fn positive_percentage_produces_recovery_for_tiny_set() {
        let files = make_files(&[1]);
        let mut o = opts();
        o.recovery_pct = 1;
        let (_, total_slices, recovery_count) = calculate_geometry(&files, &o).unwrap();
        assert_eq!(total_slices, 1);
        assert_eq!(recovery_count, 1);
    }

    #[test]
    fn explicit_zero_recovery_count_remains_zero() {
        let files = make_files(&[1]);
        let mut o = opts();
        o.recovery_pct = 10;
        o.recovery_count = Some(0);
        let (_, _, recovery_count) = calculate_geometry(&files, &o).unwrap();
        assert_eq!(recovery_count, 0);
    }

    #[test]
    fn sort_files_by_file_id_orders_by_ascending_file_id_not_input_order() {
        let dir = std::env::temp_dir().join(format!(
            "parmesan-ops-test-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        // Content (and therefore File ID) is unrelated to file name or the
        // order files are listed in — that's exactly the case this fix
        // matters for.
        let names = ["zzz.bin", "aaa.bin", "mmm.bin"];
        let mut files = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let path = dir.join(name);
            let data = vec![i as u8; 100 + i * 37];
            std::fs::write(&path, &data).unwrap();
            files.push(InputFile {
                path,
                display_name: (*name).to_string(),
                size: data.len() as u64,
            });
        }

        // Compute the expected order independently of the function under test.
        let mut expected_ids = Vec::new();
        for f in &files {
            let bytes = std::fs::read(&f.path).unwrap();
            let md5_16k = crate::packet::md5(&bytes);
            expected_ids.push(crate::packet::compute_file_id(
                &md5_16k,
                f.size,
                &f.display_name,
            ));
        }
        expected_ids.sort();

        sort_files_by_file_id(&mut files).unwrap();

        let got_ids: Vec<[u8; 16]> = files
            .iter()
            .map(|f| {
                let bytes = std::fs::read(&f.path).unwrap();
                let md5_16k = crate::packet::md5(&bytes);
                crate::packet::compute_file_id(&md5_16k, f.size, &f.display_name)
            })
            .collect();
        assert_eq!(got_ids, expected_ids);
        assert!(got_ids.windows(2).all(|w| w[0] <= w[1]));

        std::fs::remove_dir_all(&dir).ok();
    }
}
