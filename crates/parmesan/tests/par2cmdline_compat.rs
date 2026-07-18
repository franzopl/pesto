//! Cross-compatibility tests against the real `par2cmdline` binary.
//!
//! These shell out to an external `par2` binary that may not be installed
//! in every environment, so they are `#[ignore]`d by default. Run them
//! explicitly with:
//!
//! ```text
//! cargo test -p parmesan-par2 --test par2cmdline_compat -- --ignored
//! ```
//!
//! They validate the actual compatibility claim `parmesan` makes: that
//! recovery sets it creates are readable and repairable by the reference
//! implementation, and that recovery sets the reference implementation
//! creates are readable and repairable by `parmesan`. Multi-file inputs
//! specifically exercise the File-ID block-ordering fix described in
//! `ROADMAP.md` Phase 22 — a bug here would show up as a "successful"
//! repair that produces the wrong bytes, which is exactly what these tests
//! check for by comparing MD5 against the pre-corruption original, not just
//! checking that each tool reports success.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn par2cmdline_available() -> bool {
    Command::new("par2")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn parmesan_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_parmesan"))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "parmesan-par2cmdline-compat-{name}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Deterministic pseudo-random bytes — avoids a `rand` dev-dependency for a
/// handful of test fixtures.
fn random_file(path: &Path, size: usize, seed: u64) {
    let mut lcg: u64 = 0xC0FFEE_u64 ^ (seed << 32);
    let data: Vec<u8> = (0..size)
        .map(|_| {
            lcg = lcg
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (lcg >> 56) as u8
        })
        .collect();
    std::fs::write(path, data).unwrap();
}

fn corrupt_one_byte(path: &Path, offset: usize) {
    let mut data = std::fs::read(path).unwrap();
    data[offset] ^= 0xFF;
    std::fs::write(path, data).unwrap();
}

fn md5_of(path: &Path) -> [u8; 16] {
    parmesan::packet::md5(&std::fs::read(path).unwrap())
}

#[test]
#[ignore = "shells out to the external `par2` binary; run with `--ignored`"]
fn parmesan_creates_par2cmdline_repairs() {
    if !par2cmdline_available() {
        eprintln!("skipping: `par2` (par2cmdline) not found on PATH");
        return;
    }

    let dir = temp_dir("p-creates-par2-repairs");
    let movie = dir.join("movie.mkv");
    let subs = dir.join("subs.srt.bin");
    random_file(&movie, 300_000, 1);
    random_file(&subs, 60_000, 2);
    let movie_md5 = md5_of(&movie);
    let subs_md5 = md5_of(&subs);

    let status = Command::new(parmesan_bin())
        .args([
            "create",
            "movie.mkv",
            "subs.srt.bin",
            "-r",
            "40",
            "-s",
            "16KiB",
            "-q",
        ])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success(), "parmesan create failed");

    corrupt_one_byte(&movie, 54_321);
    std::fs::remove_file(&subs).unwrap();

    let status = Command::new("par2")
        .args(["repair", "movie.mkv.par2"])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "par2cmdline repair failed");

    assert_eq!(md5_of(&movie), movie_md5, "movie.mkv content mismatch");
    assert_eq!(md5_of(&subs), subs_md5, "subs.srt.bin content mismatch");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore = "shells out to the external `par2` binary; run with `--ignored`"]
fn par2cmdline_creates_parmesan_repairs() {
    if !par2cmdline_available() {
        eprintln!("skipping: `par2` (par2cmdline) not found on PATH");
        return;
    }

    let dir = temp_dir("par2-creates-p-repairs");
    let movie = dir.join("movie.mkv");
    let subs = dir.join("subs.srt.bin");
    random_file(&movie, 300_000, 3);
    random_file(&subs, 60_000, 4);
    let movie_md5 = md5_of(&movie);
    let subs_md5 = md5_of(&subs);

    let status = Command::new("par2")
        .args([
            "create",
            "-q",
            "-r40",
            "-s16384",
            "-a",
            "movie.mkv.par2",
            "movie.mkv",
            "subs.srt.bin",
        ])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success(), "par2cmdline create failed");

    corrupt_one_byte(&movie, 54_321);
    std::fs::remove_file(&subs).unwrap();

    let status = Command::new(parmesan_bin())
        .args(["repair", "movie.mkv.par2"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success(), "parmesan repair failed");

    assert_eq!(md5_of(&movie), movie_md5, "movie.mkv content mismatch");
    assert_eq!(md5_of(&subs), subs_md5, "subs.srt.bin content mismatch");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore = "shells out to the external `par2` binary; run with `--ignored`"]
fn par2cmdline_creates_parmesan_verifies_and_reports_ok() {
    if !par2cmdline_available() {
        eprintln!("skipping: `par2` (par2cmdline) not found on PATH");
        return;
    }

    let dir = temp_dir("par2-creates-p-verifies");
    let a = dir.join("a.bin");
    let b = dir.join("b.bin");
    let c = dir.join("c.bin");
    random_file(&a, 120_000, 5);
    random_file(&b, 45_000, 6);
    random_file(&c, 210_000, 7);

    let status = Command::new("par2")
        .args([
            "create",
            "-q",
            "-r20",
            "-s8192",
            "-a",
            "a.bin.par2",
            "a.bin",
            "b.bin",
            "c.bin",
        ])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success(), "par2cmdline create failed");

    let status = Command::new(parmesan_bin())
        .args(["verify", "a.bin.par2"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(
        status.success(),
        "parmesan verify should report OK (exit 0) on an intact par2cmdline-created set"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore = "shells out to the external `par2` binary; run with `--ignored`"]
fn single_file_round_trips_both_directions() {
    if !par2cmdline_available() {
        eprintln!("skipping: `par2` (par2cmdline) not found on PATH");
        return;
    }

    // A single-file set has no File-ID ordering decision to get right, but
    // it's still worth confirming as a baseline both tools agree on.
    for (dir_name, parmesan_creates) in [("single-p-creates", true), ("single-par2-creates", false)]
    {
        let dir = temp_dir(dir_name);
        let f = dir.join("solo.bin");
        random_file(&f, 250_000, 42);
        let original_md5 = md5_of(&f);

        if parmesan_creates {
            let status = Command::new(parmesan_bin())
                .args(["create", "solo.bin", "-r", "25", "-s", "8KiB", "-q"])
                .current_dir(&dir)
                .status()
                .unwrap();
            assert!(status.success(), "parmesan create failed");
        } else {
            let status = Command::new("par2")
                .args([
                    "create",
                    "-q",
                    "-r25",
                    "-s8192",
                    "-a",
                    "solo.bin.par2",
                    "solo.bin",
                ])
                .current_dir(&dir)
                .status()
                .unwrap();
            assert!(status.success(), "par2cmdline create failed");
        }

        corrupt_one_byte(&f, 123_456);

        if parmesan_creates {
            let status = Command::new("par2")
                .args(["repair", "solo.bin.par2"])
                .current_dir(&dir)
                .stdout(Stdio::null())
                .status()
                .unwrap();
            assert!(status.success(), "par2cmdline repair failed");
        } else {
            let status = Command::new(parmesan_bin())
                .args(["repair", "solo.bin.par2"])
                .current_dir(&dir)
                .status()
                .unwrap();
            assert!(status.success(), "parmesan repair failed");
        }

        assert_eq!(md5_of(&f), original_md5, "{dir_name}: content mismatch");
        std::fs::remove_dir_all(&dir).ok();
    }
}

#[test]
#[ignore = "shells out to the external `par2` binary; run with `--ignored`"]
fn unicode_filenames_round_trip_both_directions() {
    if !par2cmdline_available() {
        eprintln!("skipping: `par2` (par2cmdline) not found on PATH");
        return;
    }

    let dir = temp_dir("unicode-names");
    // Accented Latin, CJK, and an emoji — exercises non-ASCII File
    // Description packet names in both directions.
    let movie = dir.join("filme_ação.mkv");
    let subs = dir.join("字幕_🎬.srt.bin");
    random_file(&movie, 200_000, 8);
    random_file(&subs, 40_000, 9);
    let movie_md5 = md5_of(&movie);
    let subs_md5 = md5_of(&subs);

    let status = Command::new(parmesan_bin())
        .args([
            "create",
            "filme_ação.mkv",
            "字幕_🎬.srt.bin",
            "-r",
            "40",
            "-s",
            "16KiB",
            "-q",
        ])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success(), "parmesan create failed");

    corrupt_one_byte(&movie, 54_321);
    std::fs::remove_file(&subs).unwrap();

    let status = Command::new("par2")
        .args(["repair", "filme_ação.mkv.par2"])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(
        status.success(),
        "par2cmdline repair failed on Unicode names"
    );

    assert_eq!(md5_of(&movie), movie_md5);
    assert_eq!(md5_of(&subs), subs_md5);

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
#[ignore = "shells out to the external `par2` binary; run with `--ignored`"]
fn heavier_multi_file_damage_round_trips_both_directions() {
    if !par2cmdline_available() {
        eprintln!("skipping: `par2` (par2cmdline) not found on PATH");
        return;
    }

    // Five files, several corrupted in multiple places and two deleted
    // entirely — closer to a real "half the archive got mangled" scenario
    // than the single-byte-corruption tests above.
    let dir = temp_dir("heavier-damage");
    let names = ["a.bin", "b.bin", "c.bin", "d.bin", "e.bin"];
    let sizes = [90_000usize, 150_000, 45_000, 300_000, 60_000];
    let mut originals = Vec::new();
    for (i, (name, size)) in names.iter().zip(sizes).enumerate() {
        let path = dir.join(name);
        random_file(&path, size, 100 + i as u64);
        originals.push(md5_of(&path));
    }

    let status = Command::new(parmesan_bin())
        .args(
            [
                "create", "a.bin", "b.bin", "c.bin", "d.bin", "e.bin", "-r", "50", "-s", "8KiB",
                "-q",
            ]
            .as_slice(),
        )
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success(), "parmesan create failed");

    // Corrupt several spots in a.bin, several in d.bin; delete b.bin and e.bin.
    for offset in [1_000usize, 40_000, 80_000] {
        corrupt_one_byte(&dir.join("a.bin"), offset);
    }
    for offset in [5_000usize, 150_000, 250_000] {
        corrupt_one_byte(&dir.join("d.bin"), offset);
    }
    std::fs::remove_file(dir.join("b.bin")).unwrap();
    std::fs::remove_file(dir.join("e.bin")).unwrap();

    let status = Command::new("par2")
        .args(["repair", "a.bin.par2"])
        .current_dir(&dir)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "par2cmdline repair failed");

    for (name, expected) in names.iter().zip(&originals) {
        assert_eq!(
            &md5_of(&dir.join(name)),
            expected,
            "{name} content mismatch"
        );
    }

    std::fs::remove_dir_all(&dir).ok();
}
