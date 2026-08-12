//! End-to-end test of `penne check`'s CLI flag interaction: the live
//! progress bar and its startup banner write to stderr, never stdout, so
//! they must keep working even with `--json` (whose NDJSON output lives on
//! stdout) — a caller piping stdout to a file (e.g. curupira's
//! remote-check.sh) shouldn't have to give up progress feedback to get
//! parseable output. Drives the actual compiled binary against a local
//! fake NNTP server (loopback only), mirroring `cli_download_end_to_end.rs`.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};

use pesto::nzb::NzbMeta;
use pesto::poster::PostedSegment;

fn spawn_fake_stat_server(known: HashSet<&'static str>) -> SocketAddr {
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

fn handle_connection(stream: TcpStream, known: HashSet<&'static str>) {
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

        if let Some(rest) = cmd.strip_prefix("STAT ") {
            let id = rest.trim_start_matches('<').trim_end_matches('>');
            let resp = if known.contains(id) {
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

fn write_nzb(dir: &Path) -> std::path::PathBuf {
    let segment = PostedSegment {
        file_name: "movie.bin".into(),
        file_path: Path::new("movie.bin").into(),
        subject_name: "movie.bin".into(),
        wire_name: "movie.bin".into(),
        file_size: 100,
        part: 1,
        total: 1,
        message_id: "<seg1@test>".into(),
        bytes: 100,
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 0,
        total_files: 0,
    };
    let xml = pesto::nzb::generate(
        &["alt.binaries.test".to_string()],
        &[segment],
        &NzbMeta::default(),
        pesto::config::ObfuscateMode::None,
    );
    let nzb_path = dir.join("test.nzb");
    std::fs::write(&nzb_path, xml).unwrap();
    nzb_path
}

fn write_config(dir: &Path, port: u16) -> std::path::PathBuf {
    let config_path = dir.join("penne.toml");
    std::fs::write(
        &config_path,
        format!(
            "download_dir = \"{}\"\n\n[[servers]]\nhost = \"127.0.0.1\"\nport = {}\nssl = false\n",
            dir.display(),
            port
        ),
    )
    .unwrap();
    config_path
}

fn run_penne_check(nzb_path: &Path, config_path: &Path, extra_args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_penne"))
        .arg("check")
        .arg(nzb_path)
        .args(["--config", config_path.to_str().unwrap()])
        .args(extra_args)
        .output()
        .unwrap()
}

#[test]
fn json_mode_still_prints_the_progress_banner_and_bar_to_stderr() {
    let mut known = HashSet::new();
    known.insert("seg1@test");
    let addr = spawn_fake_stat_server(known);

    let dir = tempfile::tempdir().unwrap();
    let nzb_path = write_nzb(dir.path());
    let config_path = write_config(dir.path(), addr.port());

    let output = run_penne_check(
        &nzb_path,
        &config_path,
        &["--json", "--independent-servers"],
    );
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // The startup banner (eprintln!) is stderr-only — must survive even
    // with --json, which is what this test guards against regressing.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("checking"),
        "expected progress banner on stderr, got: {stderr}"
    );

    // stdout must stay clean NDJSON regardless — the banner/bar living on
    // stderr must never leak into it.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let line = stdout.lines().next().expect("one JSON line");
    let parsed: serde_json::Value = serde_json::from_str(line).expect("valid JSON");
    assert_eq!(parsed["server"], "127.0.0.1");
    assert_eq!(parsed["present"], 1);
}

#[test]
fn quiet_suppresses_the_progress_banner_even_without_json() {
    let mut known = HashSet::new();
    known.insert("seg1@test");
    let addr = spawn_fake_stat_server(known);

    let dir = tempfile::tempdir().unwrap();
    let nzb_path = write_nzb(dir.path());
    let config_path = write_config(dir.path(), addr.port());

    let output = run_penne_check(&nzb_path, &config_path, &["--quiet"]);
    assert!(output.status.success());

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains("checking"),
        "--quiet must still suppress the banner, got: {stderr}"
    );
}
