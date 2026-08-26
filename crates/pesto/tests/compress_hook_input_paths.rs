//! Regression test: `PESTO_INPUT_PATHS` must still list the original input
//! filenames after `--compress`, not just the compressed archive.
//!
//! Post-upload hooks (e.g. a hook that screenshots a video file) detect a
//! video by checking the extension of each path in `PESTO_INPUT_PATHS`. When
//! `--compress` replaces the upload payload with a single `.7z`/`.zip`/`.rar`
//! archive, a hook that only sees the archive path can never find a `.mkv`/
//! `.mp4` extension to trigger on — so screenshots would silently never run.

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
fn compress_preserves_original_filenames_in_hook_input_paths() {
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
    let hooks_dir = xdg_home.path().join("pesto").join("hooks");
    std::fs::create_dir_all(&hooks_dir).unwrap();
    let captured = dir.path().join("captured_input_paths.txt");
    let hook_path = hooks_dir.join("capture.sh");
    std::fs::write(
        &hook_path,
        format!(
            "#!/bin/sh\necho \"$PESTO_INPUT_PATHS\" > {}\n",
            captured.to_str().unwrap()
        ),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &addr.port().to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "1"])
        .args(["--par2", "0"])
        .arg("--no-check")
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

    let captured_paths = std::fs::read_to_string(&captured)
        .unwrap_or_else(|e| panic!("hook never ran / wrote {}: {e}", captured.display()));

    assert!(
        captured_paths.contains("movie.mkv"),
        "PESTO_INPUT_PATHS should still list the original movie.mkv filename \
         after --compress, so hooks can detect it by extension; got: {captured_paths:?}"
    );
    assert!(
        !captured_paths.trim().ends_with(".7z"),
        "PESTO_INPUT_PATHS should not resolve to the compressed archive only; got: {captured_paths:?}"
    );
}

fn which_7z_missing() -> bool {
    pesto::compress::find_binary("7z").is_none()
}
