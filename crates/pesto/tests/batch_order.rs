//! End-to-end CLI test: verify `--each` processes top-level entries in natural
//! lexical order. This makes episode numbering human-friendly (`E01`, `E02`,
//! `E10` instead of `E01`, `E10`, `E02`).

use std::process::Command;

#[test]
fn each_processes_entries_in_natural_order() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("Season01");
    std::fs::create_dir(&dir).unwrap();
    for name in ["Show.S01E10.mkv", "Show.S01E01.mkv", "Show.S01E02.mkv"] {
        std::fs::write(dir.join(name), b"x").unwrap();
    }

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

    assert_eq!(
        labels,
        vec![
            "── Show.S01E01.mkv ──",
            "── Show.S01E02.mkv ──",
            "── Show.S01E10.mkv ──",
        ],
        "unexpected order in stdout:\n{}",
        stdout
    );
}
