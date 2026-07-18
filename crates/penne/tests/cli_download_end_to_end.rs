//! End-to-end test of the `penne download` CLI: parses a `.nzb`, fetches its
//! one article from a local fake NNTP server (loopback only — no real
//! Usenet server), yEnc-decodes it, and writes the resulting file to disk —
//! the full Phase 2/3/4 pipeline driven through the actual binary, not just
//! the library. Mirrors the synchronous mock-server pattern used by
//! `crates/pesto/tests/server_substituted_message_id.rs`.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;

use pesto::nzb::NzbMeta;
use pesto::poster::PostedSegment;
use pesto::yenc::{encode_part, PartSpec};

fn spawn_fake_server(message_id: &'static str, body: Vec<u8>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let body = body.clone();
            std::thread::spawn(move || handle_connection(stream, message_id, body));
        }
    });

    addr
}

fn handle_connection(stream: TcpStream, message_id: &str, body: Vec<u8>) {
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
            if id == message_id {
                let header = format!("222 0 <{id}> body\r\n");
                if writer.write_all(header.as_bytes()).is_err()
                    || write_dot_stuffed(&mut writer, &body).is_err()
                    || writer.write_all(b".\r\n").is_err()
                {
                    return;
                }
            } else if writer.write_all(b"430 No such article\r\n").is_err() {
                return;
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

#[test]
fn download_fetches_decodes_and_writes_the_file() {
    let data = b"hello from penne end-to-end test".to_vec();
    let encoded = encode_part(
        "greeting.txt",
        data.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &data,
        128,
        None,
    );
    let body_len = encoded.body.len() as u64;

    let addr = spawn_fake_server("art1@test", encoded.body);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");

    // Build the .nzb via pesto's own generator, exactly as a real client
    // would produce/consume it.
    let groups = vec!["alt.binaries.test".to_string()];
    let segments = vec![PostedSegment {
        file_name: "greeting.txt".into(),
        file_path: "greeting.txt".into(),
        subject_name: "greeting.txt".into(),
        file_size: data.len() as u64,
        part: 1,
        total: 1,
        message_id: "<art1@test>".into(),
        bytes: body_len,
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
    }];
    let xml = pesto::nzb::generate(&groups, &segments, &NzbMeta::default());
    let nzb_path = dir.path().join("test.nzb");
    std::fs::write(&nzb_path, xml).unwrap();

    let config_path = dir.path().join("penne.toml");
    std::fs::write(
        &config_path,
        format!(
            "download_dir = \"{}\"\n\n[[servers]]\nhost = \"127.0.0.1\"\nport = {}\nssl = false\n",
            download_dir.display(),
            addr.port()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_penne"))
        .arg("download")
        .arg(&nzb_path)
        .args(["--config", config_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let written = std::fs::read(download_dir.join("greeting.txt")).unwrap();
    assert_eq!(written, data);
}

#[test]
fn download_reports_failure_when_article_is_missing() {
    let addr = spawn_fake_server("art-that-exists@test", b"unused".to_vec());

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");

    let groups = vec!["alt.binaries.test".to_string()];
    let segments = vec![PostedSegment {
        file_name: "ghost.bin".into(),
        file_path: "ghost.bin".into(),
        subject_name: "ghost.bin".into(),
        file_size: 10,
        part: 1,
        total: 1,
        message_id: "<does-not-exist@test>".into(),
        bytes: 10,
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
    }];
    let xml = pesto::nzb::generate(&groups, &segments, &NzbMeta::default());
    let nzb_path = dir.path().join("test.nzb");
    std::fs::write(&nzb_path, xml).unwrap();

    let config_path = dir.path().join("penne.toml");
    std::fs::write(
        &config_path,
        format!(
            "download_dir = \"{}\"\n\n[[servers]]\nhost = \"127.0.0.1\"\nport = {}\nssl = false\n",
            download_dir.display(),
            addr.port()
        ),
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_penne"))
        .arg("download")
        .arg(&nzb_path)
        .args(["--config", config_path.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(!download_dir.join("ghost.bin").exists());
}
