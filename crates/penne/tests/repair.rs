//! Integration test: `penne::repair::verify_and_repair` against real PAR2
//! recovery sets — not hand-built structs. See `tests/support/mod.rs` for
//! how the fixtures are built (drives the real PAR2 encoder via
//! `pesto::par2`'s public API).

mod support;

use std::collections::HashMap;

use penne::assemble::AssembleOutcome;
use penne::repair::{verify_and_repair, RepairOutcome};
use support::{build_fixture_set, FixtureFile};

#[tokio::test]
async fn intact_files_report_ok() {
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: vec![7u8; 500],
        }],
        128,
        4,
    );

    let outcome = verify_and_repair(&dir, &HashMap::new()).await.unwrap();
    assert!(matches!(outcome, RepairOutcome::Ok));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn recreates_a_file_left_unwritten_by_assemble() {
    // Simulates `AssembleOutcome::Incomplete`: assemble never wrote the
    // file at all because segments were missing. PAR2 can still recreate
    // it whole from recovery blocks, without any reassembly.
    let original: Vec<u8> = (0..777u32).map(|i| (i * 3) as u8).collect();
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: original.clone(),
        }],
        128,
        8,
    );
    std::fs::remove_file(dir.join("a.bin")).unwrap();

    let outcome = verify_and_repair(&dir, &HashMap::new()).await.unwrap();
    match outcome {
        RepairOutcome::Repaired(plan) => {
            assert_eq!(plan.repaired_files.len(), 1);
            assert_eq!(plan.repaired_files[0].name, "a.bin");
        }
        other => panic!("expected Repaired, got {other:?}"),
    }
    assert_eq!(std::fs::read(dir.join("a.bin")).unwrap(), original);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn patches_a_file_damaged_in_transit() {
    // Simulates `AssembleOutcome::ChecksumMismatch`: the file was written
    // but a byte got corrupted somewhere along the way.
    let original: Vec<u8> = (0..500u32).map(|i| i as u8).collect();
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: original.clone(),
        }],
        128,
        4,
    );
    let path = dir.join("a.bin");
    let mut corrupted = std::fs::read(&path).unwrap();
    corrupted[10] ^= 0xFF;
    std::fs::write(&path, &corrupted).unwrap();

    let outcome = verify_and_repair(&dir, &HashMap::new()).await.unwrap();
    assert!(matches!(outcome, RepairOutcome::Repaired(_)));
    assert_eq!(std::fs::read(&path).unwrap(), original);

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn reports_not_repairable_when_damage_exceeds_recovery_data() {
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: vec![9u8; 500],
        }],
        128,
        1, // only one recovery block for 4 slices
    );
    let path = dir.join("a.bin");
    std::fs::write(&path, vec![0u8; 500]).unwrap(); // wipe every slice

    let outcome = verify_and_repair(&dir, &HashMap::new()).await.unwrap();
    assert!(matches!(outcome, RepairOutcome::NotRepairable(_)));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn reports_no_recovery_data_when_no_par2_file_is_present() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("movie.mkv"), b"just a file, no par2").unwrap();

    let outcome = verify_and_repair(dir.path(), &HashMap::new())
        .await
        .unwrap();
    assert!(matches!(outcome, RepairOutcome::NoRecoveryData));
}

#[tokio::test]
async fn quick_check_reports_ok_when_assembled_crc32_matches_par2_data() {
    // The PAR2 quick-check path: supplying assemble()'s own already-known
    // CRC-32 lets verify_and_repair report Ok without a real par2_verify
    // pass ever running — this only proves the wiring reaches that path
    // and returns the right outcome (the quick-check math itself is
    // covered directly in tests/quickcheck.rs), not that no bytes were
    // read; that's an implementation detail, not part of the public API.
    let data: Vec<u8> = (0..500u32).map(|i| i as u8).collect();
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: data.clone(),
        }],
        128,
        4,
    );

    let mut assembled = HashMap::new();
    assembled.insert(
        "a.bin".to_string(),
        AssembleOutcome::Complete {
            actual_crc32: pesto::yenc::crc32(&data),
        },
    );

    let outcome = verify_and_repair(&dir, &assembled).await.unwrap();
    assert!(matches!(outcome, RepairOutcome::Ok));

    std::fs::remove_dir_all(&dir).ok();
}

#[tokio::test]
async fn a_known_crc32_that_does_not_match_par2_data_falls_back_to_a_real_repair() {
    // The claimed CRC-32 doesn't match what PAR2 expects (simulating, e.g.,
    // assemble() reporting a value that's stale relative to what's
    // actually on disk) — quick_check_all must reject it and fall back to
    // the full, byte-exact verify/repair pass rather than trusting the
    // claim, so real corruption on disk still gets fixed.
    let original: Vec<u8> = (0..500u32).map(|i| i as u8).collect();
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: original.clone(),
        }],
        128,
        4,
    );
    let path = dir.join("a.bin");
    let mut corrupted = std::fs::read(&path).unwrap();
    corrupted[10] ^= 0xFF;
    std::fs::write(&path, &corrupted).unwrap();

    let mut assembled = HashMap::new();
    assembled.insert(
        "a.bin".to_string(),
        AssembleOutcome::Complete { actual_crc32: 0 }, // deliberately wrong
    );

    let outcome = verify_and_repair(&dir, &assembled).await.unwrap();
    assert!(matches!(outcome, RepairOutcome::Repaired(_)));
    assert_eq!(std::fs::read(&path).unwrap(), original);

    std::fs::remove_dir_all(&dir).ok();
}
