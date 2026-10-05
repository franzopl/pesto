//! Compression integration using a mock NNTP server, without executing hooks.
//! Hook input paths and environment values are checked by pure CLI unit tests.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;

fn spawn_accepting_server() -> SocketAddr {
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
            if writer
                .write_all(b"240 <article@test> Article received OK\r\n")
                .is_err()
            {
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
fn compression_posts_the_archive_without_executing_hooks() {
    if which_7z_missing() {
        eprintln!("skipping: 7z not found in PATH");
        return;
    }

    let addr = spawn_accepting_server();
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("movie.mkv");
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
        .args([
            "--no-check",
            "--no-hooks",
            "--no-history",
            "--no-session-log",
        ])
        .arg("--compress")
        .args(["-o", out.to_str().unwrap()])
        .arg(&input)
        .output()
        .expect("failed to run pesto");

    assert!(
        output.status.success(),
        "pesto exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let nzb = std::fs::read_to_string(out).unwrap();
    assert!(
        nzb.contains(".7z"),
        "the upload must contain the compressed archive: {nzb}"
    );
}

fn which_7z_missing() -> bool {
    pesto::compress::find_binary("7z").is_none()
}
