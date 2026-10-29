//! Issue #209: non-ASCII file names must be posted unchanged as UTF-8, like
//! nyuu, ngPost and ParPar do — in the Subject, the yEnc `name=`, the `.nzb`
//! and the PAR2 File Description packets — with no Unicode normalization.
#[path = "full_shared_obfuscation/config.rs"]
mod config;
use config::dry_run_config;

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use pesto::config::{Config, ObfuscateMode};
use pesto::par2::packet::TYPE_FILE_DESC;
use pesto::par2::packet_reader::read_packets;
use pesto::poster::post_files;
use pesto::walk::expand_inputs;

/// Client paths expected for the tree built by [`build_tree`]: the common
/// `Curso/` root is stripped, everything below it is kept byte for byte.
/// `Café` is NFD (`e` + combining acute) and must stay NFD.
const CLIENT_PATHS: [&str; 3] = [
    "1_ Todo dia/010 - Quem é você.ts",
    "Ação.pdf",
    "Cafe\u{301}/legenda.srt",
];

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

/// Build `Curso/` under a fresh temp directory and return `(root, Curso)`.
fn build_tree(tag: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let root = std::env::temp_dir().join(format!("pesto_unicode_{tag}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let release = root.join("Curso");
    for (seed, rel) in CLIENT_PATHS.iter().enumerate() {
        let path = release.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content(seed as u8, 100_000)).unwrap();
    }
    (root, release)
}

type CapturedArticles = Arc<Mutex<Vec<Vec<u8>>>>;

fn spawn_accept_all_server() -> (SocketAddr, CapturedArticles) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let articles: CapturedArticles = Arc::new(Mutex::new(Vec::new()));
    let articles_clone = Arc::clone(&articles);

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let articles = Arc::clone(&articles_clone);
            std::thread::spawn(move || handle_connection(stream, articles));
        }
    });

    (addr, articles)
}

fn handle_connection(stream: TcpStream, articles: CapturedArticles) {
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
            loop {
                let mut raw = Vec::new();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if raw == b".\r\n" {
                    break;
                }
                article.extend_from_slice(&raw);
            }
            articles.lock().unwrap().push(article);
            if writer.write_all(b"240 article received\r\n").is_err() {
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

/// Every File Description name found in the `.par2` files under `dir`.
fn file_desc_names(dir: &std::path::Path) -> Vec<Vec<u8>> {
    let mut names = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("par2") {
            continue;
        }
        for packet in read_packets(&std::fs::read(&path).unwrap()) {
            if packet.packet_type == TYPE_FILE_DESC {
                // File ID (16) + MD5 (16) + 16 KiB MD5 (16) + length (8),
                // then the NUL-padded name.
                let name = &packet.body[56..];
                let end = name.iter().position(|&b| b == 0).unwrap_or(name.len());
                names.push(name[..end].to_vec());
            }
        }
    }
    names.sort();
    names.dedup();
    names
}

/// The bytes of the first header line starting with `prefix`, without CRLF.
fn header<'a>(article: &'a [u8], prefix: &[u8]) -> Option<&'a [u8]> {
    article
        .split(|&b| b == b'\n')
        .find(|line| line.starts_with(prefix))
        .map(|line| &line[prefix.len()..line.len() - usize::from(line.ends_with(b"\r"))])
}

#[tokio::test(flavor = "multi_thread")]
async fn non_ascii_names_reach_the_wire_and_par2_as_raw_utf8() {
    let (addr, articles) = spawn_accept_all_server();
    let (root, release) = build_tree("wire");

    let config = Config {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        connections: 1,
        dry_run: false,
        par2: 10,
        ..dry_run_config(ObfuscateMode::None)
    };
    let inputs = expand_inputs(std::slice::from_ref(&release)).unwrap();
    let outcome = post_files(&config, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    let captured = articles.lock().unwrap().clone();
    for client_path in CLIENT_PATHS {
        let quoted = format!("\"{client_path}\"");
        let article = captured
            .iter()
            .find(|a| {
                header(a, b"Subject: ")
                    .is_some_and(|s| s.windows(quoted.len()).any(|w| w == quoted.as_bytes()))
            })
            .unwrap_or_else(|| panic!("no article whose Subject carries `{client_path}` as UTF-8"));
        let ybegin = header(article, b"=ybegin ").expect("article must have a =ybegin line");
        assert!(
            ybegin.ends_with(format!("name={client_path}").as_bytes()),
            "=ybegin `{}` must carry `{client_path}` as raw UTF-8",
            String::from_utf8_lossy(ybegin)
        );
    }

    let mut expected: Vec<Vec<u8>> = CLIENT_PATHS.iter().map(|p| p.as_bytes().to_vec()).collect();
    expected.sort();
    assert_eq!(file_desc_names(&outcome.par2_temp_dir), expected);

    let nzb = pesto::nzb::generate(
        &config.groups,
        &outcome.segments,
        &pesto::nzb::NzbMeta::default(),
        config.obfuscate,
    );
    for client_path in CLIENT_PATHS {
        assert!(
            nzb.contains(&format!("&quot;{client_path}&quot;")),
            "`{client_path}` missing from the NZB"
        );
    }

    outcome.cleanup_par2_temp_dir().await;
    std::fs::remove_dir_all(&root).ok();
}

#[tokio::test]
async fn non_ascii_names_are_accepted_in_every_obfuscation_mode() {
    for mode in [
        ObfuscateMode::None,
        ObfuscateMode::Full,
        ObfuscateMode::FullShared,
        ObfuscateMode::Light,
        ObfuscateMode::Article,
    ] {
        let (root, release) = build_tree(&format!("{mode:?}"));
        let config = dry_run_config(mode);
        let inputs = expand_inputs(std::slice::from_ref(&release)).unwrap();
        let outcome = post_files(&config, &inputs)
            .await
            .unwrap_or_else(|e| panic!("{mode:?}: {e:#}"));
        for client_path in CLIENT_PATHS {
            let seg = outcome
                .segments
                .iter()
                .find(|s| s.file_name == format!("Curso/{client_path}"))
                .unwrap_or_else(|| panic!("{mode:?}: no segment for `{client_path}`"));
            // `light` mirrors the opaque wire Subject in the NZB instead.
            if mode != ObfuscateMode::Light {
                assert_eq!(&*seg.subject_name, client_path, "{mode:?}");
            }
        }
        std::fs::remove_dir_all(&root).ok();
    }
}
