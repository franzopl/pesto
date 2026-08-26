//! Regression guard for the `--each` connection-broker reuse (ROADMAP.new.md
//! Phase 2, "Connection pool reuse across `--each` episodes"): before that
//! change, every episode of an `--each` batch built and tore down its own
//! NNTP connection pool, so the number of real TCP connections opened by a
//! run scaled with the number of episodes instead of the configured `-n`
//! connection count.
//!
//! This posts a 5-episode `--each` batch with `-n 2` against a mock server
//! that counts every TCP connection it accepts, and asserts the total stays
//! near the connection budget (2) instead of scaling with the episode count
//! (which would show up as ~10).

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// First STAT per article Subject reports missing; later STATs (the
/// `recover_missing` pass's own Message-ID) succeed. Shared across every
/// accepted TCP so overlapping `--jobs` episodes still count per-article.
struct FirstStatMiss {
    subject_by_id: Mutex<HashMap<String, String>>,
    attempts_by_subject: Mutex<HashMap<String, u32>>,
}

fn spawn_counting_server(accepted: Arc<AtomicUsize>) -> SocketAddr {
    spawn_counting_server_with_options(accepted, Duration::ZERO, false)
}

fn spawn_counting_server_with_options(
    accepted: Arc<AtomicUsize>,
    post_delay: Duration,
    first_stat_miss: bool,
) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let miss = first_stat_miss.then(|| {
        Arc::new(FirstStatMiss {
            subject_by_id: Mutex::new(HashMap::new()),
            attempts_by_subject: Mutex::new(HashMap::new()),
        })
    });

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            accepted.fetch_add(1, Ordering::SeqCst);
            let miss = miss.clone();
            std::thread::spawn(move || handle_connection(stream, post_delay, miss));
        }
    });

    addr
}

fn handle_connection(stream: TcpStream, post_delay: Duration, miss: Option<Arc<FirstStatMiss>>) {
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
            let mut message_id = String::new();
            let mut subject = String::new();
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
                if miss.is_some() {
                    if let Ok(text) = std::str::from_utf8(&raw) {
                        if let Some(v) = text.strip_prefix("Message-ID: ") {
                            message_id = v.trim_end().to_string();
                        } else if let Some(v) = text.strip_prefix("Subject: ") {
                            subject = v.trim_end().to_string();
                        }
                    }
                }
            }
            if let Some(miss) = &miss {
                if !message_id.is_empty() && !subject.is_empty() {
                    miss.subject_by_id
                        .lock()
                        .unwrap()
                        .insert(message_id, subject);
                }
            }
            if !post_delay.is_zero() {
                std::thread::sleep(post_delay);
            }
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if let Some(id) = command.strip_prefix("STAT ") {
            let found = if let Some(miss) = &miss {
                let subject = miss.subject_by_id.lock().unwrap().get(id).cloned();
                let attempt = subject.map(|s| {
                    let mut attempts = miss.attempts_by_subject.lock().unwrap();
                    let n = attempts.entry(s).or_insert(0);
                    *n += 1;
                    *n
                });
                attempt.is_some_and(|n| n > 1)
            } else {
                true
            };
            let resp: &[u8] = if found {
                b"223 0 article exists\r\n"
            } else {
                b"430 no such article found\r\n"
            };
            if writer.write_all(resp).is_err() {
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

#[test]
fn each_batch_reuses_connections_instead_of_one_pool_per_episode() {
    const CONNECTIONS: usize = 2;
    const EPISODES: usize = 5;

    let accepted = Arc::new(AtomicUsize::new(0));
    let addr = spawn_counting_server(accepted.clone());

    let dir = tempfile::tempdir().unwrap();
    for i in 0..EPISODES {
        let entry_dir = dir.path().join(format!("episode_{i:02}"));
        std::fs::create_dir_all(&entry_dir).unwrap();
        std::fs::write(entry_dir.join("movie.bin"), vec![0xABu8; 4096]).unwrap();
    }
    let out = dir.path().join("out.nzb");
    let xdg_home = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &addr.port().to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", &CONNECTIONS.to_string()])
        .args(["--article-size", "4096"])
        .args(["--par2", "0"])
        .arg("--no-hooks")
        .arg("--no-check")
        .arg("--each")
        .args(["-o", out.to_str().unwrap()])
        .arg(dir.path())
        .output()
        .expect("failed to run pesto");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected the --each batch to succeed\nstderr:\n{stderr}"
    );

    // Without connection reuse, each of the 5 episodes would build its own
    // pool of up to `CONNECTIONS` fresh sockets (≈10 total here). With the
    // broker, the whole batch shares one pool sized to `CONNECTIONS`, so the
    // total accepted-connection count should stay near that budget — a
    // little slack is allowed for the initial connect/greeting exchange,
    // but nowhere near one full pool's worth per episode.
    let total = accepted.load(Ordering::SeqCst);
    assert!(
        total <= CONNECTIONS + 1,
        "expected connections to be reused across the {EPISODES} episodes \
         (≈{CONNECTIONS} total), but the mock server accepted {total} \
         connections — looks like each episode is opening its own pool again"
    );
}

/// T17: `--each --jobs 2 --check` with overlapping start must not open a
/// second peak of sockets for check workers / `scale_up` / `recover_missing`.
/// Episode B blocks on the broker until A has drained+recovered and
/// checkined the whole set.
///
/// Geometry: auto `-n 4` is 1 check + 3 upload. Three segments per episode
/// so `worker_count == upload_conns` and every budgeted post slot connects.
/// First STAT per article misses and `--check-post-retries 0` skips the
/// streaming repost, so `recover_missing` runs on the drained slots.
#[test]
fn each_jobs_check_overlapping_start_honors_connection_budget() {
    const CONNECTIONS: usize = 4;
    const EPISODES: usize = 2;
    const ARTICLE_SIZE: usize = 4096;
    const SEGMENTS: usize = 3;

    let accepted = Arc::new(AtomicUsize::new(0));
    // Slow POSTs so episode B is already waiting on checkout while A still
    // holds the full budget (check + upload + drain + recover), not two
    // sequential `--each` runs that would never overlap.
    let addr =
        spawn_counting_server_with_options(accepted.clone(), Duration::from_millis(200), true);

    let dir = tempfile::tempdir().unwrap();
    for i in 0..EPISODES {
        let entry_dir = dir.path().join(format!("episode_{i:02}"));
        std::fs::create_dir_all(&entry_dir).unwrap();
        std::fs::write(
            entry_dir.join("movie.bin"),
            vec![0xABu8; ARTICLE_SIZE * SEGMENTS],
        )
        .unwrap();
    }
    let out = dir.path().join("out.nzb");
    let xdg_home = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &addr.port().to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", &CONNECTIONS.to_string()])
        .args(["--article-size", &ARTICLE_SIZE.to_string()])
        .args(["--par2", "0"])
        .arg("--no-hooks")
        .arg("--check")
        .args(["--check-delay", "0"])
        .args(["--check-retries", "1"])
        .args(["--check-post-retries", "0"])
        .args(["--check-recover-percent", "100"])
        .arg("--each")
        .args(["--jobs", "2"])
        .args(["-o", out.to_str().unwrap()])
        .arg(dir.path())
        .output()
        .expect("failed to run pesto");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected the overlapping --each --jobs 2 --check batch to succeed\nstderr:\n{stderr}"
    );

    let total = accepted.load(Ordering::SeqCst);
    assert!(
        total <= CONNECTIONS,
        "expected accept() ≤ {CONNECTIONS} (not 2×, and not N+1 extra-socket slack) \
         with overlapping --jobs 2 --check, but the mock server accepted {total} \
         connections — check workers, scale_up, or recover_missing opened sockets \
         outside the broker budget"
    );
}

/// T22: `-n 1 --check` (auto) is a start-up error, not a silent skip.
#[test]
fn n1_check_auto_is_a_startup_error() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");
    let xdg_home = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", "9"])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "1"])
        .arg("--check")
        .args(["--par2", "0"])
        .arg("--no-hooks")
        .args(["-o", out.to_str().unwrap()])
        .arg(&input)
        .output()
        .expect("failed to run pesto");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        !output.status.success(),
        "expected `-n 1 --check` to fail at start-up, not silently skip checking\nstderr:\n{stderr}"
    );
    assert!(
        stderr.contains("--no-check") || stderr.contains("STAT pool"),
        "start-up error must be actionable (raise -n, lower --check-connections, or --no-check):\n{stderr}"
    );
}
