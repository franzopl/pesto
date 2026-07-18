//! End-to-end test of `penne download` against a *foreign, obfuscated*
//! `.nzb`: no `name` attribute on `<file>` (only `subject`, per the
//! standard NZB 1.1 DTD — see `crates/pesto/src/nzb.rs`'s `parse()`), and
//! hash-like subjects instead of real filenames, the way real-world
//! scene/P2P releases are commonly posted. Drives the actual compiled
//! binary against a local fake NNTP server (loopback only), mirroring the
//! pattern in `tests/cli_download_end_to_end.rs`.

mod support;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};

use pesto::yenc::{encode_part, PartSpec};
use support::{build_fixture_set, FixtureFile};

fn spawn_fake_server(known: HashMap<&'static str, Vec<u8>>) -> SocketAddr {
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

fn run_penne_download(nzb_path: &Path, config_path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_penne"))
        .arg("download")
        .arg(nzb_path)
        .args(["--config", config_path.to_str().unwrap()])
        .output()
        .unwrap()
}

/// A standard (non-`pesto`) NZB: no `name` attribute, hash-like subjects.
/// One `<file>` per `(obfuscated_subject, message_id, byte_len)`.
fn write_obfuscated_nzb(dir: &Path, files: &[(&str, &str, u64)]) -> std::path::PathBuf {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n",
    );
    for (subject, message_id, bytes) in files {
        xml.push_str(&format!(
            "  <file poster=\"poster &lt;p@x&gt;\" date=\"1700000000\" subject=\"{subject} (1/1)\">\n\
             \x20   <groups>\n      <group>alt.binaries.test</group>\n    </groups>\n\
             \x20   <segments>\n      <segment bytes=\"{bytes}\" number=\"1\">{message_id}</segment>\n    </segments>\n\
             \x20 </file>\n"
        ));
    }
    xml.push_str("</nzb>\n");
    let path = dir.join("obfuscated.nzb");
    std::fs::write(&path, xml).unwrap();
    path
}

#[test]
fn recovers_real_names_from_par2_and_tags_the_par2_file_itself() {
    let movie_data = b"totally real movie bytes, honest".to_vec();

    // A PAR2 index (no recovery blocks needed — this test only exercises
    // name recovery + verify, not repair) describing the file under its
    // *real* name, exactly as a genuine release's PAR2 would.
    let fixture_dir = build_fixture_set(
        &[FixtureFile {
            name: "movie.mkv",
            data: movie_data.clone(),
        }],
        64,
        0,
    );
    let par2_bytes = std::fs::read(fixture_dir.join("base.par2")).unwrap();

    let encoded_data = encode_part(
        "whatever.bin", // yEnc name= is irrelevant here; only the .nzb subject matters
        movie_data.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &movie_data,
        128,
        None,
    );
    let encoded_par2 = encode_part(
        "whatever.bin",
        par2_bytes.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &par2_bytes,
        128,
        None,
    );

    let mut known = HashMap::new();
    known.insert("data@obf", encoded_data.body.clone());
    known.insert("par2@obf", encoded_par2.body.clone());
    let addr = spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");

    // Neither subject reveals the real name — that's the whole point.
    let nzb_path = write_obfuscated_nzb(
        dir.path(),
        &[
            ("a1b2c3d4e5f6", "data@obf", encoded_data.body.len() as u64),
            ("f6e5d4c3b2a1", "par2@obf", encoded_par2.body.len() as u64),
        ],
    );
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download(&nzb_path, &config_path);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );

    // The real name was recovered from PAR2, not left as the obfuscated
    // subject-derived name.
    assert!(
        download_dir.join("movie.mkv").exists(),
        "expected movie.mkv to exist under its recovered name; stdout:\n{stdout}"
    );
    assert_eq!(
        std::fs::read(download_dir.join("movie.mkv")).unwrap(),
        movie_data
    );
    assert!(!download_dir.join("a1b2c3d4e5f6").exists());

    // The PAR2 file itself was content-sniffed and tagged, even though its
    // own subject was just as obfuscated as the data file's.
    assert!(download_dir.join("f6e5d4c3b2a1.par2").exists());

    assert!(
        stdout.contains("recovered name (PAR2): a1b2c3d4e5f6 -> movie.mkv"),
        "stdout did not report the PAR2-based recovery:\n{stdout}"
    );
    assert!(
        stdout.contains("par2 file: f6e5d4c3b2a1 -> f6e5d4c3b2a1.par2"),
        "stdout did not report tagging the par2 file:\n{stdout}"
    );
    assert!(
        stdout.contains("PAR2: all files verified intact"),
        "expected verify to succeed once files carry their real names:\n{stdout}"
    );
}
