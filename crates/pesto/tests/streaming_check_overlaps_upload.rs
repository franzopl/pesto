//! Regression guard for the core value of the streaming check queue: it
//! must run *concurrently* with the upload, not as a separate phase that
//! only starts once every segment has posted (the old `--check` model).
//!
//! The mock server adds a small artificial delay before acknowledging each
//! `POST`, so a several-segment upload takes long enough that a STAT check
//! (fired `--check-delay 0` after the *first* segment posts, on its own
//! dedicated connection) can be observed completing before the *last*
//! segment's `POST` finishes — proof the two run side by side rather than
//! sequentially.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::time::Duration;

/// Mock NNTP server: every `POST` is accepted with a `240` after a small
/// artificial delay (so the multi-segment upload takes measurable time);
/// every `STAT` is answered `223` (found) immediately.
fn spawn_slow_accept_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || handle_connection(stream));
        }
    });

    addr
}

fn handle_connection(stream: TcpStream) {
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
            }
            // Artificial per-article latency: makes an N-segment upload take
            // long enough that a concurrently running check has time to
            // finish checking an earlier segment before the last one posts.
            std::thread::sleep(Duration::from_millis(150));
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if let Some(_id) = command.strip_prefix("STAT ") {
            if writer.write_all(b"223 0 article exists\r\n").is_err() {
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

/// Parses a `pesto -vvv --log-file` trace log line's leading RFC3339
/// timestamp (e.g. `2026-07-14T00:41:23.980287Z ...`).
fn line_timestamp(line: &str) -> Option<&str> {
    line.split_whitespace().next()
}

#[test]
fn check_completes_an_early_segment_before_the_last_segment_finishes_posting() {
    let addr = spawn_slow_accept_server();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    // 8 segments of 100 bytes each; the server's 150ms/POST latency makes
    // this take ~1.2s total on a single connection — comfortably enough for
    // a `--check-delay 0` check on segment 1 to land while segments 2-8 are
    // still posting.
    std::fs::write(&input, vec![0xABu8; 800]).unwrap();
    let out = dir.path().join("out.nzb");
    let log = dir.path().join("trace.log");
    let xdg_home = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &addr.port().to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "1"])
        .args(["--article-size", "100"])
        .args(["--par2", "0"])
        .arg("--check")
        .args(["--check-delay", "0"])
        .args(["--check-connections", "1"])
        .args(["-vvv", "--log-file", log.to_str().unwrap()])
        .arg("--no-hooks")
        .args(["-o", out.to_str().unwrap()])
        .arg(&input)
        .output()
        .expect("failed to run pesto");

    assert!(
        output.status.success(),
        "expected pesto to succeed\nstderr:\n{}",
        String::from_utf8_lossy(&output.stderr)
    );

    let log_text = std::fs::read_to_string(&log).expect("trace log should exist");

    let last_post_ts = log_text
        .lines()
        .filter(|l| l.contains("code=240"))
        .filter_map(line_timestamp)
        .next_back()
        .expect("expected at least one code=240 line in the trace log");

    let first_stat_ok_ts = log_text
        .lines()
        .filter(|l| l.contains("code=223"))
        .filter_map(line_timestamp)
        .next()
        .expect("expected at least one successful STAT (code=223) line in the trace log");

    assert!(
        first_stat_ok_ts < last_post_ts,
        "expected the streaming check to confirm an early segment before the \
         last segment finished posting (proving check runs concurrently with \
         upload, not after it) — first STAT ok at {first_stat_ok_ts}, last \
         POST accepted at {last_post_ts}"
    );
}
