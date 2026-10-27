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
        wire_yenc_name: "movie.bin".into(),
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

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("summary"),
        "expected summary, got: {stdout}"
    );
    assert!(
        !stdout.contains("complete: movie.bin"),
        "--quiet must suppress per-file output, got: {stdout}"
    );
}

#[test]
fn fail_fast_quiet_summary_identifies_the_confirmed_miss() {
    let addr = spawn_fake_stat_server(HashSet::new());
    let dir = tempfile::tempdir().unwrap();
    let nzb_path = write_nzb(dir.path());
    let config_path = write_config(dir.path(), addr.port());

    let output = run_penne_check(&nzb_path, &config_path, &["--fail-fast", "--quiet"]);
    assert_eq!(output.status.code(), Some(1));

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("INCOMPLETE — confirmed missing article"),
        "expected explicit fail-fast verdict, got: {stdout}"
    );
    assert!(
        stdout.contains("first missing:    movie.bin part 1"),
        "expected the first missing article, got: {stdout}"
    );
}

fn parse_ndjson(output: &Output) -> Vec<serde_json::Value> {
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("valid JSON line"))
        .collect()
}

#[test]
fn json_lines_carry_schema_and_penne_version() {
    let mut known = HashSet::new();
    known.insert("seg1@test");
    let addr = spawn_fake_stat_server(known);

    let dir = tempfile::tempdir().unwrap();
    let nzb_path = write_nzb(dir.path());
    let config_path = write_config(dir.path(), addr.port());

    let lines = parse_ndjson(&run_penne_check(
        &nzb_path,
        &config_path,
        &["--json", "--quiet"],
    ));
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["schema_version"], 1);
    assert_eq!(lines[0]["penne_version"], penne::DISPLAY_VERSION);
    // The combined (failover) mode has no single server to attribute a
    // declared retention to.
    assert!(lines[0].get("declared_retention_days").is_none());
}

#[test]
fn independent_servers_report_each_servers_declared_retention() {
    let mut known = HashSet::new();
    known.insert("seg1@test");
    let without_retention = spawn_fake_stat_server(known.clone());
    let with_retention = spawn_fake_stat_server(known);

    let dir = tempfile::tempdir().unwrap();
    let nzb_path = write_nzb(dir.path());
    // Two distinct hostnames for the same loopback interface, so each line's
    // `server` identifies which config entry it came from.
    let config_path = dir.path().join("penne.toml");
    std::fs::write(
        &config_path,
        format!(
            "download_dir = \"{dir}\"\n\n\
             [[servers]]\nhost = \"127.0.0.1\"\nport = {p1}\nssl = false\n\n\
             [[servers]]\nhost = \"localhost\"\nport = {p2}\nssl = false\nretention_days = 5000\n",
            dir = dir.path().display(),
            p1 = without_retention.port(),
            p2 = with_retention.port(),
        ),
    )
    .unwrap();

    let lines = parse_ndjson(&run_penne_check(
        &nzb_path,
        &config_path,
        &["--json", "--quiet", "--independent-servers"],
    ));
    assert_eq!(lines.len(), 2);
    let by_server = |host: &str| {
        lines
            .iter()
            .find(|l| l["server"] == host)
            .unwrap_or_else(|| panic!("no line for {host}: {lines:?}"))
    };
    assert!(by_server("127.0.0.1")
        .get("declared_retention_days")
        .is_none());
    assert_eq!(by_server("localhost")["declared_retention_days"], 5000);
}

#[test]
fn servers_json_lists_selected_servers_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let config_path = dir.path().join("penne.toml");
    std::fs::write(
        &config_path,
        "[[servers]]\nname = \"main\"\nhost = \"news.example.com\"\n\
         username = \"secret-user\"\npassword = \"secret-pass\"\nretention_days = 6000\n\n\
         [[servers]]\nhost = \"backup.example.com\"\n\n\
         [[servers]]\nname = \"block\"\nhost = \"block.example.com\"\nexplicit_only = true\n",
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_penne"))
        .args([
            "servers",
            "--json",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("secret"), "credentials leaked: {stdout}");

    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!([
            { "name": "main", "host": "news.example.com", "retention_days": 6000 },
            { "name": null, "host": "backup.example.com", "retention_days": null },
        ]),
        "explicit_only entries stay out of the default set, as in `check`"
    );
}
