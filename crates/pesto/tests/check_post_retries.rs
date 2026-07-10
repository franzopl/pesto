//! End-to-end CLI test for `--check-post-retries`: a mock NNTP server that
//! only starts answering `STAT` with "found" once a given article has been
//! *posted* a set number of times, so a single repost round is provably not
//! enough and a second round is provably required. This exercises the actual
//! compiled `pesto` binary against a real TCP connection, the same way
//! `batch_order.rs` does.
//!
//! Also guards against the regression this feature shipped alongside: before
//! the fix, `repost_missing_segments` reconstructed the source file path from
//! the *published* name (`PathBuf::from(&seg.file_name)`) instead of the
//! absolute path, so a repost could silently fail to find the file. Every
//! repost here only succeeds if pesto can actually re-read `movie.bin` from
//! disk, so a regression on that front would show up as the "enough retries"
//! case unexpectedly failing too.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Starts a background mock NNTP server. `STAT` reports an article as found
/// only once it has been `POST`ed at least `threshold` times, simulating a
/// server that keeps "losing" the article until it's been sent enough times.
fn spawn_mock_server(threshold: u32) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let posts: Arc<Mutex<HashMap<String, u32>>> = Arc::new(Mutex::new(HashMap::new()));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let posts = Arc::clone(&posts);
            std::thread::spawn(move || handle_connection(stream, posts, threshold));
        }
    });

    addr
}

fn handle_connection(stream: TcpStream, posts: Arc<Mutex<HashMap<String, u32>>>, threshold: u32) {
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
            let mut raw = Vec::new();
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if raw == b".\r\n" {
                    break;
                }
                article.extend_from_slice(&raw);
            }
            let text = String::from_utf8_lossy(&article);
            let id = text
                .lines()
                .find_map(|l| l.strip_prefix("Message-ID: "))
                .map(str::trim)
                .map(str::to_string);
            if let Some(id) = id {
                *posts.lock().unwrap().entry(id).or_insert(0) += 1;
            }
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if let Some(id) = command.strip_prefix("STAT ") {
            let count = *posts.lock().unwrap().get(id).unwrap_or(&0);
            let resp = if count >= threshold {
                format!("223 0 {id} article exists\r\n")
            } else {
                "430 No such article\r\n".to_string()
            };
            if writer.write_all(resp.as_bytes()).is_err() {
                return;
            }
        } else if command.starts_with("MODE READER") {
            if writer.write_all(b"200 reader mode\r\n").is_err() {
                return;
            }
        } else if command == "QUIT" {
            let _ = writer.write_all(b"205 bye\r\n");
            return;
        } else {
            if writer.write_all(b"500 unknown command\r\n").is_err() {
                return;
            }
        }
    }
}

/// Runs `pesto` against the mock server for one small file, isolated from any
/// real `~/.config/pesto` (which may hold real credentials and hooks) via a
/// scratch `XDG_CONFIG_HOME`.
fn run_pesto(port: u16, check_post_retries: u32, input: &std::path::Path, out: &std::path::Path) -> std::process::Output {
    let xdg_home = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &port.to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "1"])
        .args(["--par2", "0"])
        .arg("--check")
        .args(["--check-delay", "0"])
        .args(["--check-retries", "1"])
        .args(["--check-connections", "1"])
        .args(["--check-post-retries", &check_post_retries.to_string()])
        .arg("--no-hooks")
        .args(["-o", out.to_str().unwrap()])
        .arg(input)
        .output()
        .expect("failed to run pesto")
}

#[test]
fn one_repost_round_is_not_enough_when_the_server_keeps_losing_the_article() {
    // The mock only considers the article found after 3 successful POSTs:
    // the original post (1) + one repost (2) still isn't enough.
    let addr = spawn_mock_server(3);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = run_pesto(addr.port(), 1, &input, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "expected pesto to fail with only 1 check-post-retries round\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("still missing after 1 repost round(s)"),
        "stderr did not report exhausting the single repost round:\n{stderr}"
    );
}

#[test]
fn a_second_repost_round_recovers_an_article_the_first_round_missed() {
    // Same flaky server, but this time pesto gets a second round: original
    // post (1) + repost round 1 (2, still not enough) + repost round 2 (3,
    // now enough) — recovers exactly because the loop doesn't give up after
    // a single round.
    let addr = spawn_mock_server(3);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = run_pesto(addr.port(), 2, &input, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected pesto to succeed with 2 check-post-retries rounds\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("all article(s) confirmed after repost"),
        "stderr did not report a successful recovery:\n{stderr}"
    );
    assert!(
        out.exists(),
        "expected the .nzb to be written once the article was confirmed"
    );

    // Also confirms the file_path fix: repost had to actually re-open
    // `movie.bin` from disk (via the mock server's POST count) rather than
    // failing to find it, which is what made the recovery possible at all.
    let nzb = std::fs::read_to_string(&out).unwrap();
    assert!(nzb.contains("movie.bin"), "nzb:\n{nzb}");
}
