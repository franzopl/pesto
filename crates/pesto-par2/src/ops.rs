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

/// Returns the smallest slice size (multiple of 64) that satisfies PAR2 limits.
pub fn calculate_geometry(
    files: &[InputFile],
    options: &CreateOptions,
) -> Result<(usize, usize, usize)> {
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();

    let (slice_size, total_slices) = if let Some(s) = options.slice_size {
        let s = (s / 64 * 64).max(64);
        let n: usize = files.iter().map(|f| (f.size as usize).div_ceil(s)).sum();
        (s, n)
    } else if let Some(count) = options.slice_count {
        let s = ((total_bytes as usize).div_ceil(count.max(1)) / 64 * 64).max(64);
        let n: usize = files.iter().map(|f| (f.size as usize).div_ceil(s)).sum();
        (s, n)
    } else {
        // Pesto's heuristic: target ~1000 slices, but stay within 32k limits.
        let target = 1000usize;
        let mut s = ((total_bytes as usize).div_ceil(target).max(64) / 64 * 64).max(64);
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
