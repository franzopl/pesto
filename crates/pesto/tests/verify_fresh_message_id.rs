//! End-to-end test for `--verify`: confirms that when a genuine `240` POST
//! accept is followed by a `STAT` miss, the retry uses a freshly generated
//! Message-ID rather than the original one.
//!
//! This mirrors the fix applied to `--check`'s repost pass (see
//! `check_post_retries.rs`): real-world observation shows a server can
//! register a Message-ID in its dedup history at accept time regardless of
//! whether the article body actually lands in the readable spool, so a
//! same-ID retry can loop forever without the article ever becoming
//! STAT-findable. `--verify`'s inline per-article retry had the identical
//! same-ID bug — this test guards against a regression back to it.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Starts a background mock NNTP server that accepts every `POST` with a
/// plain `240`, but only makes an article `STAT`-findable once its
/// Message-ID is not among the first `cursed_count` *distinct* IDs the
/// server has ever seen (in first-seen order). Identical model to
/// `spawn_flaky_dedup_server` in `check_post_retries.rs`.
fn spawn_flaky_dedup_server(cursed_count: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || handle_connection(stream, seen, cursed_count));
        }
    });

    addr
}

fn handle_connection(stream: TcpStream, seen: Arc<Mutex<Vec<String>>>, cursed_count: usize) {
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
                let mut seen = seen.lock().unwrap();
                if !seen.contains(&id) {
                    seen.push(id);
                }
            }
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if let Some(id) = command.strip_prefix("STAT ") {
            let found = {
                let seen = seen.lock().unwrap();
                seen.iter()
                    .position(|x| x == id)
                    .is_some_and(|ord| ord >= cursed_count)
            };
            let resp = if found {
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
        } else if writer.write_all(b"500 unknown command\r\n").is_err() {
            return;
        }
    }
}

fn run_pesto(
    port: u16,
    retries: u32,
    input: &std::path::Path,
    out: &std::path::Path,
) -> std::process::Output {
    let xdg_home = tempfile::tempdir().unwrap();
    Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &port.to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "1"])
        .args(["--par2", "0"])
        .arg("--verify")
        .args(["--retries", &retries.to_string()])
        .arg("--no-hooks")
        .args(["-o", out.to_str().unwrap()])
        .arg(input)
        .output()
        .expect("failed to run pesto")
}

#[test]
fn verify_recovers_via_fresh_message_id_after_stat_miss() {
    // The first 2 distinct Message-IDs the server sees are cursed: the
    // original post (1st) and the first retry's fresh ID (2nd) both miss
    // STAT. The second retry's fresh ID (3rd distinct ID) is past
    // `cursed_count` and lands — 3 retries covers exactly that.
    let addr = spawn_flaky_dedup_server(2);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = run_pesto(addr.port(), 3, &input, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected --verify to recover via a fresh Message-ID\nstderr:\n{stderr}"
    );
    assert!(
        out.exists(),
        "expected the .nzb to be written once the article was verified"
    );
}

#[test]
fn verify_fails_when_retries_run_out_before_a_lucky_id() {
    // 3 distinct IDs are cursed; with only 2 retries (2 distinct IDs tried:
    // the original + one fresh retry) neither lands.
    let addr = spawn_flaky_dedup_server(3);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = run_pesto(addr.port(), 2, &input, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "expected --verify to fail when it runs out of retries before a lucky ID\nstderr:\n{stderr}"
    );
    assert!(
        !out.exists(),
        "the NZB must not be written when the segment never verified"
    );
}
