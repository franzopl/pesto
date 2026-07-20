//! Shared test-only fake NNTP server and `AppState` fixture builder,
//! mirroring the synchronous mock-server pattern used throughout `penne`'s
//! own integration tests (`crates/penne/tests/cli_download_end_to_end.rs`).
//!
//! Each integration test file compiles this module as part of its own
//! binary, so a file that only needs [`test_web_config`]/[`build_state`]
//! (not the fake server) would otherwise warn about unused fake-server
//! helpers — allowed at module level rather than duplicating this file per
//! test binary.
#![allow(dead_code)]

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};

use sugo::config::WebConfig;
use sugo::state::{AppState, SharedState};

/// Spawn a fake NNTP server that only understands `BODY` and `QUIT`. `known`
/// maps bare Message-IDs to the article body the client should get back;
/// anything else gets a `430`.
pub fn spawn_fake_server(known: HashMap<&'static str, Vec<u8>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let known = known.clone();
            std::thread::spawn(move || handle_connection(stream, known));
        }
    });

    addr
}

fn handle_connection(stream: TcpStream, known: HashMap<&'static str, Vec<u8>>) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
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
        let cmd = line.trim_end();

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
}

/// Double any line-leading `.` per RFC 3977 §3.1.1 before sending.
fn write_dot_stuffed(w: &mut impl Write, body: &[u8]) -> std::io::Result<()> {
    for line in body.split_inclusive(|&b| b == b'\n') {
        if line.starts_with(b".") {
            w.write_all(b".")?;
        }
        w.write_all(line)?;
    }
    Ok(())
}

/// A [`WebConfig`] with one `[[servers]]` entry pointing at `127.0.0.1:port`
/// and the given `[web].api_key`.
pub fn test_web_config(download_dir: &Path, port: u16, api_key: &str) -> WebConfig {
    let toml = format!(
        "download_dir = \"{}\"\n\n[[servers]]\nhost = \"127.0.0.1\"\nport = {port}\nssl = false\n\n[web]\napi_key = \"{api_key}\"\n",
        download_dir.display()
    );
    WebConfig::parse(&toml).unwrap()
}

pub fn build_state(data_dir: PathBuf, config: WebConfig) -> SharedState {
    AppState::new(config, data_dir, None)
}
