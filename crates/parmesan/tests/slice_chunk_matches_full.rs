//! P1b: windowed ingest + chunk-sized encoder must match a full-slice pass.

use parmesan::encoder::RecoveryEncoder;
use parmesan::ops::{
    ingest_files, ingest_files_ex, plan_memory_layout, IngestHashes, InputFile, SliceWindow,
};
use parmesan::worker::Par2Worker;

use std::io::Write;
use std::path::PathBuf;

struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "parmesan-slice-chunk-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str, len: usize) -> InputFile {
        let path = self.0.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        let data: Vec<u8> = (0..len).map(|i| (i.wrapping_mul(31) % 251) as u8).collect();
        f.write_all(&data).unwrap();
        InputFile {
            path,
            display_name: name.to_string(),
            size: len as u64,
        }
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[test]
fn slice_chunk_recovery_matches_full_slice_encoder() {
    let slice = 8192usize;
    let recovery = 8usize;
    // Force windows: 8 × 8 KiB = 64 KiB; limit 16 KiB → 2 KiB would be too
    // small (fallback). Use 40 KiB so chunk ≥ 4 KiB and all dests stay.
    let plan = plan_memory_layout(slice, recovery, 40 * 1024);
    assert_eq!(plan.recovery_per_pass, recovery);
    assert!(plan.slice_chunk < slice);
    assert!(plan.slice_chunk >= 4096);

    let dir = ScratchDir::new();
    let files = vec![dir.file("a.bin", slice * 3 + 100)];
    let total_slices = 4;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    let (full, full_cs, full_h) = runtime.block_on(async {
        let enc = RecoveryEncoder::new_smart(slice, total_slices, 0, recovery).with_checksums();
        let worker = Par2Worker::spawn(enc, true, 4);
        ingest_files(&files, &worker, slice).await.unwrap();
        tokio::task::block_in_place(|| worker.finish())
    });

    let (chunked, chunk_cs, chunk_h) = runtime.block_on(async {
        let mut ingest_h = IngestHashes::default();
        let mut acc = vec![Vec::new(); recovery];
        let mut off = 0usize;
        while off < slice {
            let win = plan.slice_chunk.min(slice - off);
            let enc = RecoveryEncoder::new_smart(win, total_slices, 0, recovery);
            let worker = Par2Worker::spawn(enc, false, 4);
            let slot = (off == 0).then_some(&mut ingest_h);
            ingest_files_ex(
                &files,
                &worker,
                slice,
                None,
                |_| Ok(()),
                Some(SliceWindow {
                    offset: off,
                    len: win,
                }),
                slot,
            )
            .await
            .unwrap();
            let (part, _, _) = tokio::task::block_in_place(|| worker.finish());
            for (i, sl) in part.into_iter().enumerate() {
                acc[i].extend_from_slice(&sl.data);
            }
            off += win;
        }
        (acc, ingest_h.checksums, ingest_h.hashes)
    });

    assert_eq!(full.len(), chunked.len());
    for (i, sl) in full.iter().enumerate() {
        assert_eq!(sl.data, chunked[i], "recovery block {i}");
    }
    assert_eq!(full_h.len(), chunk_h.len());
    for (a, b) in full_h.iter().zip(&chunk_h) {
        assert_eq!(a.md5_full, b.md5_full);
        assert_eq!(a.md5_16k, b.md5_16k);
        assert_eq!(a.length, b.length);
    }
    assert_eq!(full_cs.len(), chunk_cs.len());
    for (i, (a, b)) in full_cs.iter().zip(&chunk_cs).enumerate() {
        assert_eq!(a.md5, b.md5, "slice checksum {i}");
        assert_eq!(a.crc32, b.crc32, "slice crc {i}");
    }
}
