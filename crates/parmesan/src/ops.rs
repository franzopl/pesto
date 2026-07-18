use crate::packet;
use crate::worker::Par2Worker;
use crate::SimdPath;
use anyhow::{Context, Result};
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
