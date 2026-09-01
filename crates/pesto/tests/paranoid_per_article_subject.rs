//! Legacy `--obfuscate=article` promises that every individual article gets its own
//! unique Subject, yEnc name and From, so segments cannot be deterministically grouped
//! alone — see `PostTask::subject_name`'s doc comment in `poster/mod.rs`
//! ("In paranoid mode each article gets a unique value"). A single
//! multi-segment file must therefore post each of its segments under a
//! different Subject, not one shared subject with only the `(part/total)`
//! suffix changing (which is what `--obfuscate=full` does, and paranoid is
//! supposed to go one step further than).
//!
//! Regression test: the connection-pool `worker()` built the outgoing
//! `Subject:` header from `task.meta.subject_name` (the file-level identity,
//! fixed once per file) instead of `task.subject_name` (the per-article
//! field `make_task` freshly randomises under paranoid). The bug only
//! surfaced with more than one connection — `-n 1` happens to avoid the
//! affected code path — so this test deliberately uses several connections
//! rather than the single-connection default other tests in this file use.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

fn spawn_capturing_server() -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let articles = Arc::new(Mutex::new(Vec::new()));
    let articles_clone = Arc::clone(&articles);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let articles = Arc::clone(&articles_clone);
            std::thread::spawn(move || handle_connection(stream, articles));
        }
    });

    (addr, articles)
}

fn handle_connection(stream: TcpStream, articles: Arc<Mutex<Vec<Vec<u8>>>>) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    if writer.write_all(b"200 pesto mock ready\r\n").is_err() {
        return;
    }

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end().to_string();

        if command == "POST" {
            if writer.write_all(b"340 send article\r\n").is_err() {
                return;
            }
            let mut article = Vec::new();
            loop {
                let mut raw = Vec::new();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if raw == b".\r\n" {
                    break;
                }
                article.extend_from_slice(&raw);
            }
            articles.lock().unwrap().push(article);
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if command.starts_with("STAT") {
            if writer.write_all(b"223 0 <id> article exists\r\n").is_err() {
                return;
            }
        } else if command.starts_with("MODE READER") {
            if writer.write_all(b"200 reader mode\r\n").is_err() {
                return;
            }
        } else if command == "QUIT" {
            let _ = writer.write_all(b"205 bye\r\n");
            return;
        } else if writer.write_all(b"500 unknown command\r\n").is_err() {
            return;
        }
    }
}

fn header_of(article: &[u8], header: &str) -> String {
    String::from_utf8_lossy(article)
        .lines()
        .find_map(|line| line.strip_prefix(header))
        .unwrap_or_else(|| panic!("article must have a {header} header"))
        .trim_end_matches('\r')
        .to_string()
}

fn yenc_name_of(article: &[u8]) -> String {
    String::from_utf8_lossy(article)
        .lines()
        .find(|line| line.starts_with("=ybegin "))
        .and_then(|line| line.rsplit_once(" name="))
        .map(|(_, name)| name.trim_end_matches('\r').to_string())
        .expect("article must have a yEnc name")
}

/// The quoted name token inside a `"<name>" yEnc (n/m)` subject — the part
/// that must be independently random per article under paranoid. Excludes
/// the `(part/total)` suffix, which mechanically differs per segment
/// regardless of obfuscation and would otherwise mask a repeated name (e.g.
/// `"X" yEnc (1/5)` vs `"X" yEnc (2/5)` are different strings even though
/// `X` — the part that actually matters — is the same).
fn quoted_name(subject: &str) -> &str {
    subject
        .split_once('"')
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name)
        .unwrap_or(subject)
}

#[test]
fn article_obfuscation_gives_every_segment_independent_wire_identity() {
    let (addr, articles) = spawn_capturing_server();
    let dir = tempfile::tempdir().unwrap();
    let xdg_home = tempfile::tempdir().unwrap();
    // Several times the article size so this single file posts multiple
    // segments — the case that actually exercises per-article freshness.
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 20_000]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &addr.port().to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "4"])
        .args(["--par2", "10"])
        .args(["--recovery-count", "1"])
        .args(["--article-size", "4000"])
        .arg("--no-hooks")
        .arg("--obfuscate=article")
        .args(["-o", out.to_str().unwrap()])
        .arg(&input)
        .output()
        .expect("failed to run pesto");
    assert!(
        output.status.success(),
        "pesto failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let articles = articles.lock().unwrap();
    assert!(
        articles.len() >= 5,
        "expected several segments for a single multi-segment file, got {}",
        articles.len()
    );

    let subjects: Vec<String> = articles.iter().map(|a| header_of(a, "Subject: ")).collect();
    let names: Vec<&str> = subjects.iter().map(|s| quoted_name(s)).collect();
    let mut unique = names.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(
        unique.len(),
        names.len(),
        "article mode must give every segment of the same file an \
         independent Subject (the quoted name token, not just the \
         mechanical (part/total) suffix); got duplicates among: {subjects:?}"
    );

    let yenc_names: Vec<String> = articles.iter().map(|a| yenc_name_of(a)).collect();
    let froms: Vec<String> = articles.iter().map(|a| header_of(a, "From: ")).collect();
    let from_unique: std::collections::HashSet<_> = froms.iter().collect();
    assert_eq!(from_unique.len(), froms.len(), "from repeated: {froms:?}");

    let yenc_unique: std::collections::HashSet<_> = yenc_names.iter().collect();
    assert_eq!(
        yenc_unique.len(),
        yenc_names.len(),
        "yEnc name repeated: {yenc_names:?}"
    );
    for (subject, yenc) in names.iter().zip(&yenc_names) {
        assert_ne!(*subject, yenc, "Subject and yEnc name must be independent");
    }
}

#[test]
fn full_keeps_yenc_stable_per_physical_file() {
    let (addr, articles) = spawn_capturing_server();
    let dir = tempfile::tempdir().unwrap();
    let xdg_home = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 20_000]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &addr.port().to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "4"])
        .args(["--par2", "10"])
        .args(["--recovery-count", "1"])
        .args(["--article-size", "4000"])
        .arg("--no-hooks")
        .arg("--obfuscate=full")
        .args(["-o", out.to_str().unwrap()])
        .arg(&input)
        .output()
        .expect("failed to run pesto");
    assert!(
        output.status.success(),
        "pesto failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let articles = articles.lock().unwrap();
    let subjects: Vec<String> = articles.iter().map(|a| header_of(a, "Subject: ")).collect();
    let subject_names: Vec<&str> = subjects.iter().map(|s| quoted_name(s)).collect();
    let froms: Vec<String> = articles.iter().map(|a| header_of(a, "From: ")).collect();
    let yenc_names: Vec<String> = articles.iter().map(|a| yenc_name_of(a)).collect();
    assert_eq!(
        subject_names
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        subject_names.len(),
        "full Subject repeated: {subjects:?}"
    );
    assert_eq!(
        froms.iter().collect::<std::collections::HashSet<_>>().len(),
        froms.len(),
        "full From repeated: {froms:?}"
    );

    let mut yenc_counts = std::collections::HashMap::<&str, usize>::new();
    for name in &yenc_names {
        *yenc_counts.entry(name).or_default() += 1;
    }
    assert_eq!(
        yenc_counts.len(),
        2,
        "expected one yEnc name for the data file and one for its PAR2 volume: {yenc_counts:?}"
    );
    assert_eq!(
        yenc_counts.values().copied().max(),
        Some(5),
        "all five data segments must retain one yEnc name: {yenc_counts:?}"
    );
    let par2_names: Vec<_> = yenc_counts
        .keys()
        .filter(|name| name.ends_with(".par2"))
        .collect();
    assert_eq!(
        par2_names.len(),
        1,
        "the opaque PAR2 yEnc name must retain its technical extension for client cleanup: {yenc_counts:?}"
    );
}
