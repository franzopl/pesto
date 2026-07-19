//! Integration test: `penne::quickcheck::looks_intact` against a real,
//! on-disk PAR2 index built by the actual encoder/packet-writer API — not
//! hand-built `FileEntry`/`SliceChecksum` structs. See `tests/support/mod.rs`
//! for how the fixture is built.

mod support;

use pesto::par2::recovery_set::RecoverySet;
use pesto::yenc::crc32;
use support::{build_fixture_set, FixtureFile};

#[test]
fn matches_a_real_file_whose_length_is_not_a_multiple_of_slice_size() {
    // 44 bytes over a 16-byte slice_size: two full slices plus a 12-byte
    // tail slice that the real PAR2 encoder zero-pads before hashing —
    // exactly the case `pad_crc32_to_slice_boundary` exists for.
    let data: Vec<u8> = (0..44u32).map(|i| (i * 5 + 1) as u8).collect();
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: data.clone(),
        }],
        16,
        0, // no recovery blocks needed - only the index/IFSC data is used
    );

    let set = RecoverySet::load(dir.join("base.par2")).unwrap();
    assert_eq!(set.files.len(), 1);
    assert_eq!(set.files[0].slice_checksums.len(), 3); // ceil(44/16)

    let real_crc32 = crc32(&data);
    assert_eq!(
        penne::quickcheck::looks_intact(&set.files[0], set.slice_size, real_crc32),
        Some(true)
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn matches_a_real_file_whose_length_is_an_exact_multiple_of_slice_size() {
    let data: Vec<u8> = (0..32u32).map(|i| (i * 3) as u8).collect();
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: data.clone(),
        }],
        16,
        0,
    );

    let set = RecoverySet::load(dir.join("base.par2")).unwrap();
    assert_eq!(set.files[0].slice_checksums.len(), 2); // exactly 32/16

    let real_crc32 = crc32(&data);
    assert_eq!(
        penne::quickcheck::looks_intact(&set.files[0], set.slice_size, real_crc32),
        Some(true)
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn detects_corruption_against_a_real_par2_index() {
    let data: Vec<u8> = (0..44u32).map(|i| (i * 5 + 1) as u8).collect();
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: data.clone(),
        }],
        16,
        0,
    );

    let set = RecoverySet::load(dir.join("base.par2")).unwrap();

    let mut corrupted = data.clone();
    corrupted[10] ^= 0xFF;
    let wrong_crc32 = crc32(&corrupted);

    assert_eq!(
        penne::quickcheck::looks_intact(&set.files[0], set.slice_size, wrong_crc32),
        Some(false)
    );

    std::fs::remove_dir_all(&dir).ok();
}
