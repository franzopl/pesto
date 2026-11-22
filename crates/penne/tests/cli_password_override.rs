//! CLI `--password` transport-override tests for `penne download` (PD12-01).
//!
//! Split out of `cli_download_end_to_end.rs` to keep that suite within its
//! source-size baseline. Drives the actual compiled binary against a local
//! fake NNTP server (loopback only), mirroring that file's pattern: the CLI
//! `--password` is a transport override ahead of `<meta type="password">`;
//! the `yenc_encrypted` marker stays the sole gate for decryption.

use std::collections::HashMap;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Output};

mod support;
use pesto::nzb::NzbMeta;
use pesto::poster::PostedSegment;
use pesto::yenc::{encode_part, PartSpec};
use support::{fixture_kdf_input, other_kdf_input};

/// Spawn a fake NNTP server that only understands `BODY`, `STAT` and `QUIT`
/// (mirrors the helper in `cli_download_end_to_end.rs`).
fn spawn_fake_server(known: HashMap<&'static str, Vec<u8>>) -> std::net::SocketAddr {
    use std::io::{BufRead, BufReader};
    use std::net::{TcpListener, TcpStream};

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
            } else if let Some(rest) = cmd.strip_prefix("STAT ") {
                let id = rest.trim_start_matches('<').trim_end_matches('>');
                let resp = if known.contains_key(id) {
                    format!("223 0 <{id}>\r\n")
                } else {
                    "430 No such article\r\n".to_string()
                };
                if writer.write_all(resp.as_bytes()).is_err() {
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

fn segment(file_name: &str, part: u32, total: u32, message_id: &str, size: u64) -> PostedSegment {
    PostedSegment {
        file_name: file_name.into(),
        file_path: std::path::Path::new(file_name).into(),
        subject_name: file_name.into(),
        wire_name: file_name.into(),
        wire_yenc_name: file_name.into(),
        file_size: size,
        part,
        total,
        message_id: format!("<{message_id}>"),
        bytes: size,
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 0,
        total_files: 0,
        segment_index: 0,
        plaintext_digest: None,
    }
}

fn write_nzb(dir: &Path, segments: Vec<PostedSegment>) -> std::path::PathBuf {
    let groups = vec!["alt.binaries.test".to_string()];
    let xml = pesto::nzb::generate(
        &groups,
        &segments,
        &NzbMeta::default(),
        pesto::config::ObfuscateMode::None,
    );
    let nzb_path = dir.join("test.nzb");
    std::fs::write(&nzb_path, xml).unwrap();
    nzb_path
}

fn write_config(dir: &Path, download_dir: &Path, port: u16) -> std::path::PathBuf {
    let config_path = dir.join("penne.toml");
    std::fs::write(
        &config_path,
        format!(
            "download_dir = \"{}\"\n\n[[servers]]\nhost = \"127.0.0.1\"\nport = {}\nssl = false\n",
            download_dir.display(),
            port
        ),
    )
    .unwrap();
    config_path
}

fn run_penne_download_with_password(nzb_path: &Path, config_path: &Path, password: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_penne"))
        .arg("download")
        .arg(nzb_path)
        .args(["--config", config_path.to_str().unwrap()])
        .args(["--password", password])
        .output()
        .unwrap()
}

#[test]
fn cli_password_overrides_meta_for_encrypted_download() {
    // Q1(a) / PD12-01: the CLI `--password` is a transport override ahead of
    // `<meta type="password">`; the `yenc_encrypted` marker stays the gate.
    let kdf_input = fixture_kdf_input();
    let salt = pesto::crypto::generate_alphabet_salt();
    let session =
        std::sync::Arc::new(pesto::crypto::EncryptionSession::new(kdf_input, salt).unwrap());
    let adapter = pesto::crypto::UploadEncryptionAdapter::new(session);

    let original = b"CLI override encrypted payload";
    let spec = PartSpec {
        number: 1,
        total: 1,
        offset: 0,
    };
    let mut body = Vec::new();
    let encoded = adapter
        .encode_article(
            "cli-secret.bin",
            original.len() as u64,
            spec,
            original,
            128,
            1u32,
            &mut body,
        )
        .unwrap();

    let seg = segment(
        "cli-secret.bin",
        1,
        1,
        "cli-enc1@test",
        original.len() as u64,
    );
    let groups = vec!["alt.binaries.test".to_string()];

    let cases: [(&str, Option<String>); 2] = [
        ("absent", None),
        ("wrong", Some(other_kdf_input().to_string())),
    ];
    for (label, meta_password) in cases {
        let mut known = HashMap::new();
        known.insert("cli-enc1@test", encoded.body.clone());
        let addr = spawn_fake_server(known);
        let dir = tempfile::tempdir().unwrap();
        let download_dir = dir.path().join("downloads");
        let nzb_path = dir.path().join("test.nzb");
        let meta = NzbMeta {
            password: meta_password,
            yenc_encrypted: true,
            ..Default::default()
        };
        let xml = pesto::nzb::generate(
            &groups,
            std::slice::from_ref(&seg),
            &meta,
            pesto::config::ObfuscateMode::None,
        );
        std::fs::write(&nzb_path, xml).unwrap();
        let config_path = write_config(dir.path(), &download_dir, addr.port());

        let output = run_penne_download_with_password(&nzb_path, &config_path, kdf_input);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            output.status.success(),
            "{label} meta password: stdout:\n{stdout}\nstderr:\n{stderr}"
        );
        assert_eq!(
            std::fs::read(download_dir.join("cli-secret.bin")).unwrap(),
            original,
            "{label} meta password"
        );
    }
}

#[test]
fn cli_password_does_not_enable_decryption_without_marker() {
    // Q1(a) gate: an ordinary (marker-less) NZB + `--password` behaves
    // exactly as before — the flag is archive-only there, so the plain
    // yEnc article downloads unchanged.
    let data = b"ordinary payload with cli password".to_vec();
    let encoded = encode_part(
        "plain-cli.txt",
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

    let mut known = HashMap::new();
    known.insert("plain-cli1@test", encoded.body);
    let addr = spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");
    let nzb_path = write_nzb(
        dir.path(),
        vec![segment(
            "plain-cli.txt",
            1,
            1,
            "plain-cli1@test",
            data.len() as u64,
        )],
    );
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download_with_password(&nzb_path, &config_path, fixture_kdf_input());
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        std::fs::read(download_dir.join("plain-cli.txt")).unwrap(),
        data
    );
}
