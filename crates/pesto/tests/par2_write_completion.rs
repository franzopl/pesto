//! Issue #200: recovery volumes must be complete before they are read back.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use pesto::config::{Config, FileConfig, Overrides};
use pesto::par2::{layout, packet};
use pesto::poster::{generate_and_write_season_par2, post_files_with_progress};
use pesto::progress::ProgressEvent;
use pesto::{walk, yenc};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SLICE_SIZE: usize = 256 * 1024;
const RECOVERY_COUNT: u32 = 7;

fn config(root: &Path, multi_pass: bool) -> Config {
    Config::resolve(
        FileConfig::default(),
        Overrides {
            host: Some("unused.invalid".into()),
            groups: Some(vec!["alt.binaries.test".into()]),
            dry_run: Some(true),
            ssl: Some(false),
            connections: Some(1),
            article_size: Some(128 * 1024),
            par2: Some(10),
            par2_slice_size: Some(SLICE_SIZE as u64),
            par2_recovery_count: Some(RECOVERY_COUNT as usize),
            // Two slices per pass splits the four-block volume across passes.
            par2_memory_limit: Some((SLICE_SIZE * if multi_pass { 2 } else { 16 }) as u64),
            par2_temp_dir: Some(root.join("scratch").to_string_lossy().into_owned()),
            check: Some(false),
            no_hooks: Some(true),
            history: Some(false),
            threads: Some(1),
            ..Default::default()
        },
    )
    .unwrap()
}

fn inputs(root: &Path) -> Vec<PathBuf> {
    ["a.7z.001", "a.7z.002"]
        .into_iter()
        .enumerate()
        .map(|(i, name)| {
            let path = root.join(name);
            std::fs::write(&path, vec![i as u8 + 1; SLICE_SIZE + 1]).unwrap();
            path
        })
        .collect()
}

/// Validate packet boundaries and hashes as well as every expected exponent;
/// a valid yEnc CRC alone cannot detect the missing recovery packets in #200.
fn assert_volume(data: &[u8], volume: layout::VolumeChunk) {
    let mut offset = 0;
    let mut exponents = Vec::new();
    while offset < data.len() {
        let rest = &data[offset..];
        assert!(rest.len() >= packet::HEADER_LEN, "truncated PAR2 header");
        assert_eq!(&rest[..8], b"PAR2\0PKT");
        let len = u64::from_le_bytes(rest[8..16].try_into().unwrap()) as usize;
        assert!(
            len >= packet::HEADER_LEN && len <= rest.len(),
            "truncated PAR2 packet"
        );
        assert_eq!(&rest[16..32], &packet::md5(&rest[32..len]));
        if rest[48..64] == packet::TYPE_RECOVERY {
            assert_eq!(len, packet::HEADER_LEN + 4 + SLICE_SIZE);
            exponents.push(u32::from_le_bytes(rest[64..68].try_into().unwrap()));
        }
        offset += len;
    }
    assert_eq!(
        exponents,
        (volume.first..volume.first + volume.count).collect::<Vec<_>>()
    );
}

async fn assert_queued_volumes(multi_pass: bool) {
    let root = tempfile::tempdir().unwrap();
    let paths = inputs(root.path());
    let config = config(root.path(), multi_pass);
    let files = walk::expand_inputs(&paths).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let outcome = post_files_with_progress(&config, &files, Some(tx), None, None)
        .await
        .unwrap();
    assert!(!outcome.cancelled, "{:?}", outcome.failure_reason);
    assert!(outcome.failed_tasks.is_empty());
    let mut queued = BTreeMap::new();
    let mut passes = None;
    while let Ok(event) = rx.try_recv() {
        match event {
            ProgressEvent::QueueExtended { file, bytes, .. } => {
                queued.insert(file, bytes);
            }
            ProgressEvent::Par2EncodeStarted { passes: count, .. } => passes = Some(count),
            _ => {}
        }
    }
    assert_eq!(passes, Some(if multi_pass { 4 } else { 1 }));
    let base = queued["a.par2"];
    for volume in layout::plan_volumes(RECOVERY_COUNT) {
        let name = layout::volume_name("a", volume);
        let expected = base + u64::from(volume.count) * (SLICE_SIZE as u64 + 68);
        assert_eq!(queued[&name], expected, "incomplete queued volume {name}");
        let data = std::fs::read(outcome.par2_temp_dir.join(&name)).unwrap();
        assert_eq!(data.len() as u64, expected);
        assert_volume(&data, volume);
    }
    outcome.cleanup_par2_temp_dir().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_recovery_volumes_are_complete() {
    assert_queued_volumes(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn queued_recovery_volumes_are_complete_across_passes() {
    assert_queued_volumes(true).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn season_recovery_volumes_are_complete_on_return() {
    for multi_pass in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let paths = inputs(root.path());
        let output = root.path().join("season");
        generate_and_write_season_par2(
            &paths,
            "Season01",
            &output,
            &config(root.path(), multi_pass),
        )
        .await
        .unwrap();
        // Read synchronously, before another await can hide a pending write.
        for volume in layout::plan_volumes(RECOVERY_COUNT) {
            let data = std::fs::read(output.join(layout::volume_name("Season01", volume))).unwrap();
            assert_volume(&data, volume);
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn posted_yenc_contains_every_recovery_packet() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let articles = Arc::clone(&captured);
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (reader, mut writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        writer.write_all(b"200 mock ready\r\n").await.unwrap();
        let mut line = Vec::new();
        loop {
            line.clear();
            if reader.read_until(b'\n', &mut line).await.unwrap() == 0 {
                break;
            }
            if line.starts_with(b"POST") {
                writer.write_all(b"340 send article\r\n").await.unwrap();
                let mut article = Vec::new();
                loop {
                    line.clear();
                    assert_ne!(reader.read_until(b'\n', &mut line).await.unwrap(), 0);
                    if line == b".\r\n" {
                        break;
                    }
                    // Undo NNTP dot stuffing before decoding the yEnc body.
                    article.extend_from_slice(if line.starts_with(b"..") {
                        &line[1..]
                    } else {
                        &line
                    });
                }
                articles.lock().unwrap().push(article);
                writer.write_all(b"240 accepted\r\n").await.unwrap();
            } else if line.starts_with(b"QUIT") {
                writer.write_all(b"205 bye\r\n").await.unwrap();
                break;
            } else {
                assert!(
                    line.starts_with(b"MODE READER"),
                    "unexpected command {line:?}"
                );
                writer.write_all(b"200 reader mode\r\n").await.unwrap();
            }
        }
    });
    let root = tempfile::tempdir().unwrap();
    let paths = inputs(root.path());
    let mut config = config(root.path(), true);
    config.host = address.ip().to_string();
    config.port = address.port();
    config.dry_run = false;
    let outcome = post_files_with_progress(
        &config,
        &walk::expand_inputs(&paths).unwrap(),
        None,
        None,
        None,
    )
    .await
    .unwrap();
    assert!(!outcome.cancelled, "{:?}", outcome.failure_reason);
    assert!(outcome.failed_tasks.is_empty());
    tokio::time::timeout(std::time::Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    let mut decoded = BTreeMap::<String, Vec<yenc::DecodedPart>>::new();
    for article in captured.lock().unwrap().iter() {
        let part = yenc::decode_part(article).unwrap();
        assert!(part.crc_matches());
        decoded.entry(part.name.clone()).or_default().push(part);
    }
    for parts in decoded.values_mut() {
        parts.sort_by_key(|part| part.begin);
    }
    for volume in layout::plan_volumes(RECOVERY_COUNT) {
        let name = layout::volume_name("a", volume);
        let parts = &decoded[&name];
        let data: Vec<_> = parts
            .iter()
            .flat_map(|part| part.data.iter().copied())
            .collect();
        assert_eq!(data.len() as u64, parts[0].file_size);
        assert_volume(&data, volume);
        assert_eq!(
            data,
            std::fs::read(outcome.par2_temp_dir.join(name)).unwrap()
        );
    }
    for path in paths {
        let name = path.file_name().unwrap().to_str().unwrap();
        let data: Vec<_> = decoded[name]
            .iter()
            .flat_map(|part| part.data.iter().copied())
            .collect();
        assert_eq!(data, std::fs::read(path).unwrap());
    }
    outcome.cleanup_par2_temp_dir().await;
}
