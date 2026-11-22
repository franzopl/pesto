//! PESTO-R6-2 proof, PT-03 option (b): two uploads produced through the
//! pesto library (shared transport password, distinct per-session salts),
//! the actual `--merge-season` CLI output (driven via the prebuilt `pesto`
//! binary), and a real Penne download reconstructing both plaintexts.
//!
//! History (PE-NEXT-4): this test previously spawned a nested
//! `cargo build` from inside penne's test crate to obtain a fresh `pesto`
//! binary, which panicked without `CARGO` and risked build-lock stalls.
//! The producer side is now expressed through the pesto-side adapter
//! mirror (`season_merge_encryption.rs` already mirrors penne's decryptor
//! gate for the same layering reason): articles are encoded through
//! `UploadEncryptionAdapter` and registered on a fake POST server, while
//! the merge step still drives the real prebuilt `pesto --merge-season`
//! CLI so the combined NZB remains an actual CLI output. The prebuilt
//! binary is located locally in this test file by `find_pesto_bin`
//! without spawning cargo; the proof hard-fails when the binary is
//! absent, and hard-fails when the binary is stale versus the newest
//! mtime under `crates/pesto/src/**` (where the merge CLI lives), so
//! build the workspace first (`cargo build --bin pesto`).
//! The upload bytes themselves do not depend on which binary runs.
//!
//! Unlike `download_encryption.rs::merged_encrypted_release_with_multiple_salts_decrypts`
//! (which hand-builds the merged XML), every `.nzb` involved is produced
//! from real segments and the combined season NZB is produced by the real
//! `run_merge_season`. The download side uses the real
//! `penne::nzb::load` → `download_decryptor` constructor and the same
//! fake-server pipeline as the other integration tests.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::Arc;

use penne::config::ServerTier;
use penne::download::download_queue_with_decryptor;
use penne::nzb::{download_decryptor, load};
use penne::queue::build;
use pesto::config::ServerEntry;
use pesto::crypto::{EncryptionSession, UploadEncryptionAdapter};
use pesto::poster::PostedSegment;
use pesto::yenc::PartSpec;

fn article_body(article: &[u8]) -> &[u8] {
    let marker = b"\r\n\r\n";
    let start = article
        .windows(marker.len())
        .position(|window| window == marker)
        .expect("article headers must end with CRLF CRLF")
        + marker.len();
    &article[start..]
}

fn article_message_id(article: &[u8]) -> String {
    let text = String::from_utf8_lossy(article);
    for line in text.split("\r\n") {
        if let Some(value) = line
            .strip_prefix("Message-ID:")
            .or_else(|| line.strip_prefix("Message-Id:"))
        {
            return value.trim().to_string();
        }
    }
    panic!("captured article has no Message-ID header");
}

/// Fake BODY/QUIT server for the download pass: maps bare Message-IDs to
/// article bodies, exactly like `download_encryption.rs`'s helper.
fn spawn_body_server(known: HashMap<String, Vec<u8>>) -> SocketAddr {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let known = known.clone();
            std::thread::spawn(move || {
                let Ok(mut writer) = stream.try_clone() else {
                    return;
                };
                let mut reader = BufReader::new(stream);
                if writer.write_all(b"200 mock ready\r\n").is_err() {
                    return;
                }
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line) {
                        Ok(0) | Err(_) => return,
                        Ok(_) => {}
                    }
                    let cmd = line.trim_end().to_string();
                    if let Some(rest) = cmd.strip_prefix("BODY ") {
                        let id = rest.trim_start_matches('<').trim_end_matches('>');
                        match known.get(id) {
                            Some(body) => {
                                let header = format!("222 0 <{id}> body\r\n");
                                if writer.write_all(header.as_bytes()).is_err()
                                    || write_dot_stuffed(&mut writer, body).is_err()
                                    || writer.write_all(b".\r\n").is_err()
                                {
                                    return;
                                }
                            }
                            None => {
                                if writer.write_all(b"430 No such article\r\n").is_err() {
                                    return;
                                }
                            }
                        }
                    } else if cmd == "QUIT" {
                        let _ = writer.write_all(b"205 bye\r\n");
                        return;
                    } else if writer.write_all(b"500 unknown command\r\n").is_err() {
                        return;
                    }
                }
            });
        }
    });
    addr
}

fn write_dot_stuffed(w: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
    for line in body.split_inclusive(|&b| b == b'\n') {
        if line.starts_with(b".") {
            w.write_all(b".")?;
        }
        w.write_all(line)?;
    }
    Ok(())
}

fn server_entry(addr: SocketAddr) -> ServerEntry {
    ServerEntry {
        host: addr.ip().to_string(),
        port: addr.port(),
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        retry_delay: 0,
        timeout: 5,
        proxy: None,
    }
}

/// PT-03 (test-local): locate a prebuilt `pesto` binary without spawning
/// cargo. Search order: `CARGO_BIN_EXE_pesto`, then workspace
/// `target/{debug,release}`.
fn find_pesto_bin() -> Option<PathBuf> {
    let exe_name = if cfg!(windows) { "pesto.exe" } else { "pesto" };
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Some(p) = std::env::var_os("CARGO_BIN_EXE_pesto") {
        candidates.push(PathBuf::from(p));
    }
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    if let Some(workspace) = manifest_dir.parent().and_then(Path::parent) {
        for profile in ["debug", "release"] {
            candidates.push(workspace.join("target").join(profile).join(exe_name));
        }
    }
    candidates.into_iter().find(|p| p.exists())
}

/// R11-PT-03: the merge CLI lives under `crates/pesto/src/**`
/// (`src/bin/pesto/merge.rs`, `command.rs`, ...), not in `Cargo.toml`,
/// so a `Cargo.toml`-only comparison cannot see merge-CLI edits.
/// Returns the newest mtime under `crates/pesto/src/**`, falling back
/// to the crate manifest when the source tree cannot be walked.
/// Panics when neither yields an mtime, so the proof hard-fails rather
/// than silently trusting a possibly-stale binary.
fn newest_pesto_source_mtime() -> std::time::SystemTime {
    let pesto_crate = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("penne lives under crates/")
        .join("pesto");
    let mut newest: Option<std::time::SystemTime> = None;
    let mut stack: Vec<PathBuf> = vec![pesto_crate.join("src")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                stack.push(path);
            } else if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                if newest.is_none_or(|current| modified > current) {
                    newest = Some(modified);
                }
            }
        }
    }
    if let Some(newest) = newest {
        return newest;
    }
    std::fs::metadata(pesto_crate.join("Cargo.toml"))
        .and_then(|m| m.modified())
        .expect("pesto source tree and manifest must be readable for the stale check")
}

/// PT-03 stale-binary check (test-local; R11-PD-01/R11-PT-01): `binary`
/// is stale iff it is older than `newest_source` (the newest mtime
/// under `crates/pesto/src/**`; see [`newest_pesto_source_mtime`]).
/// A missing/unreadable binary mtime counts as stale (`true`) so
/// callers hard-fail rather than exercising a possibly-outdated
/// binary. Pure predicate — no cargo process is spawned here, keeping
/// the no-nested-cargo guarantee.
fn binary_is_stale(binary: &Path, newest_source: std::time::SystemTime) -> bool {
    let Some(bmtime) = std::fs::metadata(binary).and_then(|m| m.modified()).ok() else {
        return true;
    };
    bmtime < newest_source
}

fn run_pesto(bin: &Path, xdg_home: &Path, args: &[&std::ffi::OsStr]) -> Output {
    Command::new(bin)
        .env("XDG_CONFIG_HOME", xdg_home)
        .args(args)
        .output()
        .expect("failed to run pesto")
}

/// The full producer→aggregate→consumer proof (Body Std v1.2 §8):
///
/// 1. Two library uploads with one shared `--encrypt` password (each run
///    draws its own fresh salt — asserted distinct below).
/// 2. The actual per-episode NZBs merged by the actual `--merge-season`.
/// 3. The actual combined XML parsed via `penne::nzb::load` and the real
///    `download_decryptor` constructor.
/// 4. The existing fake-server download proof reconstructs both plaintexts.
#[tokio::test]
async fn merged_cli_season_output_downloads_and_decrypts_both_episodes() {
    // PT-03: hard fail when the prebuilt binary is absent -- no skip and
    // no rebuild attempt anywhere.
    let bin = find_pesto_bin()
        .expect("prebuilt pesto binary not found; build workspace first (cargo build --bin pesto)");
    assert!(
        !binary_is_stale(&bin, newest_pesto_source_mtime()),
        "stale pesto binary {}; rebuild workspace first (cargo build --bin pesto)",
        bin.display()
    );
    let password = "merged-season-proof-pass";
    let payload1: Vec<u8> = (0..200u32).map(|i| (i % 251) as u8).collect();
    let payload2: Vec<u8> = (0..200u32).map(|i| (i % 253) as u8).collect();

    let root = tempfile::tempdir().unwrap();
    let nzb_dir = root.path().join("nzbs");
    std::fs::create_dir_all(&nzb_dir).unwrap();

    let xdg_home = root.path().join("xdg");
    std::fs::create_dir_all(&xdg_home).unwrap();

    // Two library uploads, each with its own fresh session salt under one
    // shared transport password (pesto-side adapter mirror; see header).
    let mut posted: Vec<Vec<u8>> = Vec::new();
    let episodes: [(&str, &[u8], &str); 2] = [
        ("Show.S01E01.mkv", &payload1, "ep1-msg-01@example.com"),
        ("Show.S01E02.mkv", &payload2, "ep2-msg-01@example.com"),
    ];
    for (idx, (file_name, payload, msg_id)) in episodes.iter().enumerate() {
        let salt = pesto::crypto::generate_alphabet_salt();
        let session = Arc::new(EncryptionSession::new(password, salt).unwrap());
        let uploader = UploadEncryptionAdapter::new(session);
        let mut body = Vec::new();
        let encoded = uploader
            .encode_article(
                file_name,
                payload.len() as u64,
                PartSpec {
                    number: 1,
                    total: 1,
                    offset: 0,
                },
                payload,
                128,
                (idx + 1) as u32,
                &mut body,
            )
            .unwrap();
        // Wrap in minimal NNTP article headers so the existing
        // Message-ID/body extraction helpers apply unchanged.
        let mut article = format!(
            "From: poster@example.com\r\nSubject: \"{file_name}\" yEnc\r\nMessage-ID: <{msg_id}>\r\n\r\n"
        )
        .into_bytes();
        article.extend_from_slice(&encoded.body);
        posted.push(article);

        // Per-episode NZB written through the real NZB generator.
        let seg = PostedSegment {
            file_name: file_name.to_string(),
            file_path: std::path::Path::new(file_name).into(),
            subject_name: (*file_name).into(),
            wire_name: (*file_name).into(),
            wire_yenc_name: (*file_name).into(),
            file_size: payload.len() as u64,
            part: 1,
            total: 1,
            message_id: format!("<{msg_id}>"),
            bytes: payload.len() as u64, // NZB bytes = plaintext segment length (Body Std v1.2 §2.1)
            from: "poster@example.com".into(),
            date: (None, Some(1774300000)),
            full_crc32: 0,
            server_idx: 0,
            file_index: (idx + 1) as u32,
            total_files: 1,
            segment_index: (idx + 1) as u32,
            plaintext_digest: None,
        };
        let meta = pesto::nzb::NzbMeta {
            password: Some(password.to_string()),
            yenc_encrypted: true,
            ..Default::default()
        };
        let xml = pesto::nzb::generate(
            &["alt.binaries.test".to_string()],
            &[seg],
            &meta,
            pesto::config::ObfuscateMode::None,
        );
        let ep_nzb = file_name.replace(".mkv", ".nzb");
        std::fs::write(nzb_dir.join(&ep_nzb), xml).unwrap();
    }

    assert!(
        posted.len() >= 2,
        "expected articles from both episodes; got {}",
        posted.len()
    );

    // Distinct salts: each upload invocation must draw its own session salt
    // even though both share the transport password.
    let mut salts = Vec::new();
    for article in &posted {
        let (salt, _) = pesto::crypto::extract_bootstrap_from_line1(article_body(article)).unwrap();
        salts.push(salt);
    }
    salts.sort();
    salts.dedup();
    assert!(
        salts.len() >= 2,
        "the two uploads must use distinct salts under one shared password"
    );

    // Per-episode NZBs generated via `pesto::nzb::generate` must be marked and
    // carry the transport password.
    for episode in ["Show.S01E01.nzb", "Show.S01E02.nzb"] {
        let content = std::fs::read_to_string(nzb_dir.join(episode)).unwrap();
        let parsed = pesto::nzb::parse(&content).unwrap();
        assert!(parsed.meta.yenc_encrypted, "{episode} must be marked");
        assert_eq!(
            parsed.meta.password.as_deref(),
            Some(password),
            "{episode} must carry the transport password"
        );
    }

    // The actual offline merge over the actual per-episode NZBs. Season
    // grouping derives the key from the episode stems (`Show.S01E01` →
    // `Show.S01`), so the combined NZB is `Show.S01.nzb`.
    let merge_output = run_pesto(
        &bin,
        &xdg_home,
        &[std::ffi::OsStr::new("--merge-season"), nzb_dir.as_os_str()],
    );
    assert!(
        merge_output.status.success(),
        "merge-season failed:\n{}",
        String::from_utf8_lossy(&merge_output.stderr)
    );
    let merged_path = nzb_dir.join("Show.S01.nzb");
    assert!(merged_path.exists(), "merged season NZB must be written");

    // Real download metadata + decryptor constructor over the actual
    // combined XML.
    let parsed = load(&merged_path).expect("merged NZB must parse");
    assert!(parsed.meta.yenc_encrypted, "merged NZB must be marked");
    assert_eq!(
        parsed.meta.password.as_deref(),
        Some(password),
        "merged NZB must retain the shared transport password"
    );
    let decryptor = download_decryptor(&parsed.meta)
        .expect("decryptor construction must not fail")
        .expect("decryption must activate on the merged metadata");

    // Existing fake-server proof: map Message-ID → captured article body and
    // reconstruct both plaintexts through the real Penne pipeline.
    let mut known = HashMap::new();
    for article in &posted {
        known.insert(
            article_message_id(article)
                .trim_start_matches('<')
                .trim_end_matches('>')
                .to_string(),
            article_body(article).to_vec(),
        );
    }
    let body_addr = spawn_body_server(known);

    let queue = build(&parsed);
    let dest = root.path().join("downloads");
    let outcome = download_queue_with_decryptor(
        &queue,
        &[ServerTier::solo(server_entry(body_addr))],
        &dest,
        0,
        None,
        Some(decryptor),
    )
    .await
    .expect("download must succeed");

    assert!(outcome.missing.is_empty(), "missing: {:?}", outcome.missing);
    assert!(outcome.corrupt.is_empty(), "corrupt: {:?}", outcome.corrupt);
    assert_eq!(
        std::fs::read(dest.join("Show.S01E01.mkv")).unwrap(),
        payload1,
        "episode 1 plaintext must reconstruct exactly"
    );
    assert_eq!(
        std::fs::read(dest.join("Show.S01E02.mkv")).unwrap(),
        payload2,
        "episode 2 plaintext must reconstruct exactly"
    );
}

/// The sole stale-predicate test (R11-PT-02), next to its only
/// consumer. `binary_is_stale` is a pure predicate over a binary path
/// and a source mtime: it spawns no cargo and reads no `CARGO_*` env.
#[test]
fn stale_binary_detection_flags_older_binary_without_rebuild() {
    use std::time::{Duration, UNIX_EPOCH};
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("pesto");
    std::fs::write(&binary, b"binary").unwrap();
    let base = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
    std::fs::File::open(&binary)
        .unwrap()
        .set_modified(base)
        .unwrap();
    // Binary newer than the newest source mtime => not stale.
    assert!(!binary_is_stale(&binary, base - Duration::from_secs(3600)));
    // Binary older than the newest source mtime => stale.
    assert!(binary_is_stale(&binary, base + Duration::from_secs(3600)));
    // Missing binary => stale.
    assert!(binary_is_stale(
        root.path().join("missing-bin").as_path(),
        base
    ));
}
