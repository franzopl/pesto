//! End-to-end tests of the `penne download` CLI against a local fake NNTP
//! server (loopback only — no real Usenet server). Drives the actual
//! compiled binary, not just the library, mirroring the synchronous
//! mock-server pattern used by
//! `crates/pesto/tests/server_substituted_message_id.rs`.

mod support;

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::process::{Command, Output};

use pesto::nzb::NzbMeta;
use pesto::poster::PostedSegment;
use pesto::yenc::{encode_part, PartSpec};
use support::{build_fixture_set, FixtureFile};

/// Spawn a fake NNTP server that only understands `BODY` and `QUIT`. `known`
/// maps bare Message-IDs to the article body the client should get back;
/// anything else gets a `430`.
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

fn segment(file_name: &str, part: u32, total: u32, message_id: &str, size: u64) -> PostedSegment {
    PostedSegment {
        file_name: file_name.into(),
        file_path: file_name.into(),
        subject_name: file_name.into(),
        file_size: size,
        part,
        total,
        message_id: format!("<{message_id}>"),
        bytes: size,
        from: "poster <p@x>".into(),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
    }
}

fn write_nzb(dir: &Path, segments: Vec<PostedSegment>) -> std::path::PathBuf {
    let groups = vec!["alt.binaries.test".to_string()];
    let xml = pesto::nzb::generate(&groups, &segments, &NzbMeta::default());
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

fn run_penne_download(nzb_path: &Path, config_path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_penne"))
        .arg("download")
        .arg(nzb_path)
        .args(["--config", config_path.to_str().unwrap()])
        .output()
        .unwrap()
}

fn run_penne_download_stat(nzb_path: &Path, config_path: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_penne"))
        .arg("download")
        .arg(nzb_path)
        .args(["--config", config_path.to_str().unwrap()])
        .arg("--stat")
        .output()
        .unwrap()
}

fn run_penne_download_with_mode(nzb_path: &Path, config_path: &Path, mode: &str) -> Output {
    Command::new(env!("CARGO_BIN_EXE_penne"))
        .arg("download")
        .arg(nzb_path)
        .args(["--config", config_path.to_str().unwrap()])
        .args(["--mode", mode])
        .output()
        .unwrap()
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

    let mut known = HashMap::new();
    known.insert("art1@test", encoded.body);
    let addr = spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");

    let nzb_path = write_nzb(
        dir.path(),
        vec![segment(
            "greeting.txt",
            1,
            1,
            "art1@test",
            body_len.max(data.len() as u64),
        )],
    );
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download(&nzb_path, &config_path);
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
    let mut known = HashMap::new();
    known.insert("art-that-exists@test", b"unused".to_vec());
    let addr = spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");

    let nzb_path = write_nzb(
        dir.path(),
        vec![segment("ghost.bin", 1, 1, "does-not-exist@test", 10)],
    );
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download(&nzb_path, &config_path);
    assert!(!output.status.success());
    assert!(!download_dir.join("ghost.bin").exists());
}

#[test]
fn download_recovers_a_fully_missing_segment_via_par2() {
    // The whole point of Phase 6: a segment never arrives (server never has
    // it), so `assemble` writes nothing for that file at all — but the
    // release also shipped PAR2 recovery data, which `penne download`
    // should use to recreate the file anyway.
    let original: Vec<u8> = (0..512u32).map(|i| i as u8).collect();
    let slice_size = 64;
    let total_slices = original.len() / slice_size; // 8
    let fixture_dir = build_fixture_set(
        &[FixtureFile {
            name: "movie.bin",
            data: original.clone(),
        }],
        slice_size,
        total_slices, // 100% redundancy: enough to rebuild the whole file
    );
    let par2_index = std::fs::read(fixture_dir.join("base.par2")).unwrap();
    let par2_vol =
        std::fs::read(fixture_dir.join(format!("base.vol000+{total_slices:03}.par2"))).unwrap();
    std::fs::remove_dir_all(&fixture_dir).ok();

    // Split movie.bin into two yEnc parts; the fake server will only ever
    // serve the first one.
    let half = original.len() / 2;
    let part1 = encode_part(
        "movie.bin",
        original.len() as u64,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        &original[..half],
        128,
        None,
    );
    let par2_index_encoded = encode_part(
        "base.par2",
        par2_index.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &par2_index,
        128,
        None,
    );
    let par2_vol_encoded = encode_part(
        &format!("base.vol000+{total_slices:03}.par2"),
        par2_vol.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &par2_vol,
        128,
        None,
    );

    let mut known = HashMap::new();
    known.insert("seg1@test", part1.body);
    // "seg2@test" (the second half of movie.bin) is deliberately absent.
    known.insert("par2idx@test", par2_index_encoded.body);
    known.insert("par2vol@test", par2_vol_encoded.body);
    let addr = spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");

    let nzb_path = write_nzb(
        dir.path(),
        vec![
            segment("movie.bin", 1, 2, "seg1@test", half as u64),
            segment(
                "movie.bin",
                2,
                2,
                "seg2@test",
                (original.len() - half) as u64,
            ),
            segment("base.par2", 1, 1, "par2idx@test", par2_index.len() as u64),
            segment(
                &format!("base.vol000+{total_slices:03}.par2"),
                1,
                1,
                "par2vol@test",
                par2_vol.len() as u64,
            ),
        ],
    );
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download(&nzb_path, &config_path);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let recovered = std::fs::read(download_dir.join("movie.bin")).unwrap();
    assert_eq!(recovered, original);
}

#[test]
fn download_prints_live_progress_instead_of_staying_silent_until_done() {
    // Regression test: `penne download` used to build the whole
    // `DownloadOutcome` before printing anything at all, so a large release
    // looked hung for however long the fetch took. `print_progress` should
    // now report as segments land.
    let part1 = b"first half of the file".to_vec();
    let part2 = b"second half of the file".to_vec();
    let encoded1 = encode_part(
        "movie.bin",
        (part1.len() + part2.len()) as u64,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        &part1,
        128,
        None,
    );
    let encoded2 = encode_part(
        "movie.bin",
        (part1.len() + part2.len()) as u64,
        PartSpec {
            number: 2,
            total: 2,
            offset: part1.len() as u64,
        },
        &part2,
        128,
        None,
    );

    let mut known = HashMap::new();
    known.insert("seg1@test", encoded1.body);
    known.insert("seg2@test", encoded2.body);
    let addr = spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");

    let nzb_path = write_nzb(
        dir.path(),
        vec![
            segment("movie.bin", 1, 2, "seg1@test", part1.len() as u64),
            segment("movie.bin", 2, 2, "seg2@test", part2.len() as u64),
        ],
    );
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download(&nzb_path, &config_path);
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let stderr = String::from_utf8_lossy(&output.stderr);
    // Progress lives on stderr (matching `pesto`'s own convention, and
    // keeping stdout clean for the final per-file result lines). The test
    // harness captures the child's stderr via a pipe, not a TTY, so the
    // renderer takes its non-interactive, one-line-per-percent path —
    // deterministic and easy to assert on, unlike the redraw-in-place TTY
    // panel.
    assert!(
        stderr.contains("fetching: 1/2 segments (50%)"),
        "stderr did not contain a progress line at 50%%:\n{stderr}"
    );
    assert!(
        stderr.contains("fetching: 2/2 segments (100%)"),
        "stderr did not contain a progress line at 100%%:\n{stderr}"
    );
    assert!(
        stderr.contains("assembled: movie.bin"),
        "stderr did not report the file being assembled:\n{stderr}"
    );
}

#[test]
fn stat_reports_complete_and_downloads_nothing_when_every_segment_is_present() {
    let mut known = HashMap::new();
    known.insert("seg1@test", b"irrelevant, STAT never fetches this".to_vec());
    let addr = spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");
    let nzb_path = write_nzb(dir.path(), vec![segment("movie.bin", 1, 1, "seg1@test", 4)]);
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download_stat(&nzb_path, &config_path);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("complete: movie.bin (1/1 segments)"));
    // The concise closing summary leads with the percentage of articles
    // present — the number that matters most at a glance.
    assert!(
        stdout.contains("articles present: 1/1 (100.0%)"),
        "stdout did not report the present-article percentage:\n{stdout}"
    );
    assert!(stdout.contains("files complete:   1/1"));
    // The whole point of --stat is that it's cheap — report exactly how
    // cheap, in a human-readable size, not just a raw byte count.
    assert!(
        stdout.contains("data used:") && stdout.contains("STAT only"),
        "stdout did not report bytes used for the check:\n{stdout}"
    );
    // --stat never downloads: the destination directory shouldn't even
    // have been created, let alone contain a file.
    assert!(!download_dir.exists());
}

#[test]
fn mode_download_skips_par2_repair_and_leaves_an_incomplete_file_unwritten() {
    // Same fixture as `download_recovers_a_fully_missing_segment_via_par2`
    // (one segment of `movie.bin` never arrives, but PAR2 recovery data
    // could rebuild it) — except this time `--mode download` should never
    // even try. The run must still succeed (missing/damaged files are only
    // a warning below `--mode repair`, not a hard failure), but the file
    // stays unwritten, exactly as `assemble::AssembleOutcome::Incomplete`
    // leaves it.
    let original: Vec<u8> = (0..512u32).map(|i| i as u8).collect();
    let slice_size = 64;
    let total_slices = original.len() / slice_size;
    let fixture_dir = build_fixture_set(
        &[FixtureFile {
            name: "movie.bin",
            data: original.clone(),
        }],
        slice_size,
        total_slices,
    );
    let par2_index = std::fs::read(fixture_dir.join("base.par2")).unwrap();
    let par2_vol =
        std::fs::read(fixture_dir.join(format!("base.vol000+{total_slices:03}.par2"))).unwrap();
    std::fs::remove_dir_all(&fixture_dir).ok();

    let half = original.len() / 2;
    let part1 = encode_part(
        "movie.bin",
        original.len() as u64,
        PartSpec {
            number: 1,
            total: 2,
            offset: 0,
        },
        &original[..half],
        128,
        None,
    );
    let par2_index_encoded = encode_part(
        "base.par2",
        par2_index.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &par2_index,
        128,
        None,
    );
    let par2_vol_encoded = encode_part(
        &format!("base.vol000+{total_slices:03}.par2"),
        par2_vol.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &par2_vol,
        128,
        None,
    );

    let mut known = HashMap::new();
    known.insert("seg1@test", part1.body);
    known.insert("par2idx@test", par2_index_encoded.body);
    known.insert("par2vol@test", par2_vol_encoded.body);
    let addr = spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");

    let nzb_path = write_nzb(
        dir.path(),
        vec![
            segment("movie.bin", 1, 2, "seg1@test", half as u64),
            segment(
                "movie.bin",
                2,
                2,
                "seg2@test",
                (original.len() - half) as u64,
            ),
            segment("base.par2", 1, 1, "par2idx@test", par2_index.len() as u64),
            segment(
                &format!("base.vol000+{total_slices:03}.par2"),
                1,
                1,
                "par2vol@test",
                par2_vol.len() as u64,
            ),
        ],
    );
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download_with_mode(&nzb_path, &config_path, "download");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("rerun with --mode repair"),
        "stdout did not warn about the unrepaired file:\n{stdout}"
    );
    assert!(
        !stdout.contains("checking PAR2 recovery data..."),
        "PAR2 verification ran despite --mode download:\n{stdout}"
    );
    assert!(!download_dir.join("movie.bin").exists());
}

#[test]
fn mode_delete_removes_par2_recovery_data_once_everything_is_intact() {
    let original: Vec<u8> = (0..256u32).map(|i| i as u8).collect();
    let slice_size = 64;
    let fixture_dir = build_fixture_set(
        &[FixtureFile {
            name: "movie.bin",
            data: original.clone(),
        }],
        slice_size,
        1,
    );
    let par2_index = std::fs::read(fixture_dir.join("base.par2")).unwrap();
    let par2_vol = std::fs::read(fixture_dir.join("base.vol000+001.par2")).unwrap();
    std::fs::remove_dir_all(&fixture_dir).ok();

    let movie_encoded = encode_part(
        "movie.bin",
        original.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &original,
        128,
        None,
    );
    let par2_index_encoded = encode_part(
        "base.par2",
        par2_index.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &par2_index,
        128,
        None,
    );
    let par2_vol_encoded = encode_part(
        "base.vol000+001.par2",
        par2_vol.len() as u64,
        PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        &par2_vol,
        128,
        None,
    );

    let mut known = HashMap::new();
    known.insert("movie@test", movie_encoded.body);
    known.insert("par2idx@test", par2_index_encoded.body);
    known.insert("par2vol@test", par2_vol_encoded.body);
    let addr = spawn_fake_server(known);

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");

    let nzb_path = write_nzb(
        dir.path(),
        vec![
            segment("movie.bin", 1, 1, "movie@test", original.len() as u64),
            segment("base.par2", 1, 1, "par2idx@test", par2_index.len() as u64),
            segment(
                "base.vol000+001.par2",
                1,
                1,
                "par2vol@test",
                par2_vol.len() as u64,
            ),
        ],
    );
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download_with_mode(&nzb_path, &config_path, "delete");
    assert!(
        output.status.success(),
        "stdout: {}\nstderr: {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        std::fs::read(download_dir.join("movie.bin")).unwrap(),
        original
    );
    assert!(!download_dir.join("base.par2").exists());
    assert!(!download_dir.join("base.vol000+001.par2").exists());
}

#[test]
fn stat_reports_incomplete_and_exits_non_zero_when_a_segment_is_missing() {
    // The server knows nothing at all — every STAT comes back 430.
    let addr = spawn_fake_server(HashMap::new());

    let dir = tempfile::tempdir().unwrap();
    let download_dir = dir.path().join("downloads");
    let nzb_path = write_nzb(dir.path(), vec![segment("movie.bin", 1, 1, "seg1@test", 4)]);
    let config_path = write_config(dir.path(), &download_dir, addr.port());

    let output = run_penne_download_stat(&nzb_path, &config_path);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !output.status.success(),
        "expected a non-zero exit when a segment is missing; stdout:\n{stdout}"
    );
    assert!(stdout.contains("INCOMPLETE: movie.bin (0/1 segments)"));
    assert!(stdout.contains("articles present: 0/1 (0.0%)"));
    assert!(!download_dir.exists());
}
