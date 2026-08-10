//! Regression test: `--season` combined with `--password` (compression) must
//! generate the season's global PAR2 recovery set over the files actually
//! posted to Usenet — the per-episode archives — not the original,
//! never-posted episode files.
//!
//! `post_season_par2_volumes` used to be handed `entries`, the season
//! directory's original top-level file paths. But under `--compress`/
//! `--password`, each episode is compressed into an archive in a per-entry
//! temp dir, *that* archive is what's segmented and posted, and the temp
//! dir is deleted right after — before the season's global PAR2 step even
//! runs. The season PAR2 therefore described data (name, size, hash) that
//! was never on the wire at all, making it useless for verification/repair
//! against what a downloader actually receives.
//!
//! This test runs the real `pesto` binary end-to-end against a mock NNTP
//! server, reconstructs every posted file exactly as a downloader would
//! (yEnc-decoding the captured articles), and checks the season PAR2 volume
//! against *that* reconstructed directory with the real `par2` CLI. Before
//! the fix this fails, because the described files (original `.mkv` names)
//! don't exist anywhere in what was actually posted.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::process::Command;
use std::sync::{Arc, Mutex};

use pesto::yenc::decode::decode_part;

/// A mock NNTP server that accepts every `POST` with `240` and records the
/// full raw article body (headers + yEnc data), as exact bytes — a lossy
/// `String` capture (as some other tests use, since they only grep for
/// ASCII control lines) would corrupt the binary yEnc payload this test
/// needs to decode back into real archive/PAR2 bytes.
fn spawn_capturing_server() -> (SocketAddr, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let captured: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
    let captured_clone = Arc::clone(&captured);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let captured = Arc::clone(&captured_clone);
            std::thread::spawn(move || handle_connection(stream, captured));
        }
    });

    (addr, captured)
}

fn handle_connection(stream: TcpStream, captured: Arc<Mutex<Vec<Vec<u8>>>>) {
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
            let mut article = Vec::new();
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
                article.extend_from_slice(&raw);
            }
            captured.lock().unwrap().push(article);
            if writer
                .write_all(b"240 <article@pesto.test> article received\r\n")
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

/// Deterministic, mutually independent pseudo-random bytes — incompressible
/// enough that 7z can't shrink episode content away to nothing.
fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len as u64)
        .map(|i| {
            let mut z = i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (seed as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z >> 33) as u8
        })
        .collect()
}

fn which_7z_missing() -> bool {
    pesto::compress::find_binary("7z").is_none()
}

#[test]
fn season_par2_describes_the_posted_archive_not_the_original_episode() {
    if which_7z_missing() {
        eprintln!("skipping: 7z not found in PATH");
        return;
    }
    let par2_missing = Command::new("par2").arg("--version").output().is_err();
    if par2_missing {
        eprintln!("skipping: par2 (par2cmdline) not found in PATH");
        return;
    }

    let (addr, captured) = spawn_capturing_server();

    let root = tempfile::tempdir().unwrap();
    let season_dir = root.path().join("Season01");
    std::fs::create_dir_all(&season_dir).unwrap();
    std::fs::write(season_dir.join("S01E01.mkv"), content(0, 20_000)).unwrap();
    std::fs::write(season_dir.join("S01E02.mkv"), content(1, 20_000)).unwrap();

    let nzb_dir = root.path().join("nzbs");
    std::fs::create_dir_all(&nzb_dir).unwrap();

    let xdg_home = tempfile::tempdir().unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_pesto"))
        .env("XDG_CONFIG_HOME", xdg_home.path())
        .arg("--season")
        .arg(&season_dir)
        .arg("--no-ssl")
        .args(["-s", "127.0.0.1"])
        .args(["-P", &addr.port().to_string()])
        .args(["-g", "alt.binaries.test"])
        .args(["-n", "1"])
        .args(["--jobs", "1"])
        .args(["--par2", "40"])
        // A small article size (rather than a manual --slice-size, which has
        // its own unrelated pre-existing divide-by-zero when set below the
        // article size — see poster::mod's `articles_per_slice` computation)
        // gives the auto slice-size logic enough articles-per-episode to
        // produce real recovery data even for these tiny test files.
        .args(["--article-size", "2000"])
        // Bare `--password` (no explicit value) makes pesto generate and
        // print a random archive password at runtime — this test never
        // needs to know it (PAR2 verification only sees the encrypted
        // archive as opaque bytes), and it keeps no credential-shaped
        // literal in source for secret scanners to (rightly, in general)
        // flag.
        .arg("--password")
        .args(["--nzb-title", "Season01"])
        .args(["--nzb-dir", nzb_dir.to_str().unwrap()])
        .output()
        .expect("failed to run pesto");

    assert!(
        output.status.success(),
        "pesto exited with {}\nstdout:\n{}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    // Reconstruct every posted file exactly as a downloader would: yEnc-decode
    // each captured article and reassemble by name (parts sorted by offset).
    let mut parts_by_name: HashMap<String, Vec<(u64, Vec<u8>)>> = HashMap::new();
    for article in captured.lock().unwrap().iter() {
        let sep = b"\r\n\r\n";
        let body_start = article
            .windows(sep.len())
            .position(|w| w == sep)
            .map(|i| i + sep.len())
            .expect("article missing header/body separator");
        let decoded = decode_part(&article[body_start..]).expect("failed to decode yEnc article");
        parts_by_name
            .entry(decoded.name.clone())
            .or_default()
            .push((decoded.begin, decoded.data));
    }
    assert!(
        !parts_by_name.is_empty(),
        "no articles were captured — the mock server never saw a POST"
    );

    let download_dir = root.path().join("downloaded");
    std::fs::create_dir_all(&download_dir).unwrap();
    let mut downloaded_names: Vec<String> = Vec::new();
    for (name, mut parts) in parts_by_name {
        parts.sort_by_key(|(begin, _)| *begin);
        let mut bytes = Vec::new();
        for (_, data) in parts {
            bytes.extend_from_slice(&data);
        }
        std::fs::write(download_dir.join(&name), &bytes).unwrap();
        downloaded_names.push(name);
    }

    // Sanity check: only the compressed archive was ever posted, never the
    // plaintext episode — otherwise this test wouldn't exercise the bug at
    // all (it would trivially pass with either the old or new behaviour).
    assert!(
        !downloaded_names.iter().any(|n| n.ends_with(".mkv")),
        "a plaintext .mkv episode was posted directly — --password should \
         have replaced it with a compressed archive; got: {downloaded_names:?}"
    );
    assert!(
        downloaded_names.iter().any(|n| n.ends_with(".7z")),
        "expected at least one posted .7z archive; got: {downloaded_names:?}"
    );

    let season_par2: Vec<&String> = downloaded_names
        .iter()
        .filter(|n| n.starts_with("Season01") && n.ends_with(".par2"))
        .collect();
    assert!(
        !season_par2.is_empty(),
        "no season PAR2 volumes were posted; got: {downloaded_names:?}"
    );

    // The decisive check: verify the season PAR2 against exactly what was
    // downloaded. This only succeeds if the PAR2's File Description packets
    // name the real, actually-posted archive(s) with matching size/hash —
    // which is only true once the season PAR2 step reads the compressed
    // archive bytes instead of the original (never-posted) episode files.
    let verify = Command::new("par2")
        .arg("verify")
        .arg("-q")
        .arg(season_par2[0])
        .current_dir(&download_dir)
        .output()
        .expect("failed to run par2 verify");

    assert!(
        verify.status.success(),
        "par2 verify failed against the actually-posted archive — the season \
         PAR2 set doesn't describe what was really posted.\nstdout:\n{}\nstderr:\n{}\n\
         downloaded files: {downloaded_names:?}",
        String::from_utf8_lossy(&verify.stdout),
        String::from_utf8_lossy(&verify.stderr)
    );
}
