//! Test-only fixtures: build a small, real PAR2 recovery set on disk by
//! driving the actual encoder and packet writers, mirroring the packet
//! sequence `main.rs` produces for `create`. Used by `recovery_set` and
//! `verify` unit tests so they exercise real on-disk bytes instead of
//! hand-built structs.

use crate::encoder::{FileHasher, RecoveryEncoder};
use crate::packet;
use std::path::PathBuf;

/// One input file to seed into a fixture recovery set.
pub(crate) struct FixtureFile {
    pub name: &'static str,
    pub data: Vec<u8>,
}

/// Build a tiny PAR2 recovery set (`base.par2` index + a single recovery
/// volume, when `recovery_count > 0`) under a fresh temp directory.
///
/// Returns `(directory, index_file_path)`. The caller is responsible for
/// removing the directory when done (`std::fs::remove_dir_all`).
///
/// # Panics
///
/// Panics on any I/O failure — this is test-only scaffolding, not a path
/// meant to handle real-world error conditions.
pub(crate) fn build_fixture_set(
    dir_name: &str,
    files: &[FixtureFile],
    slice_size: usize,
    recovery_count: usize,
) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "parmesan-test-{dir_name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();

    for f in files {
        std::fs::write(dir.join(f.name), &f.data).unwrap();
    }

    // Mirror `main.rs`'s `sort_files_by_file_id` step: Reed-Solomon
    // coefficients are assigned by ascending File ID, not input order (see
    // `ops::sort_files_by_file_id` and `ROADMAP.md` Phase 22). Fixtures with
    // more than one file must encode in this same order or `RecoverySet`
    // (which always presents files in File-ID order) would disagree with
    // what was actually encoded.
    let mut order: Vec<usize> = (0..files.len()).collect();
    let file_ids_for_sort: Vec<[u8; 16]> = files
        .iter()
        .map(|f| {
            let head_len = f.data.len().min(16 * 1024);
            let md5_16k = packet::md5(&f.data[..head_len]);
            packet::compute_file_id(&md5_16k, f.data.len() as u64, f.name)
        })
        .collect();
    order.sort_by_key(|&i| file_ids_for_sort[i]);
    let files: Vec<&FixtureFile> = order.into_iter().map(|i| &files[i]).collect();

    let total_slices: usize = files
        .iter()
        .map(|f| f.data.len().div_ceil(slice_size))
        .sum();

    let mut enc =
        RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count).with_checksums();

    let mut hashes = Vec::new();
    let mut slice_counts = Vec::new();
    for f in &files {
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

    let mut file_ids = Vec::new();
    for (idx, f) in files.iter().enumerate() {
        let h = &hashes[idx];
        file_ids.push(packet::compute_file_id(&h.md5_16k, h.length, f.name));
    }

    let main_b = packet::main_body(slice_size as u64, &file_ids);
    let rsid = packet::recovery_set_id(&main_b);

    let mut index_bytes = Vec::new();
    index_bytes.extend(packet::serialize_packet(&rsid, &packet::TYPE_MAIN, &main_b));
    index_bytes.extend(packet::serialize_packet(
        &rsid,
        &packet::TYPE_CREATOR,
        &packet::creator_body("parmesan-tests"),
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

    let index_path = dir.join("base.par2");
    std::fs::write(&index_path, &index_bytes).unwrap();

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

    (dir, index_path)
}
