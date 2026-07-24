//! End-to-end CLI test for `--check-post-retries` against the actual
//! compiled `pesto` binary over a real TCP connection, the same way
//! `batch_order.rs` does.
//!
//! `repost_missing_segments` posts each repost attempt under a **freshly
//! generated** Message-ID rather than the original one (see its doc comment
//! in `poster/mod.rs`): real-world observation is that some servers register
//! a Message-ID in their dedup history at `240`-accept time regardless of
//! whether the article body actually lands in the readable spool, so
//! reposting under the *same* ID can get permanently stuck — every future
//! attempt gets a genuine-looking `240` again without the article ever
//! becoming STAT-findable, not even hours later. `spawn_flaky_dedup_server`
//! models a server that drops a *run* of distinct IDs before one lands
//! correctly, proving that retrying with fresh IDs (not the same one) is
//! what actually recovers the article across `--check-post-retries` attempts.
//!
//! `spawn_always_reject_server` covers a different, simpler failure mode: a
//! server that rejects every repost as a duplicate (`441`) no matter what ID
//! is used. That must never be mistaken for proof of success — see
//! `Connection::repost_parts_confirmed` — regardless of whether the ID is
//! fresh or reused.
//!
//! Also guards against the regression this feature shipped alongside: before
//! the fix, `repost_missing_segments` reconstructed the source file path from
//! the *published* name (`PathBuf::from(&seg.file_name)`) instead of the
//! absolute path, so a repost could silently fail to find the file. Every
//! repost here only succeeds if pesto can actually re-read `movie.bin` from
//! disk, so a regression on that front would show up as the "enough retries"
//! case unexpectedly failing too.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

/// Reads one full POSTed article (headers + body, up to the `.\r\n`
/// terminator) from `reader` and returns its `Message-ID` header value, if
/// present. Shared by every mock server below.
fn read_posted_message_id(reader: &mut BufReader<TcpStream>) -> Option<String> {
    let mut article = Vec::new();
    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) | Err(_) => return None,
            Ok(_) => {}
        }
        if raw == b".\r\n" {
            break;
        }
        article.extend_from_slice(&raw);
    }
    let text = String::from_utf8_lossy(&article);
    text.lines()
        .find_map(|l| l.strip_prefix("Message-ID: "))
        .map(str::trim)
        .map(str::to_string)
}

/// Starts a background mock NNTP server that accepts every `POST` with a
/// plain `240`, but only makes an article `STAT`-findable once its
/// Message-ID is not among the first `cursed_count` *distinct* IDs the
/// server has ever seen (in first-seen order). Models a server that
/// genuinely, silently drops a run of articles at accept time — each still
/// gets an honest `240` — before recovering. Since `repost_missing_segments`
/// posts a fresh ID on every attempt, recovering requires exactly
/// `cursed_count` reposts (one per cursed ID) before a "lucky" ID lands.
fn spawn_flaky_dedup_server(cursed_count: usize) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let seen = Arc::clone(&seen);
            std::thread::spawn(move || handle_flaky_dedup_connection(stream, seen, cursed_count));
        }
    });

    addr
}

fn handle_flaky_dedup_connection(
    stream: TcpStream,
    seen: Arc<Mutex<Vec<String>>>,
    cursed_count: usize,
) {
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
            let id = read_posted_message_id(&mut reader);
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

/// Starts a background mock NNTP server that rejects every `POST` as a
/// duplicate (`441`), regardless of the Message-ID used, and never lets
/// `STAT` find anything. Models a server that can't be talked out of a stuck
/// dedup entry no matter how it's approached — used to confirm a `441`
/// rejection is never mistaken for a successful repost.
fn spawn_always_reject_server() -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            std::thread::spawn(move || handle_always_reject_connection(stream));
        }
    });

    addr
}

fn handle_always_reject_connection(stream: TcpStream) {
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
            let _ = read_posted_message_id(&mut reader);
            if writer
                .write_all(b"441 435 Already exists in history\r\n")
                .is_err()
            {
                return;
            }
        } else if command.starts_with("STAT ") {
            if writer.write_all(b"430 No such article\r\n").is_err() {
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

/// Runs `pesto` against the mock server for one small file, isolated from any
/// real `~/.config/pesto` (which may hold real credentials and hooks) via a
/// scratch `XDG_CONFIG_HOME`.
fn run_pesto(
    port: u16,
    check_post_retries: u32,
    input: &std::path::Path,
    out: &std::path::Path,
) -> std::process::Output {
    run_pesto_with_args(port, check_post_retries, input, out, &[])
}

fn run_pesto_with_args(
    port: u16,
    check_post_retries: u32,
    input: &std::path::Path,
    out: &std::path::Path,
    extra_args: &[&str],
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
        .arg("--check")
        .args(["--check-delay", "0"])
        .args(["--check-retries", "1"])
        .args(["--check-connections", "1"])
        .args(["--check-post-retries", &check_post_retries.to_string()])
        .arg("--no-hooks")
        .args(["-o", out.to_str().unwrap()])
        .args(extra_args)
        .arg(input)
        .output()
        .expect("failed to run pesto")
}

#[test]
fn one_repost_attempt_is_not_enough_when_the_server_keeps_losing_the_article() {
    // The first 2 distinct Message-IDs the server ever sees are cursed: the
    // original post (1st) and the one allowed repost's fresh ID (2nd) both
    // fail STAT. `check-post-retries 1` allows only one repost attempt per
    // article, so it's not enough.
    let addr = spawn_flaky_dedup_server(2);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = run_pesto(addr.port(), 1, &input, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "expected pesto to fail with only 1 check-post-retries attempt\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("still missing after every repost attempt"),
        "stderr did not report exhausting the single repost attempt:\n{stderr}"
    );
}

#[test]
fn a_second_repost_attempt_recovers_an_article_the_first_missed() {
    // Same flaky server: original post (1st distinct ID, cursed) + repost 1
    // (2nd distinct ID, still cursed) + repost 2 (3rd distinct ID, past
    // `cursed_count` — lucky) recovers exactly because the streaming check
    // queue doesn't give up after a single repost, and each attempt uses a
    // fresh ID.
    let addr = spawn_flaky_dedup_server(2);
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = run_pesto(addr.port(), 2, &input, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected pesto to succeed with 2 check-post-retries attempts\nstderr:\n{stderr}"
    );
    // Non-TTY runs report verification through the plain renderer's final
    // line (`… · check 1 verified/0 missing/0 pending`); the standalone
    // "check: all N article(s) verified" line was dropped as a duplicate of
    // the renderer's own summary.
    assert!(
        stderr.contains("check 1 verified") && !stderr.contains("missing after every repost"),
        "stderr did not report a successful recovery:\n{stderr}"
    );
    assert!(
        out.exists(),
        "expected the .nzb to be written once the article was confirmed"
    );

    // Also confirms the file_path fix: repost had to actually re-open
    // `movie.bin` from disk (to re-encode it under the fresh ID) rather than
    // failing to find it, which is what made the recovery possible at all.
    let nzb = std::fs::read_to_string(&out).unwrap();
    assert!(nzb.contains("movie.bin"), "nzb:\n{nzb}");
}

/// Guards against the false-positive "reposted successfully" bug: a server
/// that rejects every repost as a duplicate (`441`), no matter what
/// Message-ID is used, must never have that rejection mistaken for proof the
/// article now exists. Without `Connection::repost_parts_confirmed` (a
/// genuine `240` required), `already_exists()` would treat the `441` as
/// success and the NZB would ship a segment that was never actually stored.
#[test]
fn repost_never_trusts_a_441_duplicate_rejection_as_proof_of_success() {
    let addr = spawn_always_reject_server();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = run_pesto(addr.port(), 1, &input, &out);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "expected pesto to fail against a server that never actually serves the article\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("still missing after every repost attempt"),
        "a 441 duplicate rejection must not be counted as a successful repost — stderr did not report the article as still missing:\n{stderr}"
    );
    assert!(
        !out.exists(),
        "the NZB must not be written when an article is confirmed missing"
    );
}

/// `--allow-incomplete-nzb` is the explicit opt-in to publish anyway (e.g.
/// relying on PAR2 recovery) when articles are still confirmed missing after
/// every repost attempt — it should not silently mask the failure, just stop
/// it from blocking the NZB and post-hooks.
#[test]
fn allow_incomplete_nzb_publishes_despite_confirmed_missing_articles() {
    let addr = spawn_always_reject_server();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");

    let output = run_pesto_with_args(addr.port(), 1, &input, &out, &["--allow-incomplete-nzb"]);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // Still reported as a failure (exit code), since the article really is
    // missing — the flag only changes whether the NZB gets written.
    assert!(
        !output.status.success(),
        "an upload with confirmed-missing articles should still exit non-zero\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("Publishing anyway"),
        "expected the opt-in warning to be printed:\n{stderr}"
    );
    assert!(
        out.exists(),
        "expected the NZB to be written when --allow-incomplete-nzb is set"
    );
}
