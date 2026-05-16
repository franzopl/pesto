//! Parallel posting: the orchestration that ties together file reading, yEnc
//! encoding, article assembly and the NNTP client.
//!
//! Files are split into segments; a fixed pool of workers — one NNTP
//! connection each — drains a shared work queue, encoding and posting segments
//! concurrently. Failed segments are retried on a fresh connection.

use std::io::SeekFrom;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::article::{default_subject, generate_message_id, Article};
use crate::config::Config;
use crate::nntp::Connection;
use crate::yenc;

/// Maximum number of attempts to post a single segment before giving up.
const MAX_POST_ATTEMPTS: u32 = 3;
/// Pause between failed attempts.
const RETRY_BACKOFF: Duration = Duration::from_secs(1);

/// A posted segment, retained for later `.nzb` generation (phase 4).
#[derive(Debug, Clone)]
pub struct PostedSegment {
    /// Real name of the source file on disk.
    pub file_name: String,
    /// Name the segment was posted under (random when obfuscation is on,
    /// otherwise equal to `file_name`).
    pub posting_name: String,
    /// Total size of the source file, in bytes.
    pub file_size: u64,
    /// 1-based part number.
    pub part: u32,
    /// Total number of parts for the file.
    pub total: u32,
    /// `Message-ID` the segment was posted under (with angle brackets).
    pub message_id: String,
    /// Size of the posted article, in bytes.
    pub bytes: u64,
}

/// The result of a posting run.
#[derive(Debug)]
pub struct PostOutcome {
    /// Segments posted successfully.
    pub segments: Vec<PostedSegment>,
    /// Human-readable description of each segment that could not be posted.
    pub failures: Vec<String>,
    /// True if posting stopped early because of a Ctrl-C interruption.
    pub cancelled: bool,
}

/// Metadata about one input file.
struct FileMeta {
    path: PathBuf,
    /// The file's real name on disk.
    real_name: String,
    /// The name used on the wire — random when obfuscation is on.
    posting_name: String,
    size: u64,
}

/// One unit of work: a single segment of a file.
#[derive(Debug, Clone, Copy)]
struct WorkItem {
    file_index: usize,
    part: u32,
    total: u32,
    offset: u64,
    len: usize,
}

/// Live progress counters, shared between the workers and the monitor task.
struct Progress {
    total_segments: u64,
    done_segments: AtomicU64,
    done_bytes: AtomicU64,
    start: Instant,
}

impl Progress {
    fn new(total_segments: u64) -> Self {
        Self {
            total_segments,
            done_segments: AtomicU64::new(0),
            done_bytes: AtomicU64::new(0),
            start: Instant::now(),
        }
    }

    /// Record that one segment finished, carrying `raw_bytes` of source data.
    fn segment_done(&self, raw_bytes: u64) {
        self.done_segments.fetch_add(1, Ordering::Relaxed);
        self.done_bytes.fetch_add(raw_bytes, Ordering::Relaxed);
    }
}

/// State shared by every worker and the monitor task.
struct Shared {
    config: Config,
    metas: Vec<FileMeta>,
    domain: String,
    queue: Mutex<Vec<WorkItem>>,
    results: Mutex<Vec<PostedSegment>>,
    failures: Mutex<Vec<String>>,
    progress: Progress,
    /// Set when a Ctrl-C interrupt asks workers to stop taking new segments.
    cancelled: AtomicBool,
}

/// Post every file in `files` to the groups configured in `config`.
pub async fn post_files(config: &Config, files: &[PathBuf]) -> Result<PostOutcome> {
    let mut metas = Vec::with_capacity(files.len());
    for path in files {
        let md = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("reading metadata of `{}`", path.display()))?;
        if !md.is_file() {
            bail!("`{}` is not a regular file", path.display());
        }
        let real_name = path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| anyhow!("invalid file name: `{}`", path.display()))?
            .to_string();
        let posting_name = if config.obfuscate {
            crate::article::obfuscated_name()
        } else {
            real_name.clone()
        };
        metas.push(FileMeta {
            path: path.clone(),
            real_name,
            posting_name,
            size: md.len(),
        });
    }

    let work = build_work_items(&metas, config.article_size);
    let total_segments = work.len() as u64;
    let worker_count = config
        .connections
        .max(1)
        .min(total_segments.max(1) as usize);

    eprintln!(
        "posting {} file(s) as {} segment(s) over {} connection(s) to {}:{}",
        metas.len(),
        total_segments,
        worker_count,
        config.host,
        config.port,
    );

    let shared = Arc::new(Shared {
        config: config.clone(),
        domain: domain_from(&config.from),
        metas,
        queue: Mutex::new(work),
        results: Mutex::new(Vec::new()),
        failures: Mutex::new(Vec::new()),
        progress: Progress::new(total_segments),
        cancelled: AtomicBool::new(false),
    });

    // Ask workers to stop taking new segments on the first Ctrl-C; in-flight
    // segments still finish so the resulting .nzb stays consistent.
    let cancel_handle = {
        let shared = shared.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                shared.cancelled.store(true, Ordering::Relaxed);
                eprintln!("\ninterrupt received — finishing in-flight segments...");
            }
        })
    };

    let done = Arc::new(AtomicBool::new(false));
    let monitor_handle = tokio::spawn(monitor(shared.clone(), done.clone()));

    let mut handles = Vec::with_capacity(worker_count);
    for _ in 0..worker_count {
        handles.push(tokio::spawn(worker(shared.clone())));
    }
    for handle in handles {
        let _ = handle.await;
    }

    done.store(true, Ordering::Relaxed);
    let _ = monitor_handle.await;
    cancel_handle.abort();

    let mut segments = std::mem::take(&mut *shared.results.lock().unwrap());
    segments.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.part.cmp(&b.part)));
    let failures = std::mem::take(&mut *shared.failures.lock().unwrap());
    let cancelled = shared.cancelled.load(Ordering::Relaxed);

    Ok(PostOutcome {
        segments,
        failures,
        cancelled,
    })
}

/// Split every file into segments and flatten them into a work list.
fn build_work_items(metas: &[FileMeta], article_size: usize) -> Vec<WorkItem> {
    let mut work = Vec::new();
    for (file_index, meta) in metas.iter().enumerate() {
        let segments = yenc::segments(meta.size, article_size);
        let total = segments.len() as u32;
        for (i, (offset, len)) in segments.into_iter().enumerate() {
            work.push(WorkItem {
                file_index,
                part: i as u32 + 1,
                total,
                offset,
                len,
            });
        }
    }
    work
}

/// Extract a domain for `Message-ID`s from the `From` header value.
fn domain_from(from: &str) -> String {
    if let Some(rest) = from.rsplit('@').next().filter(|_| from.contains('@')) {
        let domain = rest.trim_end_matches('>').trim();
        if !domain.is_empty() {
            return domain.to_string();
        }
    }
    "pesto".to_string()
}

/// A single worker: owns one NNTP connection and drains the shared queue.
async fn worker(shared: Arc<Shared>) {
    let mut conn: Option<Connection> = None;
    let mut open: Option<(usize, File)> = None;

    loop {
        if shared.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let item = match shared.queue.lock().unwrap().pop() {
            Some(item) => item,
            None => break,
        };
        let meta = &shared.metas[item.file_index];

        let data = match read_item(&mut open, meta, &item).await {
            Ok(data) => data,
            Err(e) => {
                record_failure(&shared, meta, &item, &format!("read error: {e:#}"));
                shared.progress.segment_done(0);
                continue;
            }
        };

        let encoded = yenc::encode_part(
            &meta.posting_name,
            meta.size,
            yenc::PartSpec {
                number: item.part,
                total: item.total,
                offset: item.offset,
            },
            &data,
            yenc::DEFAULT_LINE_LENGTH,
            None,
        );
        let message_id = generate_message_id(&shared.domain);
        let article = Article {
            message_id: message_id.clone(),
            from: shared.config.from.clone(),
            newsgroups: shared.config.groups.clone(),
            subject: default_subject(&meta.posting_name, item.part, item.total),
        };
        let payload = article.serialize(&encoded.body);

        let mut posted = false;
        let mut last_err = String::from("unknown error");
        for attempt in 1..=MAX_POST_ATTEMPTS {
            if conn.is_none() {
                match connect_and_auth(&shared.config).await {
                    Ok(c) => conn = Some(c),
                    Err(e) => {
                        last_err = format!("connect: {e:#}");
                        if attempt < MAX_POST_ATTEMPTS {
                            tokio::time::sleep(RETRY_BACKOFF).await;
                        }
                        continue;
                    }
                }
            }
            let connection = conn.as_mut().expect("connection established above");
            match connection.post(&payload).await {
                Ok(()) => {
                    posted = true;
                    break;
                }
                Err(e) => {
                    last_err = format!("{e:#}");
                    // Drop the connection so the next attempt reconnects.
                    conn = None;
                    if attempt < MAX_POST_ATTEMPTS {
                        tokio::time::sleep(RETRY_BACKOFF).await;
                    }
                }
            }
        }

        if posted {
            shared.results.lock().unwrap().push(PostedSegment {
                file_name: meta.real_name.clone(),
                posting_name: meta.posting_name.clone(),
                file_size: meta.size,
                part: item.part,
                total: item.total,
                message_id,
                bytes: payload.len() as u64,
            });
        } else {
            record_failure(&shared, meta, &item, &last_err);
        }
        shared.progress.segment_done(item.len as u64);
    }

    if let Some(mut connection) = conn {
        connection.quit().await;
    }
}

/// Open (or reuse) the source file and read the bytes for one work item.
async fn read_item(
    open: &mut Option<(usize, File)>,
    meta: &FileMeta,
    item: &WorkItem,
) -> Result<Vec<u8>> {
    if open.as_ref().map(|(i, _)| *i) != Some(item.file_index) {
        let file = File::open(&meta.path)
            .await
            .with_context(|| format!("opening `{}`", meta.path.display()))?;
        *open = Some((item.file_index, file));
    }
    let file = &mut open.as_mut().expect("file opened above").1;
    file.seek(SeekFrom::Start(item.offset))
        .await
        .with_context(|| format!("seeking in `{}`", meta.path.display()))?;
    let mut buf = vec![0u8; item.len];
    file.read_exact(&mut buf)
        .await
        .with_context(|| format!("reading `{}`", meta.path.display()))?;
    Ok(buf)
}

/// Open a connection and authenticate if credentials are configured.
async fn connect_and_auth(config: &Config) -> Result<Connection> {
    let mut conn = Connection::connect(&config.host, config.port, config.ssl).await?;
    if let Some(username) = &config.username {
        let password = config.password.as_deref().unwrap_or("");
        conn.authenticate(username, password).await?;
    }
    Ok(conn)
}

/// Record a failed segment in the shared failure list.
fn record_failure(shared: &Shared, meta: &FileMeta, item: &WorkItem, error: &str) {
    shared.failures.lock().unwrap().push(format!(
        "{} part {}/{}: {error}",
        meta.real_name, item.part, item.total
    ));
}

/// Periodically print progress to stderr until `done` is set.
async fn monitor(shared: Arc<Shared>, done: Arc<AtomicBool>) {
    loop {
        let segments = shared.progress.done_segments.load(Ordering::Relaxed);
        let bytes = shared.progress.done_bytes.load(Ordering::Relaxed);
        let elapsed = shared.progress.start.elapsed().as_secs_f64().max(0.001);
        let rate = (bytes as f64 / elapsed) as u64;
        eprint!(
            "\rposting: {}/{} segments · {} · {}/s    ",
            segments,
            shared.progress.total_segments,
            format_size(bytes),
            format_size(rate),
        );
        if done.load(Ordering::Relaxed) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    eprintln!();
}

/// Format a byte count with a binary unit suffix.
fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta(name: &str, size: u64) -> FileMeta {
        FileMeta {
            path: PathBuf::from(name),
            real_name: name.to_string(),
            posting_name: name.to_string(),
            size,
        }
    }

    #[test]
    fn work_items_cover_every_segment() {
        let metas = vec![meta("a.bin", 250), meta("b.bin", 100)];
        let work = build_work_items(&metas, 100);

        // a.bin -> 3 parts, b.bin -> 1 part.
        assert_eq!(work.len(), 4);
        let a: Vec<_> = work.iter().filter(|w| w.file_index == 0).collect();
        assert_eq!(a.len(), 3);
        assert!(a.iter().all(|w| w.total == 3));
        assert_eq!(a.iter().map(|w| w.part).collect::<Vec<_>>(), vec![1, 2, 3]);

        let b: Vec<_> = work.iter().filter(|w| w.file_index == 1).collect();
        assert_eq!(b.len(), 1);
        assert_eq!(b[0].total, 1);
    }

    #[test]
    fn domain_is_extracted_from_from_header() {
        assert_eq!(domain_from("poster <p@example.com>"), "example.com");
        assert_eq!(domain_from("a@b.net"), "b.net");
        assert_eq!(domain_from("no-at-sign"), "pesto");
        assert_eq!(domain_from(""), "pesto");
    }

    #[test]
    fn format_size_uses_binary_units() {
        assert_eq!(format_size(512), "512 B");
        assert_eq!(format_size(1024), "1.0 KiB");
        assert_eq!(format_size(1024 * 1024), "1.0 MiB");
    }
}
