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

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

fn spawn_counting_server(accepted: Arc<AtomicUsize>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            accepted.fetch_add(1, Ordering::SeqCst);
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
            if writer.write_all(b"240 article received\r\n").is_err() {
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
