//! Bounded partial MediaInfo inspection of NZB media and stored archives.
//!
//! No PAR2 repair, hooks, or full-download fallback. Reports are partial:
//! only container metadata visible in the sampled ranges can be returned.
mod archive;
mod crypto;
mod sevenz;

use anyhow::{bail, ensure, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::{
    io::{Seek, SeekFrom, Write},
    path::Path,
};

use crate::remote::{NntpSource, Remote, Source};
use crate::{
    config::Config,
    queue::{DownloadQueue, QueuedFile},
};
use archive::{Media, Span};

/// Limits and file selection for partial inspection.
#[derive(Clone)]
pub struct Options {
    /// Maximum transferred bytes, including failed transfers. Complete NNTP
    /// articles are consumed, so inaccurate NZB sizes can overshoot by one article.
    pub max_bytes: u64,
    /// Select an exact NZB filename. Otherwise prefer the largest non-sample media.
    pub file: Option<String>,
    /// Include the native English text report from the same sampled data.
    pub text: bool,
    /// Archive password, already resolved against the NZB metadata by the caller.
    pub password: Option<String>,
}
impl std::fmt::Debug for Options {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Options")
            .field("max_bytes", &self.max_bytes)
            .field("file", &self.file)
            .field("text", &self.text)
            .field("has_password", &self.password.is_some())
            .finish()
    }
}
impl Default for Options {
    fn default() -> Self {
        Self {
            max_bytes: 16 * 1024 * 1024,
            file: None,
            text: false,
            password: None,
        }
    }
}

/// MediaInfo JSON plus the cost and coverage of this partial inspection.
#[derive(Debug, Serialize)]
pub struct Report {
    pub file: String,
    pub file_size: u64,
    pub downloaded_bytes: u64,
    pub fetched_articles: usize,
    pub sampled_bytes: u64,
    pub partial: bool,
    pub mediainfo: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Inspect one media file from a release using the existing NNTP/yEnc APIs.
/// Requires the `mediainfo` CLI on PATH. Temporary sparse files are deleted
/// on success and failure; the normal download directory/cache is untouched.
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
    let result = inspect_with(&mut remote, options, &mut MediaInfo).await;
    remote.source.close().await;
    result
}

trait Probe {
    async fn inspect(&mut self, path: &Path) -> Result<Value>;
    async fn text(&mut self, path: &Path) -> Result<String>;
}
struct MediaInfo;
impl MediaInfo {
    async fn run(&self, path: &Path, json: bool) -> Result<Vec<u8>> {
        let mut command = tokio::process::Command::new("mediainfo");
        command.args([
            "--ParseSpeed=0",
            "--File_TestContinuousFileNames=0",
            "--Language=en",
        ]);
        if json {
            command.arg("--Output=JSON");
        }
        command.arg(path).kill_on_drop(true);
        let output = tokio::time::timeout(std::time::Duration::from_secs(20), command.output())
            .await
            .context("MediaInfo timed out while inspecting sampled data")?
            .context("cannot launch mediainfo; install the MediaInfo CLI and add it to PATH")?;
        ensure!(
            output.status.success(),
            "MediaInfo failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
        Ok(output.stdout)
    }
}
impl Probe for MediaInfo {
    async fn inspect(&mut self, path: &Path) -> Result<Value> {
        serde_json::from_slice(&self.run(path, true).await?)
            .context("MediaInfo returned invalid JSON")
    }
    async fn text(&mut self, path: &Path) -> Result<String> {
        let text = String::from_utf8(self.run(path, false).await?)
            .context("MediaInfo returned invalid UTF-8 text")?;
        ensure!(
            !text.trim().is_empty(),
            "MediaInfo returned an empty text report"
        );
        Ok(text)
    }
}

fn has_media(value: &Value) -> bool {
    value
        .pointer("/media/track")
        .and_then(Value::as_array)
        .is_some_and(|tracks| {
            tracks.iter().any(|t| {
                matches!(t["@type"].as_str(), Some("Video" | "Audio"))
                    && t["Format"].as_str().is_some_and(|f| !f.is_empty())
            })
        })
}

pub(super) fn is_media_name(name: &str) -> bool {
    matches!(
        name.rsplit('.')
            .next()
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "mkv"
            | "mp4"
            | "m4v"
            | "mov"
            | "avi"
            | "webm"
            | "ts"
            | "m2ts"
            | "mts"
            | "mpg"
            | "mpeg"
            | "vob"
            | "wmv"
            | "flv"
            | "mp3"
            | "flac"
            | "m4a"
            | "aac"
            | "ogg"
            | "opus"
            | "wav"
            | "wma"
            | "aiff"
            | "ac3"
            | "dts"
    )
}
fn rar_key(name: &str) -> Option<(String, u64)> {
    let name = name.to_ascii_lowercase();
    let (stem, extension) = name.rsplit_once('.')?;
    if extension == "rar" {
        if let Some((base, number)) = stem.rsplit_once(".part") {
            return Some((base.into(), number.parse().ok()?));
        }
        return Some((stem.into(), 0));
    }
    if let Some(number) = extension.strip_prefix('r') {
        return Some((stem.into(), number.parse::<u64>().ok()?.checked_add(1)?));
    }
    None
}
fn is_archive(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    rar_key(&name).is_some()
        || name.ends_with(".7z")
        || name.contains(".7z.")
        || name.ends_with(".zip")
}
fn candidate(files: &[QueuedFile], options: &Options) -> Result<usize> {
    if let Some(name) = &options.file {
        return files
            .iter()
            .position(|f| &f.name == name)
            .with_context(|| format!("NZB has no file named {name}"));
    }
    files
        .iter()
        .enumerate()
        .filter(|(_, f)| {
            let extension = f.name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
            !matches!(
                extension.as_str(),
                "par2"
                    | "nfo"
                    | "sfv"
                    | "srr"
                    | "nzb"
                    | "txt"
                    | "jpg"
                    | "jpeg"
                    | "png"
                    | "gif"
                    | "url"
                    | "rev"
            )
        })
        .max_by_key(|(_, f)| {
            (
                !f.name.to_ascii_lowercase().contains("sample"),
                is_media_name(&f.name),
                is_archive(&f.name),
                f.segments
                    .iter()
                    .map(|s| s.bytes)
                    .fold(0u64, u64::saturating_add),
            )
        })
        .map(|(index, _)| index)
        .context("NZB does not contain a supported media file or archive")
}

async fn locate<S: Source>(remote: &mut Remote<S>, options: &Options) -> Result<Media> {
    let mut file = candidate(&remote.files, options)?;
    if let Some((base, _)) = rar_key(&remote.files[file].name) {
        file = remote
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, f)| {
                let (other, number) = rar_key(&f.name)?;
                (other == base).then_some((number, index))
            })
            .min()
            .map(|(_, index)| index)
            .unwrap_or(file);
    }
    let size = remote.size(file).await?;
    let signature = remote.read(file, 0, size.min(8) as usize).await?;
    if signature.starts_with(b"Rar!\x1a\x07") {
        let key = rar_key(&remote.files[file].name);
        let mut volumes: Vec<(u64, usize)> = remote
            .files
            .iter()
            .enumerate()
            .filter_map(|(index, f)| {
                if let Some((base, _)) = &key {
                    let (other, number) = rar_key(&f.name)?;
                    (other == *base).then_some((number, index))
                } else {
                    (index == file).then_some((0, index))
                }
            })
            .collect();
        volumes.sort_unstable();
        for pair in volumes.windows(2) {
            ensure!(pair[1].0 == pair[0].0 + 1, "missing RAR archive volume");
        }
        return archive::rar(
            remote,
            &volumes.iter().map(|v| v.1).collect::<Vec<_>>(),
            true,
            options.password.as_deref(),
        )
        .await;
    }
    if signature.starts_with(b"7z\xbc\xaf\x27\x1c") {
        ensure!(
            !remote.files[file]
                .name
                .to_ascii_lowercase()
                .contains(".7z."),
            "split 7z archive cannot be inspected partially"
        );
        return sevenz::media(remote, file, options.password.as_deref()).await;
    }
    ensure!(
        !signature.starts_with(b"PK\x03\x04") && !is_archive(&remote.files[file].name),
        "compressed or unsupported archive; partial MediaInfo extraction is unavailable"
    );
    Ok(Media {
        name: remote.files[file].name.clone(),
        spans: vec![Span {
            file,
            offset: 0,
            size,
            crypto: None,
            plaintext_offset: 0,
            cbc_previous: None,
        }],
        expected_size: None,
        volumes: vec![],
        password: None,
    })
}

async fn inspect_with<S: Source, P: Probe>(
    remote: &mut Remote<S>,
    options: &Options,
    probe: &mut P,
) -> Result<Report> {
    let mut media = locate(remote, options).await?;
    let size = media.size();
    ensure!(
        size > 0 && size <= 1024 * 1024 * 1024 * 1024,
        "empty or excessive media file size"
    );
    let directory = tempfile::tempdir().context("creating temporary MediaInfo directory")?;
    let name = crate::queue::sanitize_file_name(&media.name);
    let path = directory.path().join(&name);
    let mut sparse = std::fs::File::create(&path)?;
    sparse.set_len(size)?;
    let mut head = 0;
    let mut tail = 0;
    // Grow only when MediaInfo cannot identify an audio/video stream. Always
    // try the head first: MKV/FLAC often need just one article. A tail sample
    // allows MP4/MOV with a trailing moov atom without downloading mdat.
    for target in [64 * 1024u64, 256 * 1024, 1024 * 1024, 4 * 1024 * 1024] {
        let target = target.min(size);
        if target > head {
            let bytes = media.read(remote, head, (target - head) as usize).await?;
            sparse.seek(SeekFrom::Start(head))?;
            sparse.write_all(&bytes)?;
            head = target;
        }
        for with_tail in [false, true] {
            if with_tail {
                let desired = target.min(size - head);
                if desired <= tail {
                    continue;
                }
                let bytes = media
                    .read(remote, size - desired, (desired - tail) as usize)
                    .await?;
                sparse.seek(SeekFrom::Start(size - desired))?;
                sparse.write_all(&bytes)?;
                tail = desired;
            }
            sparse.flush()?;
            let mut value = probe.inspect(&path).await?;
            if has_media(&value) {
                // Hide the temporary filesystem path from exported reports.
                if let Some(object) = value.get_mut("media").and_then(Value::as_object_mut) {
                    object.insert("@ref".into(), Value::String(media.name.clone()));
                    if let Some(tracks) = object.get_mut("track").and_then(Value::as_array_mut) {
                        for track in tracks {
                            if let Some(fields) = track.as_object_mut() {
                                if fields.contains_key("CompleteName") {
                                    fields.insert(
                                        "CompleteName".into(),
                                        Value::String(media.name.clone()),
                                    );
                                }
                            }
                        }
                    }
                }
                let text = if options.text {
                    let raw = probe.text(&path).await?;
                    Some(raw.replace(path.to_string_lossy().as_ref(), &media.name))
                } else {
                    None
                };
                let (downloaded_bytes, fetched_articles) = remote.source.stats();
                return Ok(Report {
                    file: media.name,
                    file_size: size,
                    downloaded_bytes,
                    fetched_articles,
                    sampled_bytes: (head + tail).min(size),
                    partial: head + tail < size,
                    mediainfo: value,
                    text,
                });
            }
        }
        if head + tail >= size {
            break;
        }
    }
    if media.spans.iter().any(|s| s.crypto.is_some()) {
        bail!("incorrect archive password, file is not recognized as audio/video, or its metadata is outside the partial sample; no full download was attempted");
    }
    bail!("file is not recognized as audio/video, or its metadata is outside the partial sample; no full download was attempted")
}

#[cfg(test)]
mod tests;
