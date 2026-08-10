//! Under `--obfuscate=full` and `--obfuscate=full-shared`, the wire
//! `Subject:` header and the yEnc body's `=ybegin ... name=` value must
//! never match exactly. Reusing the same string leaves an exact-match
//! signature (header == body name) across every posted article that
//! fingerprints this specific posting tool, undermining part of what
//! obfuscation is for — see `poster/mod.rs`'s
//! `ObfuscateMode::Full | ObfuscateMode::Paranoid` and `FullShared` arms.
//!
//! `full-shared` is a partial exception: since issue #106, its yEnc name
//! deliberately keeps the release's shared prefix (with its own random
//! suffix) so an indexer reading only the yEnc body can still recognise the
//! article as part of the release — it just avoids repeating the *whole*
//! subject string verbatim.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// A mock NNTP server that accepts every `POST` and records the raw article
/// bytes it received, in order.
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

/// Pull the `Subject:` header value and the yEnc `=ybegin ... name=` value
/// out of one raw posted article.
fn subject_and_yenc_name(article: &[u8]) -> (String, String) {
    let text = String::from_utf8_lossy(article);
    let subject = text
        .lines()
        .find_map(|l| l.strip_prefix("Subject: "))
        .expect("article must have a Subject header")
        .trim_end_matches('\r')
        .to_string();
    let ybegin = text
        .lines()
        .find(|l| l.starts_with("=ybegin "))
        .expect("article must have a =ybegin line");
    let name = ybegin
        .split("name=")
        .nth(1)
        .expect("=ybegin must carry name=")
        .trim_end_matches('\r')
        .to_string();
    (subject, name)
}

/// The quoted real/obfuscated name inside a `"<name>" yEnc (n/m)` subject.
fn quoted_name(subject: &str) -> &str {
    subject
        .split_once('"')
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(name, _)| name)
        .unwrap_or(subject)
}

fn run_pesto(
    addr: SocketAddr,
    xdg_home: &std::path::Path,
    obfuscate_flag: &str,
    inputs: &[&std::path::Path],
    out: &std::path::Path,
) {
    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home)
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &addr.port().to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "1"])
        .args(["--par2", "0"])
        .arg("--no-hooks")
        .arg(obfuscate_flag)
        .args(["-o", out.to_str().unwrap()])
        .args(inputs)
        .output()
        .expect("failed to run pesto");
    assert!(
        output.status.success(),
        "pesto failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn full_obfuscation_uses_independent_subject_and_yenc_name() {
    let (addr, articles) = spawn_capturing_server();
    let dir = tempfile::tempdir().unwrap();
    let xdg_home = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    run_pesto(addr, xdg_home.path(), "--obfuscate=full", &[&input], &out);

    let articles = articles.lock().unwrap();
    assert_eq!(articles.len(), 1);
    let (subject, yenc_name) = subject_and_yenc_name(&articles[0]);
    let subject_name = quoted_name(&subject);
    assert_ne!(
        subject_name, yenc_name,
        "subject and yEnc name= must be independently random under --obfuscate=full"
    );
}

#[test]
fn full_shared_obfuscation_keeps_prefix_on_subject_and_yenc_name_but_avoids_exact_match() {
    let (addr, articles) = spawn_capturing_server();
    let dir = tempfile::tempdir().unwrap();
    let xdg_home = tempfile::tempdir().unwrap();
    let input_a = dir.path().join("a.bin");
    let input_b = dir.path().join("b.bin");
    std::fs::write(&input_a, vec![0xABu8; 64]).unwrap();
    std::fs::write(&input_b, vec![0xCDu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    run_pesto(
        addr,
        xdg_home.path(),
        "--obfuscate=full-shared",
        &[&input_a, &input_b],
        &out,
    );

    let articles = articles.lock().unwrap();
    assert_eq!(articles.len(), 2);

    let (subject_a, yenc_a) = subject_and_yenc_name(&articles[0]);
    let (subject_b, yenc_b) = subject_and_yenc_name(&articles[1]);
    let name_a = quoted_name(&subject_a);
    let name_b = quoted_name(&subject_b);

    // Both files' wire subjects share the same random prefix — that's the
    // whole point of `full-shared` (issue #58): indexers group the release
    // by matching that shared prefix across subjects.
    let prefix_a = name_a.split('.').next().unwrap().split('-').next().unwrap();
    let prefix_b = name_b.split('.').next().unwrap().split('-').next().unwrap();
    assert_eq!(
        prefix_a, prefix_b,
        "full-shared must keep one shared prefix across every file's subject"
    );

    // The yEnc body name= carries that same prefix too (issue #106) — an
    // indexer that only inspects the yEnc body must still be able to
    // recognise the article as part of the release.
    assert!(
        yenc_a.starts_with(&format!("{prefix_a}-")),
        "yEnc name= `{yenc_a}` must start with the shared prefix `{prefix_a}-`"
    );
    assert!(
        yenc_b.starts_with(&format!("{prefix_a}-")),
        "yEnc name= `{yenc_b}` must start with the shared prefix `{prefix_a}-`"
    );

    // But the per-file random suffix still keeps the two yEnc names — and
    // each yEnc name from its own file's subject — from matching exactly.
    assert_ne!(
        yenc_a, yenc_b,
        "yEnc name= suffix must be independently random per file under full-shared"
    );
    assert_ne!(
        name_a, yenc_a,
        "file a's subject and yEnc name= must not match exactly under full-shared"
    );
    assert_ne!(
        name_b, yenc_b,
        "file b's subject and yEnc name= must not match exactly under full-shared"
    );
}
