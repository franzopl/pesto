//! Regression guard: a file whose size is an exact multiple of the slice size.
//!
//! `ops::ingest_files` marks one slice per file `is_last_of_file`, and
//! `worker`'s hasher thread pushes that file's MD5/CRC only when it sees the
//! flag. The flag used to be attached solely to a *partial* trailing slice, so
//! any file that divided evenly into slices produced no hash at all: the
//! recovery set was then built by indexing a `Vec<FileHashes>` that was short
//! by one entry per such file, and `parmesan create` aborted with
//! `index out of bounds: the len is 0 but the index is 0`.
//!
//! Nothing exotic triggers it. Passing an explicit `--slice-size` (or
//! `--slice-count`) makes exact division the normal case rather than a
//! coincidence, which is how `bench/suites/20-par2.sh` — which computes one
//! shared, 4 KiB-aligned geometry for every tool it compares — hit it on its
//! first run.
//!
//! `pesto`'s own posting path had the same bug in its separate accumulation
//! loop and was fixed earlier; see
//! `crates/pesto/tests/par2_exact_multiple_of_slice_size.rs`. This covers the
//! `parmesan` library/CLI path, which that fix did not touch.

use parmesan::encoder::RecoveryEncoder;
use parmesan::ops::{ingest_files, InputFile};
use parmesan::worker::Par2Worker;

use std::io::Write;
use std::path::PathBuf;

const SLICE: usize = 4096;

/// Scratch directory, made the same way `par2cmdline_compat.rs` does it —
/// this crate deliberately carries no `tempfile` dev-dependency.
struct ScratchDir(PathBuf);

impl ScratchDir {
    fn new(name: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "parmesan-exact-multiple-{name}-{}-{}",
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
        // Deterministic, non-zero content: an all-zero file would hash the
        // same whether or not the final slice was ever fed in, hiding the
        // very failure this test exists to catch.
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        f.write_all(&data).unwrap();
        f.flush().unwrap();
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

/// Ingest `files` and return how many per-file hashes the worker produced.
fn hash_count(files: &[InputFile], total_slices: usize) -> usize {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        let enc = RecoveryEncoder::new_smart(SLICE, total_slices, 0, 1).with_checksums();
        let worker = Par2Worker::spawn(enc, true, parmesan::worker::DEFAULT_CHANNEL_DEPTH);
        ingest_files(files, &worker, SLICE).await.unwrap();
        let (_slices, _checksums, hashes) = tokio::task::block_in_place(|| worker.finish());
        hashes.len()
    })
}

#[test]
fn single_file_of_exactly_one_slice_yields_a_hash() {
    let dir = ScratchDir::new("one");
    let files = vec![dir.file("one.bin", SLICE)];
    assert_eq!(hash_count(&files, 1), 1);
}

#[test]
fn single_file_of_several_whole_slices_yields_a_hash() {
    let dir = ScratchDir::new("many");
    let files = vec![dir.file("many.bin", SLICE * 8)];
    assert_eq!(hash_count(&files, 8), 1);
}

#[test]
fn every_file_in_a_whole_slice_multiple_set_yields_a_hash() {
    // The multi-file case used to corrupt rather than crash: with no
    // end-of-file marker the running hasher carried straight on into the next
    // file, so the hashes that *were* produced described the wrong bytes.
    let dir = ScratchDir::new("set");
    let files = vec![
        dir.file("a.bin", SLICE),
        dir.file("b.bin", SLICE * 2),
        dir.file("c.bin", SLICE * 3),
    ];
    assert_eq!(hash_count(&files, 6), 3);
}

#[test]
fn an_empty_file_yields_no_hash() {
    // A zero-length file has zero slices (PAR2 spec: `ceil(length / slice)`),
    // so the worker never sees an end-of-file marker for it and returns no
    // hash. That is the invariant `parmesan create` relies on: it walks the
    // worker's hashes as a stream over the *non-empty* files and synthesizes
    // the MD5-of-nothing for the rest. Indexing the hash vector by file
    // position instead used to panic on any set containing an empty file.
    let dir = ScratchDir::new("empty");
    let files = vec![
        dir.file("data.bin", SLICE * 2),
        dir.file("nothing.bin", 0),
        dir.file("more.bin", SLICE),
    ];
    assert_eq!(hash_count(&files, 3), 2);
}

#[test]
fn a_partial_trailing_slice_still_yields_exactly_one_hash_per_file() {
    // The path that always worked, kept so a future change to the exact
    // multiple case cannot regress it into emitting two hashes for one file.
    let dir = ScratchDir::new("mixed");
    let files = vec![
        dir.file("ragged.bin", SLICE * 2 + 17),
        dir.file("aligned.bin", SLICE * 2),
    ];
    assert_eq!(hash_count(&files, 5), 2);
}
