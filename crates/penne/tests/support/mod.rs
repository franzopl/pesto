//! Shared test-only PAR2 fixture builder, used by more than one integration
//! test file (hence `tests/support/mod.rs`, not a top-level `tests/*.rs`,
//! which Cargo would otherwise treat as its own test binary).
//!
//! Drives the real PAR2 encoder and packet writers via `pesto::par2`'s fully
//! public API, adapted from `crates/parmesan/src/test_support.rs` (which is
//! `pub(crate)` there and not reachable from another crate). This proves
//! `penne`'s PAR2 integration against genuine on-disk PAR2 bytes rather than
//! a fake in-memory `RecoverySet`.

use std::path::PathBuf;

use pesto::par2::encoder::{FileHasher, RecoveryEncoder};
use pesto::par2::packet;

pub struct FixtureFile {
    pub name: &'static str,
    pub data: Vec<u8>,
}

/// Build a small PAR2 recovery set (`base.par2` index + one recovery
/// volume, when `recovery_count > 0`) under a fresh temp directory. Returns
/// the directory path; the caller is responsible for removing it when done.
pub fn build_fixture_set(
    files: &[FixtureFile],
    slice_size: usize,
    recovery_count: usize,
) -> PathBuf {
    let dir = tempfile::tempdir().unwrap().keep();

    for f in files {
        std::fs::write(dir.join(f.name), &f.data).unwrap();
    }

    let total_slices: usize = files
        .iter()
        .map(|f| f.data.len().div_ceil(slice_size))
        .sum();

    let mut enc =
        RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count).with_checksums();

    let mut hashes = Vec::new();
    let mut slice_counts = Vec::new();
    for f in files {
        let mut hasher = FileHasher::new();
        let n_slices = f.data.len().div_ceil(slice_size);
        let mut pos = 0usize;
        for _ in 0..n_slices {
            let end = (pos + slice_size).min(f.data.len());
            let chunk = &f.data[pos..end];
            hasher.update(chunk);
            let mut padded = vec![0u8; slice_size];
            padded[..chunk.len()].copy_from_slice(chunk);
            enc.add_slice(padded);
            pos = end;
        }
        hashes.push(hasher.finish());
        slice_counts.push(n_slices);
    }

    let (recovery_slices, all_checksums) = enc.finish();

    let file_ids: Vec<[u8; 16]> = files
        .iter()
        .enumerate()
        .map(|(idx, f)| {
            let h = &hashes[idx];
            packet::compute_file_id(&h.md5_16k, h.length, f.name)
        })
        .collect();

    let main_b = packet::main_body(slice_size as u64, &file_ids);
    let rsid = packet::recovery_set_id(&main_b);

    let mut index_bytes = Vec::new();
    index_bytes.extend(packet::serialize_packet(&rsid, &packet::TYPE_MAIN, &main_b));
    index_bytes.extend(packet::serialize_packet(
        &rsid,
        &packet::TYPE_CREATOR,
        &packet::creator_body("penne-tests"),
    ));

    let mut cursor = 0usize;
    for (idx, f) in files.iter().enumerate() {
        let fid = &file_ids[idx];
        let h = &hashes[idx];
        index_bytes.extend(packet::serialize_packet(
            &rsid,
            &packet::TYPE_FILE_DESC,
            &packet::file_description_body(fid, &h.md5_full, &h.md5_16k, h.length, f.name),
        ));
        let n = slice_counts[idx];
        let slices = &all_checksums[cursor..cursor + n];
        cursor += n;
        index_bytes.extend(packet::serialize_packet(
            &rsid,
            &packet::TYPE_IFSC,
            &packet::ifsc_body(fid, slices),
        ));
    }

    std::fs::write(dir.join("base.par2"), &index_bytes).unwrap();

    if !recovery_slices.is_empty() {
        let mut vol_bytes = index_bytes.clone();
        for slice in &recovery_slices {
            vol_bytes.extend(packet::serialize_packet(
                &rsid,
                &packet::TYPE_RECOVERY,
                &packet::recovery_body(slice.exponent, &slice.data),
            ));
        }
        let vol_path = dir.join(format!("base.vol000+{:03}.par2", recovery_slices.len()));
        std::fs::write(&vol_path, &vol_bytes).unwrap();
    }

    dir
}
