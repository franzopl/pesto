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

use parmesan::packet::{
    TYPE_FILE_DESC, TYPE_IFSC, TYPE_MAIN, TYPE_RECOVERY, TYPE_UNICODE_FILENAME,
};
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

/// Every packet of `packet_type` across the `.par2` files in `dir` whose
/// name starts with `prefix`, as sorted, de-duplicated `(set ID, body)` pairs.
/// The volumes repeat the critical packets, and ParPar and parmesan split
/// recovery slices across volumes differently, so the whole set is compared.
fn packets_of_type(dir: &Path, prefix: &str, packet_type: [u8; 16]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut packets = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if !name.starts_with(prefix) || !name.ends_with(".par2") {
            continue;
        }
        for packet in read_packets(&std::fs::read(&path).unwrap()) {
            if packet.packet_type == packet_type {
                packets.push((packet.recovery_set_id.to_vec(), packet.body));
            }
        }
    }
    packets.sort();
    packets.dedup();
    packets
}

#[test]
#[ignore = "shells out to the external `parpar` binary; run with `--ignored`"]
fn recovery_set_matches_parpar_byte_for_byte() {
    if !parpar_available() {
        eprintln!("skipping: `parpar` not found on PATH");
        return;
    }

    let dir = temp_dir("parity");
    // Several files so the File ID order matters, with accented Latin, CJK,
    // an emoji (surrogate pair in UTF-16), NFD, and an ASCII name that must
    // not get a Unicode packet.
    let names = [
        "filme_ação.mkv",
        "字幕_🎬.srt.bin",
        "Cafe\u{301}.txt",
        "plain.bin",
        "extra-1.bin",
        "extra-2.bin",
    ];
    for (seed, name) in names.iter().enumerate() {
        random_file(&dir.join(name), 50_000 + seed * 1000, seed as u64);
    }

    let status = Command::new(parmesan_bin())
        .arg("create")
        .args(names)
        .args(["--recovery-count", "6", "-s", "16KiB", "-q"])
        .current_dir(&dir)
        .status()
        .unwrap();
    assert!(status.success(), "parmesan create failed");

    let status = Command::new("parpar")
        .args(["-s", "16K", "-r", "6", "-o", "parpar.par2", "-q"])
        .args(names)
        .current_dir(&dir)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success(), "parpar failed");

    // parmesan names the set after the first input.
    let ours = "filme_ação.mkv";
    let theirs = "parpar";
    for (label, packet_type) in [
        ("Main", TYPE_MAIN),
        ("File Description", TYPE_FILE_DESC),
        ("Unicode Filename", TYPE_UNICODE_FILENAME),
        ("IFSC", TYPE_IFSC),
        ("Recovery Slice", TYPE_RECOVERY),
    ] {
        let mine = packets_of_type(&dir, ours, packet_type);
        assert!(!mine.is_empty(), "parmesan wrote no {label} packets");
        assert_eq!(
            mine,
            packets_of_type(&dir, theirs, packet_type),
            "{label} packets differ from ParPar's"
        );
    }
    assert_eq!(
        packets_of_type(&dir, ours, TYPE_UNICODE_FILENAME).len(),
        3,
        "one Unicode packet per non-ASCII name"
    );
    assert_eq!(packets_of_type(&dir, ours, TYPE_RECOVERY).len(), 6);

    std::fs::remove_dir_all(&dir).ok();
}
