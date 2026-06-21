use crate::worker::Par2Worker;
use crate::SimdPath;
use anyhow::Result;
use std::path::PathBuf;

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

/// Metadata for an input file to be protected by PAR2.
#[derive(Debug, Clone)]
pub struct InputFile {
    pub path: PathBuf,
    pub display_name: String,
    pub size: u64,
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
            s *= 2;
            n = total_slice_count(files, s);
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
        (total_slices * options.recovery_pct as usize) / 100
    };

    if total_slices > 32768 {
        anyhow::bail!("too many input slices: {total_slices} (max 32768)");
    }
    if recovery_count > 65535 {
        anyhow::bail!("too many recovery blocks: {recovery_count} (max 65535)");
    }

    Ok((slice_size, total_slices, recovery_count))
}

/// Ingests files into a PAR2 worker.
pub async fn ingest_files(
    files: &[InputFile],
    worker: &Par2Worker,
    slice_size: usize,
) -> Result<()> {
    for file_info in files {
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(4);
        let path = file_info.path.clone();

        // Double-buffered reader task: fetch data while we process previous chunks.
        let reader_handle = tokio::task::spawn_blocking(move || {
            use std::fs::File;
            use std::io::Read;
            let mut file = File::open(&path)?;
            loop {
                let mut buf = vec![0u8; 8 * 1024 * 1024]; // 8 MiB chunks
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

        let mut slice_accum = worker.take_buffer(slice_size);
        slice_accum.clear();

        while let Some(chunk) = rx.recv().await {
            let mut chunk_pos = 0;
            while chunk_pos < chunk.len() {
                let space = slice_size - slice_accum.len();
                let take = space.min(chunk.len() - chunk_pos);
                slice_accum.extend_from_slice(&chunk[chunk_pos..chunk_pos + take]);
                chunk_pos += take;

                if slice_accum.len() >= slice_size {
                    let next = worker.take_buffer(slice_size);
                    let padded = std::mem::replace(&mut slice_accum, next);
                    tokio::task::block_in_place(|| {
                        worker.send_slice(padded, slice_size, false);
                    });
                }
            }
        }

        reader_handle.await??;

        if !slice_accum.is_empty() {
            let actual_len = slice_accum.len();
            slice_accum.resize(slice_size, 0);
            tokio::task::block_in_place(|| worker.send_slice(slice_accum, actual_len, true));
        }
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
}
