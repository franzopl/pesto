//! Integration test: `penne::health::evaluate` against a real, on-disk PAR2
//! recovery set — not a hand-built `RecoverySet` struct. See
//! `tests/support/mod.rs` for how the fixture is built.

mod support;

use penne::health::evaluate;
use support::{build_fixture_set, FixtureFile};

#[test]
fn reports_repairable_when_damage_fits_available_recovery_blocks() {
    // slice_size=128, 500 bytes -> 4 slices; 4 recovery blocks cover exactly
    // that much reconstructable data.
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: vec![7u8; 500],
        }],
        128,
        4,
    );

    let health = evaluate(&dir, 4 * 128).unwrap().unwrap();
    assert_eq!(health.available_recovery_bytes, 4 * 128);
    assert!(health.looks_repairable());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn reports_unrepairable_when_damage_exceeds_available_recovery_blocks() {
    let dir = build_fixture_set(
        &[FixtureFile {
            name: "a.bin",
            data: vec![7u8; 500],
        }],
        128,
        1, // only one recovery block for 4 slices
    );

    let health = evaluate(&dir, 4 * 128).unwrap().unwrap();
    assert_eq!(health.available_recovery_bytes, 128);
    assert!(!health.looks_repairable());

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn no_par2_index_present_evaluates_to_none() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.bin"), b"just a file, no par2").unwrap();

    assert_eq!(evaluate(dir.path(), 1_000).unwrap(), None);
}
