//! Extract declared file hashes from a PAR2 in an NZB without fetching payload files.
use anyhow::{bail, ensure, Context, Result};
use pesto::par2::recovery_set::RecoverySet;
use serde::Serialize;

use crate::{
    config::Config,
    queue::DownloadQueue,
    remote::{NntpSource, Remote, Source},
};

/// Transfer limits and optional exact NZB filename selection.
#[derive(Debug, Clone)]
pub struct Options {
    pub max_bytes: u64,
    /// Select an obfuscated PAR2 by its NZB filename.
    pub file: Option<String>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            file: None,
        }
    }
}

/// Hashes describe the protected file, which may be an archive volume.
#[derive(Debug, Serialize)]
pub struct FileHash {
    pub name: String,
    pub size_bytes: u64,
    pub md5: String,
    pub md5_16k: String,
}

/// One complete recovery set's protected-file manifest, not a content verification.
#[derive(Debug, Serialize)]
pub struct Report {
    /// JSON schema version. Consumers should reject unsupported versions.
    pub schema_version: u32,
    pub par2_file: String,
    pub recovery_set_id: String,
    pub hash_source: &'static str,
    pub verification_status: &'static str,
    pub downloaded_bytes: u64,
    pub fetched_articles: usize,
    pub files: Vec<FileHash>,
}

/// Fetch only a PAR2, reusing NNTP failover and CRC-checked yEnc decoding.
/// Temporary metadata is removed on both success and failure. No hooks run.
pub async fn inspect(queue: &DownloadQueue, config: &Config, options: &Options) -> Result<Report> {
    ensure!(
        options.max_bytes > 0,
        "--max-bytes must be greater than zero"
    );
    ensure!(
        !config.server_tiers.is_empty(),
        "no news servers configured"
    );
    let source = NntpSource::new(config.clone(), options.max_bytes);
    let mut remote = Remote::new(queue.files.clone(), source);
    let result = inspect_with(&mut remote, options).await;
    remote.source.close().await;
    result
}

fn hex(hash: &[u8; 16]) -> String {
    hash.iter().map(|b| format!("{b:02x}")).collect()
}

fn manifest(bytes: &[u8], name: String) -> Result<Report> {
    // Reuse parmesan's packet checksum validation and recovery-set assembly.
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("index.par2");
    std::fs::write(&path, bytes)?;
    let set = RecoverySet::load_metadata(&path)?;
    ensure!(!set.files.is_empty(), "PAR2 has no protected files");
    Ok(Report {
        schema_version: 1,
        par2_file: name,
        recovery_set_id: hex(&set.recovery_set_id),
        hash_source: "par2",
        verification_status: "declared",
        downloaded_bytes: 0,
        fetched_articles: 0,
        files: set
            .files
            .into_iter()
            .map(|f| FileHash {
                name: f.name,
                size_bytes: f.length,
                md5: hex(&f.md5_full),
                md5_16k: hex(&f.md5_16k),
            })
            .collect(),
    })
}

async fn inspect_with<S: Source>(remote: &mut Remote<S>, options: &Options) -> Result<Report> {
    let mut candidates: Vec<usize> = if let Some(name) = &options.file {
        vec![remote
            .files
            .iter()
            .position(|f| &f.name == name)
            .with_context(|| format!("NZB has no file named {name}"))?]
    } else {
        remote
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| f.name.to_ascii_lowercase().ends_with(".par2"))
            .map(|(i, _)| i)
            .collect()
    };
    ensure!(
        !candidates.is_empty(),
        "NZB has no named PAR2 files; use --file for an obfuscated PAR2"
    );
    // Prefer index files, then the smallest recovery volumes. Only one complete
    // recovery set is reported; --file selects a different set explicitly.
    candidates.sort_by_key(|&i| {
        let file = &remote.files[i];
        (
            file.name.to_ascii_lowercase().contains(".vol"),
            file.segments
                .iter()
                .map(|s| s.bytes)
                .fold(0u64, u64::saturating_add),
        )
    });
    let mut errors = Vec::new();
    for file in candidates {
        let name = remote.files[file].name.clone();
        let result: Result<Report> = async {
            let size = remote.size(file).await?;
            ensure!(size <= options.max_bytes,
                "PAR2 exceeds download budget; increase --max-bytes or select a smaller PAR2 with --file");
            let length = usize::try_from(size).context("PAR2 is too large for this platform")?;
            let bytes = remote.read(file, 0, length).await?;
            tokio::task::spawn_blocking(move || manifest(&bytes, name))
                .await.context("PAR2 metadata task panicked")?
        }.await;
        match result {
            Ok(mut report) => {
                (report.downloaded_bytes, report.fetched_articles) = remote.source.stats();
                return Ok(report);
            }
            Err(error) => errors.push(format!("{}: {error:#}", remote.files[file].name)),
        }
    }
    bail!(
        "could not extract a complete PAR2 manifest: {}",
        errors.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{QueuedFile, QueuedSegment};
    use pesto::{par2::packet, yenc::DecodedPart};

    struct MockSource {
        data: Vec<Vec<u8>>,
        requests: Vec<String>,
        bytes: u64,
    }

    impl Source for MockSource {
        async fn fetch(&mut self, file: &QueuedFile, index: usize) -> Result<DecodedPart> {
            self.requests.push(file.name.clone());
            let f: usize = file.segments[index].message_id.parse()?;
            let bytes = &self.data[f];
            let start = index * 64;
            let end = (start + 64).min(bytes.len());
            self.bytes += (end - start) as u64;
            Ok(DecodedPart {
                name: file.name.clone(),
                line_len: 128,
                file_size: bytes.len() as u64,
                part: index as u32 + 1,
                total: file.segments.len() as u32,
                begin: start as u64 + 1,
                end: end as u64,
                data: bytes[start..end].to_vec(),
                part_crc32: None,
                file_crc32: None,
            })
        }
        fn stats(&self) -> (u64, usize) {
            (self.bytes, self.requests.len())
        }
    }

    fn remote(files: Vec<(&str, Vec<u8>)>) -> Remote<MockSource> {
        let queue = files
            .iter()
            .enumerate()
            .map(|(i, (name, data))| QueuedFile {
                name: name.to_string(),
                segments: (0..data.len().div_ceil(64))
                    .map(|part| QueuedSegment {
                        message_id: i.to_string(),
                        part: part as u32 + 1,
                        bytes: 64,
                    })
                    .collect(),
            })
            .collect();
        Remote::new(
            queue,
            MockSource {
                data: files.into_iter().map(|(_, bytes)| bytes).collect(),
                requests: Vec::new(),
                bytes: 0,
            },
        )
    }

    fn index(include_description: bool) -> Vec<u8> {
        let content = b"protected content";
        let hash = packet::md5(content);
        let id = packet::compute_file_id(&hash, content.len() as u64, "movie.mkv");
        let main = packet::main_body(64, &[id]);
        let set = packet::recovery_set_id(&main);
        let mut bytes = packet::serialize_packet(&set, &packet::TYPE_MAIN, &main);
        if include_description {
            bytes.extend(packet::serialize_packet(
                &set,
                &packet::TYPE_FILE_DESC,
                &packet::file_description_body(
                    &id,
                    &hash,
                    &hash,
                    content.len() as u64,
                    "movie.mkv",
                ),
            ));
        }
        bytes
    }

    #[tokio::test]
    async fn extracts_declared_hashes_without_fetching_payload_or_recovery_volume() {
        let mut r = remote(vec![
            ("movie.mkv", vec![0; 1024]),
            ("release.vol00+01.par2", vec![0; 1024]),
            ("release.par2", index(true)),
        ]);
        let report = inspect_with(&mut r, &Options::default()).await.unwrap();
        assert_eq!(report.files.len(), 1);
        assert_eq!(report.files[0].name, "movie.mkv");
        assert_eq!(report.files[0].size_bytes, 17);
        assert_eq!(report.files[0].md5, hex(&packet::md5(b"protected content")));
        assert_eq!(report.verification_status, "declared");
        assert!(r.source.requests.iter().all(|name| name == "release.par2"));
        assert_eq!(report.downloaded_bytes, index(true).len() as u64);
    }

    #[tokio::test]
    async fn rejects_missing_or_corrupt_descriptions_and_falls_back_to_volume() {
        let mut broken = index(true);
        *broken.last_mut().unwrap() ^= 1;
        assert!(manifest(&broken, "broken.par2".into()).is_err());
        assert!(manifest(&index(false), "incomplete.par2".into()).is_err());
        let mut r = remote(vec![
            ("release.par2", broken),
            ("release.vol00+01.par2", index(true)),
        ]);
        let report = inspect_with(&mut r, &Options::default()).await.unwrap();
        assert_eq!(report.par2_file, "release.vol00+01.par2");
    }

    #[tokio::test]
    async fn explicit_selection_supports_obfuscation_and_bounds_large_files() {
        let mut r = remote(vec![("abcdef", index(true))]);
        assert!(inspect_with(&mut r, &Options::default()).await.is_err());
        assert!(r.source.requests.is_empty());
        let options = Options {
            file: Some("abcdef".into()),
            ..Options::default()
        };
        assert!(inspect_with(&mut r, &options).await.is_ok());
        let mut r = remote(vec![("large.par2", vec![0; 1024])]);
        let options = Options {
            max_bytes: 128,
            file: None,
        };
        let error = inspect_with(&mut r, &options).await.unwrap_err();
        assert!(error.to_string().contains("budget"));
        assert_eq!(r.source.requests.len(), 1);
    }
}
