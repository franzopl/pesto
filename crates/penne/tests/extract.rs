//! Integration tests: `penne::extract::extract_all` against real archives,
//! built with the actual `7z`/`rar` CLIs (the same tools `pesto::compress`
//! shells out to for creation) — not hand-crafted archive bytes.
//!
//! Skips gracefully when a required tool isn't installed, since these are
//! optional system dependencies (matching `pesto::compress`'s own stance:
//! `7z`/`rar` are expected in `PATH`, `rar` "not distributed... due to
//! licensing").

use std::path::Path;
use std::process::Command;

use penne::extract::{extract_all, ArchiveKind};

fn have(bin: &str) -> bool {
    Command::new(bin)
        .arg("--help")
        .output()
        .map(|o| o.status.success() || !o.stdout.is_empty() || !o.stderr.is_empty())
        .unwrap_or(false)
}

#[tokio::test]
async fn extracts_a_plain_7z_archive() {
    if !have("7z") {
        eprintln!("skipping: 7z not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let payload_dir = dir.path().join("payload");
    std::fs::create_dir_all(&payload_dir).unwrap();
    std::fs::write(payload_dir.join("hello.txt"), b"hello from 7z").unwrap();

    let archive = dir.path().join("release.7z");
    let status = Command::new("7z")
        .arg("a")
        .arg("-y")
        .arg(&archive)
        .arg(payload_dir.join("hello.txt"))
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::remove_dir_all(&payload_dir).unwrap();

    let extracted = extract_all(dir.path(), None).await.unwrap();
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0].kind, ArchiveKind::SevenZip);
    assert_eq!(extracted[0].base_name, "release");

    let out = std::fs::read(dir.path().join("hello.txt")).unwrap();
    assert_eq!(out, b"hello from 7z");
}

#[tokio::test]
async fn extracts_a_password_protected_7z_archive() {
    if !have("7z") {
        eprintln!("skipping: 7z not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("secret.txt");
    std::fs::write(&src, b"top secret payload").unwrap();

    let archive = dir.path().join("release.7z");
    let status = Command::new("7z")
        .arg("a")
        .arg("-y")
        .arg("-phunter2")
        .arg(&archive)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::remove_file(&src).unwrap();

    let extracted = extract_all(dir.path(), Some("hunter2")).await.unwrap();
    assert_eq!(extracted.len(), 1);

    let out = std::fs::read(dir.path().join("secret.txt")).unwrap();
    assert_eq!(out, b"top secret payload");
}

#[tokio::test]
async fn wrong_password_fails_instead_of_silently_producing_garbage() {
    if !have("7z") {
        eprintln!("skipping: 7z not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("secret.txt");
    std::fs::write(&src, b"top secret payload").unwrap();

    let archive = dir.path().join("release.7z");
    let status = Command::new("7z")
        .arg("a")
        .arg("-y")
        .arg("-phunter2")
        .arg(&archive)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::remove_file(&src).unwrap();

    let result = extract_all(dir.path(), Some("wrong-password")).await;
    assert!(result.is_err());
    assert!(!dir.path().join("secret.txt").exists());
}

#[tokio::test]
async fn extracts_a_multi_volume_rar_archive() {
    if !have("rar") {
        eprintln!("skipping: rar not installed");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("big.bin");
    // Big enough to force at least 3 volumes at a tiny volume size.
    std::fs::write(&src, vec![0xABu8; 20_000]).unwrap();

    let archive = dir.path().join("release.rar");
    let status = Command::new("rar")
        .arg("a")
        .arg("-m0")
        .arg("-ep1") // strip the absolute path down to the base name, like
        // `pesto::compress::compress_with_rar` does for real releases
        .arg("-v5k") // 5 KiB volumes
        .arg(&archive)
        .arg(&src)
        .status()
        .unwrap();
    assert!(status.success());
    std::fs::remove_file(&src).unwrap();

    // Confirm the test actually created more than one volume, otherwise
    // this isn't testing what it claims to. Modern `rar` names volumes
    // `release.rar`, `release.part2.rar`, `release.part3.rar`, … (old-style
    // `.r00`/`.r01` is also possible depending on `rar` version/settings) —
    // match loosely on both.
    let volume_count = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy().into_owned();
            name.starts_with("release") && (name.ends_with(".rar") || name.contains(".r"))
        })
        .count();
    assert!(
        volume_count > 1,
        "expected multiple RAR volumes in fixture, found {volume_count}"
    );

    if !have("unrar") {
        eprintln!("skipping extraction half: unrar not installed");
        return;
    }

    let extracted = extract_all(dir.path(), None).await.unwrap();
    assert_eq!(extracted.len(), 1);
    assert_eq!(extracted[0].kind, ArchiveKind::Rar);
    // The exact entry-point name depends on the installed `rar` version: it
    // may keep the first volume as a bare `release.rar`, or (as newer
    // versions do, once more than one volume turns out to be needed)
    // retroactively rename it to `release.part1.rar` alongside
    // `release.part2.rar`, etc. Either way it must be the *first* volume.
    let entry_name = extracted[0]
        .entry_path
        .file_name()
        .unwrap()
        .to_string_lossy();
    assert!(
        entry_name == "release.rar" || entry_name == "release.part1.rar",
        "unexpected entry point: {entry_name}"
    );

    let out = std::fs::read(dir.path().join("big.bin")).unwrap();
    assert_eq!(out, vec![0xABu8; 20_000]);
}

#[tokio::test]
async fn no_archives_present_extracts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("readme.txt"), b"nothing to see here").unwrap();

    let extracted = extract_all(dir.path(), None).await.unwrap();
    assert!(extracted.is_empty());
}

#[test]
fn have_helper_detects_a_definitely_missing_binary() {
    assert!(!have("this-binary-does-not-exist-anywhere-hopefully"));
    let _ = Path::new(".");
}
