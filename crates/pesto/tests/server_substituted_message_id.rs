//! End-to-end guard for a real-world server behavior discovered by comparing
//! `pesto` against `nyuu` on Newshosting: some servers substitute their own
//! Message-ID at `POST` accept time and echo the *actual* one used in the
//! `240` response text, instead of confirming the client-supplied ID.
//! `nyuu` has adopted the server's returned ID in that case since 2016 (see
//! its `RE_POST` matcher in `lib/nntp.js`); a client that keeps tracking its
//! own generated ID instead will never find the article again via `STAT`,
//! because that ID was never the one actually used to store it.
//!
//! The mock server here always substitutes a fixed, different Message-ID in
//! its `240` response, and only ever answers `STAT` truthfully for that
//! substituted ID — never for whatever ID the client originally sent. If
//! `pesto` doesn't adopt the server's returned ID, `--check` would report
//! the article as missing (and it would be genuinely unrecoverable, since
//! nothing was ever actually stored under the client's own ID).

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;

const SUBSTITUTED_ID: &str = "server-assigned-id@substituted.test";

fn spawn_id_substituting_server() -> SocketAddr {
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
            // Always claim a different, server-assigned Message-ID, exactly
            // like a real provider doing dedup/canonicalization at accept
            // time — regardless of what the client sent.
            let resp = format!("240 <{SUBSTITUTED_ID}> Article received OK\r\n");
            if writer.write_all(resp.as_bytes()).is_err() {
                return;
            }
        } else if let Some(id) = command.strip_prefix("STAT ") {
            let bare = id.trim_start_matches('<').trim_end_matches('>');
            let resp = if bare == SUBSTITUTED_ID {
                format!("223 0 <{bare}> article exists\r\n")
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

#[test]
fn pesto_adopts_the_server_returned_message_id() {
    let addr = spawn_id_substituting_server();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.bin");
    std::fs::write(&input, vec![0xABu8; 64]).unwrap();
    let out = dir.path().join("out.nzb");
    let xdg_home = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &addr.port().to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "1"])
        .args(["--par2", "0"])
        .arg("--check")
        .args(["--check-delay", "0"])
        .args(["--check-retries", "1"])
        .args(["--check-connections", "1"])
        .arg("--no-hooks")
        .args(["-o", out.to_str().unwrap()])
        .arg(&input)
        .output()
        .expect("failed to run pesto");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "expected pesto to succeed by adopting the server's returned Message-ID\nstderr:\n{stderr}"
    );
    // If pesto had kept tracking its own generated ID instead, --check would
    // have reported it missing and tried to repost — neither of which
    // should happen here, since the article was correctly found on the very
    // first STAT pass under the server's substituted ID.
    assert!(
        !stderr.contains("not found") && !stderr.contains("reposting"),
        "pesto should have found the article immediately via the server's \
         substituted ID, with no repost needed:\n{stderr}"
    );

    let nzb = std::fs::read_to_string(&out).unwrap();
    assert!(
        nzb.contains(SUBSTITUTED_ID),
        "the .nzb must reference the server's substituted Message-ID:\n{nzb}"
    );
}
