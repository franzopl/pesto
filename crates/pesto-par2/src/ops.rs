use crate::worker::Par2Worker;
use crate::SimdPath;
use anyhow::{Context, Result};
use std::path::PathBuf;
use tokio::fs::File;
use tokio::io::AsyncReadExt;

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

/// Returns the smallest slice size (multiple of 4) that satisfies PAR2 limits.
pub fn calculate_geometry(
    files: &[InputFile],
    options: &CreateOptions,
) -> Result<(usize, usize, usize)> {
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();

    let (slice_size, total_slices) = if let Some(s) = options.slice_size {
        let s = (s / 4 * 4).max(4);
        let n: usize = files.iter().map(|f| (f.size as usize).div_ceil(s)).sum();
        (s, n)
    } else if let Some(count) = options.slice_count {
        let s = ((total_bytes as usize).div_ceil(count.max(1)) / 4 * 4).max(4);
        let n: usize = files.iter().map(|f| (f.size as usize).div_ceil(s)).sum();
        (s, n)
    } else {
        // Pesto's heuristic: target ~1000 slices, but stay within 32k limits.
        let target = 1000usize;
        let mut s = ((total_bytes as usize).div_ceil(target).max(4) / 4 * 4).max(4);
        let mut n: usize = files.iter().map(|f| (f.size as usize).div_ceil(s)).sum();

        // If we exceed 32768 slices, increase slice size until we fit.
        while n > 32768 {
            s *= 2;
            n = files.iter().map(|f| (f.size as usize).div_ceil(s)).sum();
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

/// Ingests files into a PAR2 worker, computing MD5 hashes in parallel if requested.
pub async fn ingest_files(
    files: &[InputFile],
    worker: &Par2Worker,
    slice_size: usize,
    compute_hashes: bool,
) -> Result<Option<Vec<crate::encoder::FileHashes>>> {
    let mut all_hashes = if compute_hashes {
        Some(Vec::with_capacity(files.len()))
    } else {
        None
    };

    for file_info in files {
        let mut file = File::open(&file_info.path)
            .await
            .with_context(|| format!("opening `{}`", file_info.path.display()))?;

        let mut current_hasher = if compute_hashes {
            Some(crate::encoder::FileHasher::new())
        } else {
            None
        };

        let mut slice_buf = worker.take_buffer(slice_size);
        slice_buf.clear();

        let mut remaining = file_info.size as usize;
        while remaining > 0 {
            let space = slice_size - slice_buf.len();
            let to_read = space.min(remaining);

            let base = slice_buf.len();
            slice_buf.reserve(to_read);
            let dst = unsafe {
                std::slice::from_raw_parts_mut(slice_buf.as_mut_ptr().add(base), to_read)
            };
            file.read_exact(dst)
                .await
                .with_context(|| format!("reading `{}`", file_info.path.display()))?;

            if let Some(h) = &mut current_hasher {
                h.update(dst);
            }

            unsafe { slice_buf.set_len(base + to_read) };
            remaining -= to_read;

            if slice_buf.len() >= slice_size {
                let next = worker.take_buffer(slice_size);
                let padded = std::mem::replace(&mut slice_buf, next);
                tokio::task::block_in_place(|| worker.send_slice(padded, slice_size, remaining == 0));
            }
        }

        if let (Some(hashes), Some(h)) = (&mut all_hashes, current_hasher) {
            hashes.push(h.finish());
        }

        if !slice_buf.is_empty() {
            let actual_len = slice_buf.len();
            slice_buf.resize(slice_size, 0);
            tokio::task::block_in_place(|| worker.send_slice(slice_buf, actual_len, true));
        }
    }

    Ok(all_hashes)
}
