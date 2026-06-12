//! End-to-end CLI test: verify `--each` processes top-level entries in the
//! deterministic order returned by the platform's native `PathBuf` sort. The
//! test computes the expected order using the same sort the binary uses, so it
//! is robust across macOS and Linux while still catching scheduler-dependent
//! reordering bugs.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn each_processes_entries_in_sorted_order() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Season01");
    std::fs::create_dir(&dir).unwrap();
    let names = ["Show.S01E10.mkv", "Show.S01E01.mkv", "Show.S01E02.mkv"];
    for name in names {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

    let mut sorted = names.map(PathBuf::from);
    sorted.sort();
    let expected: Vec<_> = sorted
        .iter()
        .map(|p| format!("── {} ──", p.file_name().unwrap().to_string_lossy()))
        .collect();

    let bin = env!("CARGO_BIN_EXE_pesto");
    let output = Command::new(bin)
        .arg("--dry-run")
        .arg("--groups")
        .arg("alt.binaries.test")
        .arg("--each")
        .arg(&dir)
        .output()
        .expect("failed to run pesto");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let labels: Vec<_> = stdout
        .lines()
        .filter(|l| l.starts_with("── ") && l.ends_with(" ──"))
        .map(|l| l.to_string())
        .collect();

    assert!(
        output.status.success(),
        "pesto exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        stdout,
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(labels, expected, "unexpected order in stdout:\n{}", stdout);
}
