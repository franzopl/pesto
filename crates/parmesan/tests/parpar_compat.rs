//! Byte-level compatibility checks against the real ParPar binary.
//!
//! These shell out to an external `parpar` binary that may not be installed
//! in every environment, so they are `#[ignore]`d by default. Run them
//! explicitly with:
//!
//! ```text
//! cargo test -p parmesan-par2 --test parpar_compat -- --ignored
//! ```

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use parmesan::packet::{TYPE_FILE_DESC, TYPE_UNICODE_FILENAME};
use parmesan::packet_reader::read_packets;

fn parpar_available() -> bool {
    Command::new("parpar")
        .arg("--version")
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
        "parmesan-parpar-compat-{name}-{}-{:?}",
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

/// The sorted bodies of every packet of `packet_type` in `path`.
///
/// Headers are left out: they carry the recovery set ID, the MD5 of the Main
/// packet, and parmesan lists File IDs in Main in a different order than
/// ParPar does.
fn bodies_of_type(path: &Path, packet_type: [u8; 16]) -> Vec<Vec<u8>> {
    let mut bodies: Vec<_> = read_packets(&std::fs::read(path).unwrap())
        .into_iter()
        .filter(|p| p.packet_type == packet_type)
        .map(|p| p.body)
        .collect();
    bodies.sort();
    bodies
}

#[test]
#[ignore = "shells out to the external `parpar` binary; run with `--ignored`"]
fn unicode_filename_packets_match_parpar() {
    if !parpar_available() {
        eprintln!("skipping: `parpar` not found on PATH");
        return;
    }

    let dir = temp_dir("unicode-names");
    // Accented Latin, CJK, an emoji (surrogate pair in UTF-16) and an ASCII
    // name that must not get a Unicode packet.
    let names = [
        "filme_ação.mkv",
        "字幕_🎬.srt.bin",
        "Cafe\u{301}.txt",
        "plain.bin",
    ];
    for (seed, name) in names.iter().enumerate() {
        random_file(&dir.join(name), 50_000 + seed * 1000, seed as u64);
    }

    let status = Command::new(parmesan_bin())
        .arg("create")
        .args(names)
        .args(["-r", "10", "-s", "16KiB", "-q"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success(), "parmesan create failed");

    let status = Command::new("parpar")
        .args(["-s", "16K", "-r", "10%", "-o", "parpar.par2", "-q"])
        .args(names)
        .current_dir(&dir)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "parpar failed");

    // parmesan names the index after the first input.
    let ours = dir.join("filme_ação.mkv.par2");
    let theirs = dir.join("parpar.par2");
    assert_eq!(
        bodies_of_type(&ours, TYPE_FILE_DESC),
        bodies_of_type(&theirs, TYPE_FILE_DESC),
        "File Description packets differ from ParPar's"
    );
    let unicode = bodies_of_type(&ours, TYPE_UNICODE_FILENAME);
    assert_eq!(unicode.len(), 3, "one Unicode packet per non-ASCII name");
    assert_eq!(
        unicode,
        bodies_of_type(&theirs, TYPE_UNICODE_FILENAME),
        "Unicode Filename packets differ from ParPar's"
    );

    std::fs::remove_dir_all(&dir).ok();
}
