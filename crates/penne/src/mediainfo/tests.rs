use super::*;
use crate::queue::QueuedSegment;
use pesto::yenc::DecodedPart;

struct MockSource {
    data: Vec<Vec<u8>>,
    requests: Vec<(String, usize)>,
    chunk: usize,
}
impl Source for MockSource {
    async fn fetch(&mut self, file: &QueuedFile, index: usize) -> Result<DecodedPart> {
        self.requests.push((file.name.clone(), index));
        let file_index = file.segments[index]
            .message_id
            .split(':')
            .next()
            .unwrap()
            .parse::<usize>()?;
        let data = &self.data[file_index];
        let begin = index * self.chunk;
        let end = (begin + self.chunk).min(data.len());
        Ok(DecodedPart {
            name: file.name.clone(),
            line_len: 128,
            file_size: data.len() as u64,
            part: index as u32 + 1,
            total: file.segments.len() as u32,
            begin: begin as u64 + 1,
            end: end as u64,
            data: data[begin..end].to_vec(),
            part_crc32: None,
            file_crc32: None,
        })
    }
    fn stats(&self) -> (u64, usize) {
        (
            (self.requests.len() * self.chunk) as u64,
            self.requests.len(),
        )
    }
}
fn remote(data: Vec<Vec<u8>>, names: &[&str], chunk: usize) -> Remote<MockSource> {
    let files = names
        .iter()
        .enumerate()
        .map(|(f, name)| QueuedFile {
            name: name.to_string(),
            segments: (0..data[f].len().div_ceil(chunk))
                .map(|i| QueuedSegment {
                    message_id: format!("{f}:{i}"),
                    part: i as u32 + 1,
                    bytes: chunk as u64,
                })
                .collect(),
        })
        .collect();
    Remote::new(
        files,
        MockSource {
            data,
            requests: vec![],
            chunk,
        },
    )
}
struct MockProbe {
    tail_required: bool,
}
impl Probe for MockProbe {
    async fn text(&mut self, path: &Path) -> Result<String> {
        Ok(format!("General\nComplete name                            : {}\nFormat                                   : Matroska\n\nVideo\nFormat                                   : AVC\n", path.display()))
    }
    async fn inspect(&mut self, path: &Path) -> Result<Value> {
        let mut file = std::fs::File::open(path)?;
        let mut head = [0; 4];
        use std::io::Read;
        file.read_exact(&mut head)?;
        file.seek(SeekFrom::End(-4))?;
        let mut tail = [0; 4];
        file.read_exact(&mut tail)?;
        if head == *b"TEST" && (!self.tail_required || tail == *b"TAIL") {
            Ok(
                serde_json::json!({"media":{"@ref":path.to_string_lossy(),"track":[
                {"@type":"General","CompleteName":path.to_string_lossy()},
                {"@type":"Video","Format":"AVC"}]}}),
            )
        } else {
            Ok(serde_json::json!({"media":{"track":[]}}))
        }
    }
}
#[tokio::test]
async fn head_only_stops_early_and_hides_temporary_path() {
    let mut data = vec![42; 1024 * 1024];
    data[..4].copy_from_slice(b"TEST");
    let mut r = remote(vec![data], &["movie.mkv"], 64 * 1024);
    let report = inspect_with(
        &mut r,
        &Options::default(),
        &mut MockProbe {
            tail_required: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.fetched_articles, 1);
    assert_eq!(report.sampled_bytes, 64 * 1024);
    assert!(report.partial);
    assert_eq!(report.mediainfo["media"]["@ref"], "movie.mkv");
    assert_eq!(
        report.mediainfo["media"]["track"][0]["CompleteName"],
        "movie.mkv"
    );
}
#[tokio::test]
async fn tail_metadata_skips_middle_payload() {
    let mut data = vec![42; 1024 * 1024];
    data[..4].copy_from_slice(b"TEST");
    let len = data.len();
    data[len - 4..].copy_from_slice(b"TAIL");
    let mut r = remote(vec![data], &["movie.mp4"], 64 * 1024);
    let report = inspect_with(
        &mut r,
        &Options::default(),
        &mut MockProbe {
            tail_required: true,
        },
    )
    .await
    .unwrap();
    assert!(report.fetched_articles < 8);
    assert_eq!(report.sampled_bytes, 128 * 1024);
    assert!(report.partial);
}
#[tokio::test]
async fn non_media_fails_without_false_success() {
    let mut r = remote(vec![vec![42; 100]], &["fake.mkv"], 32);
    let err = inspect_with(
        &mut r,
        &Options::default(),
        &mut MockProbe {
            tail_required: false,
        },
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("not recognized as audio/video"));
    assert!(!has_media(
        &serde_json::json!({"media":{"track":[{"@type":"General","Format":"RAR"}]}})
    ));
}
#[tokio::test]
async fn byte_ranges_use_yenc_offsets_and_cache() {
    let bytes: Vec<_> = (0..200).collect();
    let mut r = remote(vec![bytes.clone()], &["data.bin"], 17);
    assert_eq!(r.read(0, 103, 50).await.unwrap(), bytes[103..153]);
    let before = r.source.requests.len();
    assert_eq!(r.read(0, 110, 10).await.unwrap(), bytes[110..120]);
    assert_eq!(r.source.requests.len(), before);
    assert!(r.read(0, 199, 2).await.is_err());
}
fn rar4_member(data: &[u8], before: bool, after: bool, compressed: bool) -> Vec<u8> {
    let name = b"movie.mkv";
    let length = 32 + name.len();
    let flags: u16 = 0x8000 | u16::from(before) | (u16::from(after) << 1);
    let mut h = vec![0, 0, 0x74];
    h.extend(flags.to_le_bytes());
    h.extend((length as u16).to_le_bytes());
    h.extend((data.len() as u32).to_le_bytes());
    h.extend((data.len() as u32).to_le_bytes());
    h.extend([0; 10]);
    h.push(if compressed { 0x33 } else { 0x30 });
    h.extend((name.len() as u16).to_le_bytes());
    h.extend([0; 4]);
    h.extend(name);
    h.extend(data);
    let mut archive = b"Rar!\x1a\x07\x00".to_vec();
    archive.extend(h);
    archive
}
#[tokio::test]
async fn stored_rar_maps_multivolume_media_and_rejects_compression() {
    let mut first = rar4_member(b"first", false, true, false);
    first[18..22].copy_from_slice(&9u32.to_le_bytes());
    let mut r = remote(
        vec![first, rar4_member(b"last", true, false, false)],
        &["movie.part01.rar", "movie.part02.rar"],
        16,
    );
    let mut media = locate(&mut r, &Options::default()).await.unwrap();
    assert_eq!(media.size(), 9);
    assert_eq!(media.read(&mut r, 3, 5).await.unwrap(), b"stlas");
    let mut r = remote(
        vec![rar4_member(b"data", false, false, true)],
        &["movie.rar"],
        16,
    );
    assert!(locate(&mut r, &Options::default())
        .await
        .unwrap_err()
        .to_string()
        .contains("compressed"));
}
#[test]
fn selection_prefers_main_media_and_groups_only_its_archive() {
    let r = remote(
        vec![vec![0; 100], vec![0; 10], vec![0; 200]],
        &["sample.mkv", "movie.mkv", "recovery.par2"],
        10,
    );
    assert_eq!(candidate(&r.files, &Options::default()).unwrap(), 1);
    let r = remote(
        vec![vec![0; 100], vec![0; 20]],
        &["sample.mkv", "movie.rar"],
        10,
    );
    assert_eq!(candidate(&r.files, &Options::default()).unwrap(), 1);
    let r = remote(
        vec![vec![0; 100], vec![0; 200]],
        &["obfuscated_hash", "recovery.par2"],
        10,
    );
    assert_eq!(candidate(&r.files, &Options::default()).unwrap(), 0);
    assert_eq!(rar_key("Movie.PART010.RAR"), Some(("movie".into(), 10)));
    assert_eq!(rar_key("movie.r00"), Some(("movie".into(), 1)));
    assert!(rar_key("movie.rev").is_none());
}

async fn mock_nntp(
    body: Option<Vec<u8>>,
) -> (pesto::config::ServerEntry, tokio::task::JoinHandle<()>) {
    use tokio::{
        io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
        net::TcpListener,
    };
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let (read, mut write) = stream.into_split();
        let mut read = BufReader::new(read);
        write.write_all(b"200 mock ready\r\n").await.unwrap();
        let mut line = String::new();
        loop {
            line.clear();
            if read.read_line(&mut line).await.unwrap() == 0 {
                break;
            }
            if line.starts_with("BODY ") {
                if let Some(body) = &body {
                    write.write_all(b"222 0 <mock> body\r\n").await.unwrap();
                    for line in body.split_inclusive(|b| *b == b'\n') {
                        if line.starts_with(b".") {
                            write.write_all(b".").await.unwrap();
                        }
                        write.write_all(line).await.unwrap();
                    }
                    write.write_all(b".\r\n").await.unwrap();
                } else {
                    write.write_all(b"430 missing\r\n").await.unwrap();
                }
            } else if line.starts_with("QUIT") {
                write.write_all(b"205 bye\r\n").await.unwrap();
                break;
            }
        }
    });
    (
        pesto::config::ServerEntry {
            host: address.ip().to_string(),
            port: address.port(),
            ssl: false,
            connections: 1,
            username: None,
            password: None,
            retry_delay: 0,
            timeout: 5,
            proxy: None,
        },
        task,
    )
}
#[tokio::test]
async fn nntp_missing_article_fails_over_and_budget_blocks_new_transfers() {
    let body = pesto::yenc::encode_part(
        "movie.mkv",
        4,
        pesto::yenc::PartSpec {
            number: 1,
            total: 1,
            offset: 0,
        },
        b"TEST",
        128,
        None,
    )
    .body;
    let (primary, primary_task) = mock_nntp(None).await;
    let (backup, backup_task) = mock_nntp(Some(body)).await;
    let config = crate::config::RawConfig::parse(&format!(
        "[[servers]]\nhost='{}'\nport={}\nssl=false\nretry_delay=0\n\n[[servers]]\nhost='{}'\nport={}\nssl=false\nretry_delay=0\n",
        primary.host,primary.port,backup.host,backup.port)).unwrap().resolve().unwrap();
    let r = remote(vec![b"TEST".to_vec()], &["movie.mkv"], 4);
    let mut source = NntpSource::new(config.clone(), 4096);
    let part = source.fetch(&r.files[0], 0).await.unwrap();
    assert_eq!(part.data, b"TEST");
    assert_eq!(source.stats().1, 2);
    assert!(source.stats().0 > 100);
    source.close().await;
    primary_task.await.unwrap();
    backup_task.await.unwrap();
    let mut limited = NntpSource::new(config, 1);
    assert!(limited
        .fetch(&r.files[0], 0)
        .await
        .unwrap_err()
        .to_string()
        .contains("budget exhausted"));
    assert_eq!(limited.stats(), (0, 0));
}

#[tokio::test]
async fn native_text_uses_the_same_sample_and_hides_temporary_path() {
    let mut data = vec![42; 1024 * 1024];
    data[..4].copy_from_slice(b"TEST");
    let mut r = remote(vec![data], &["movie.mkv"], 64 * 1024);
    let report = inspect_with(
        &mut r,
        &Options {
            text: true,
            ..Options::default()
        },
        &mut MockProbe {
            tail_required: false,
        },
    )
    .await
    .unwrap();
    assert_eq!(report.fetched_articles, 1);
    let text = report.text.unwrap();
    assert!(text.starts_with("General\n"));
    assert!(text.contains("Complete name                            : movie.mkv\n"));
    assert!(text.contains("\nVideo\n"));
    assert!(!text.contains("/tmp/"));
}

fn protected_options() -> Options {
    Options {
        password: Some("fixture".into()),
        ..Options::default()
    }
}
fn fixture_remote(data: &[u8], name: &str) -> Remote<MockSource> {
    remote(vec![data.to_vec()], &[name], 256)
}
#[tokio::test]
async fn encrypted_rar5_and_7z_read_head_tail_and_unaligned_ranges() {
    let fixtures: &[(&[u8], &str)] = &[
        (
            include_bytes!("../../tests/fixtures/mediainfo/rar4-data.rar"),
            "data.rar",
        ),
        (
            include_bytes!("../../tests/fixtures/mediainfo/rar4-headers.rar"),
            "headers.rar",
        ),
        (
            include_bytes!("../../tests/fixtures/mediainfo/rar5-data.rar"),
            "data.rar",
        ),
        (
            include_bytes!("../../tests/fixtures/mediainfo/rar5-headers.rar"),
            "headers.rar",
        ),
        (
            include_bytes!("../../tests/fixtures/mediainfo/7z-data.7z"),
            "data.7z",
        ),
        (
            include_bytes!("../../tests/fixtures/mediainfo/7z-headers.7z"),
            "headers.7z",
        ),
    ];
    for &(data, name) in fixtures {
        let mut r = fixture_remote(data, name);
        let mut media = locate(&mut r, &protected_options()).await.unwrap();
        assert_eq!(media.name, "movie.mkv");
        assert_eq!(media.size(), 16391);
        assert_eq!(media.read(&mut r, 0, 4).await.unwrap(), b"TEST");
        assert_eq!(
            media.read(&mut r, 107, 20).await.unwrap(),
            (103..123u8).collect::<Vec<_>>()
        );
        assert_eq!(
            media.read(&mut r, media.size() - 3, 3).await.unwrap(),
            b"END"
        );
        assert!(
            r.source.requests.len() * 256 < data.len() / 2,
            "{name} downloaded too much"
        );
        assert!(!format!("{media:?}").contains("fixture"));
    }
}
#[tokio::test]
async fn encrypted_header_password_errors_and_compressed_payload_rejection() {
    for (data, name) in [
        (
            include_bytes!("../../tests/fixtures/mediainfo/rar4-headers.rar").as_slice(),
            "headers.rar",
        ),
        (
            include_bytes!("../../tests/fixtures/mediainfo/rar5-headers.rar").as_slice(),
            "headers.rar",
        ),
        (
            include_bytes!("../../tests/fixtures/mediainfo/7z-headers.7z").as_slice(),
            "headers.7z",
        ),
    ] {
        let mut r = fixture_remote(data, name);
        assert!(locate(&mut r, &Options::default())
            .await
            .unwrap_err()
            .to_string()
            .contains("password"));
        let mut r = fixture_remote(data, name);
        let error = locate(
            &mut r,
            &Options {
                password: Some("wrong-password".into()),
                ..Options::default()
            },
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("password"));
        assert!(!format!("{error:#}").contains("wrong-password"));
    }
    let mut r = fixture_remote(
        include_bytes!("../../tests/fixtures/mediainfo/7z-compressed.7z"),
        "compressed.7z",
    );
    assert!(locate(&mut r, &protected_options())
        .await
        .unwrap_err()
        .to_string()
        .contains("compressed"));
}
#[tokio::test]
async fn encrypted_rar5_continuations_keep_cbc_state_and_trim_padding() {
    let data = vec![
        include_bytes!("../../tests/fixtures/mediainfo/rar5-split.part1.rar").to_vec(),
        include_bytes!("../../tests/fixtures/mediainfo/rar5-split.part2.rar").to_vec(),
        include_bytes!("../../tests/fixtures/mediainfo/rar5-split.part3.rar").to_vec(),
        include_bytes!("../../tests/fixtures/mediainfo/rar5-split.part4.rar").to_vec(),
        include_bytes!("../../tests/fixtures/mediainfo/rar5-split.part5.rar").to_vec(),
    ];
    let mut r = remote(
        data,
        &[
            "movie.part1.rar",
            "movie.part2.rar",
            "movie.part3.rar",
            "movie.part4.rar",
            "movie.part5.rar",
        ],
        256,
    );
    let mut media = locate(&mut r, &protected_options()).await.unwrap();
    assert_eq!(media.size(), 16391);
    assert_eq!(media.read(&mut r, 0, 4).await.unwrap(), b"TEST");
    assert!(r
        .source
        .requests
        .iter()
        .all(|(name, _)| name == "movie.part1.rar"));
    // The last volume needs the preceding ciphertext block, not its original IV.
    assert_eq!(media.read(&mut r, 16388, 3).await.unwrap(), b"END");
    let expected: Vec<u8> = (3970..4040).map(|i| ((i - 4) % 256) as u8).collect();
    assert_eq!(media.read(&mut r, 3970, 70).await.unwrap(), expected);
}

#[tokio::test]
async fn rar4_long_password_preserves_legacy_sha1_schedule() {
    let mut r = fixture_remote(
        include_bytes!("../../tests/fixtures/mediainfo/rar4-long-password.rar"),
        "long.rar",
    );
    let options = Options {
        password: Some("0123456789abcdef0123456789abcdef".into()),
        ..Options::default()
    };
    let mut media = locate(&mut r, &options).await.unwrap();
    assert_eq!(media.read(&mut r, 0, 4).await.unwrap(), b"TEST");
    assert_eq!(media.read(&mut r, 16388, 3).await.unwrap(), b"END");
}
