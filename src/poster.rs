//! Parallel posting: the orchestration that ties together file reading, yEnc
//! encoding, article assembly and the NNTP client.
//!
//! Files are read sequentially by a single producer task which computes PAR2
//! parity concurrently. The producer feeds segments to a pool of worker tasks
//! via a bounded channel; workers yEnc-encode and post them. If the required
//! PAR2 recovery data exceeds a memory limit, the producer will make multiple
//! read passes over the files.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::article::{default_subject, generate_message_id, obfuscated_name, Article};
use crate::config::{Config, ObfuscateMode};
use crate::nntp::Connection;
use crate::yenc;
use crate::par2::encoder::{FileHasher, RecoveryEncoder, slice_checksum};
use crate::par2::packet::{self, SliceChecksum};
use crate::par2::layout;

/// Maximum number of attempts to post a single segment before giving up.
const MAX_POST_ATTEMPTS: u32 = 3;
/// Pause between failed attempts.
const RETRY_BACKOFF: Duration = Duration::from_secs(1);
/// Maximum memory to use for PAR2 recovery slices (in bytes).
const MAX_PAR2_MEMORY: usize = 1_000_000_000; // 1 GB

/// A posted segment, retained for later `.nzb` generation.
#[derive(Debug, Clone)]
pub struct PostedSegment {
    pub file_name: String,
    pub subject_name: String,
    pub file_size: u64,
    pub part: u32,
    pub total: u32,
    pub message_id: String,
    pub bytes: u64,
}

/// The result of a posting run.
#[derive(Debug)]
pub struct PostOutcome {
    pub segments: Vec<PostedSegment>,
    pub failures: Vec<String>,
    pub cancelled: bool,
}

#[derive(Debug, Clone)]
struct FileMeta {
    path: PathBuf,
    real_name: String,
    subject_name: String,
    yenc_name: String,
    size: u64,
}

struct PostTask {
    meta: Arc<FileMeta>,
    part: u32,
    total: u32,
    offset: u64,
    data: Vec<u8>,
}

struct Progress {
    total_segments: AtomicU64,
    done_segments: AtomicU64,
    done_bytes: AtomicU64,
    start: Instant,
}

impl Progress {
    fn new(total_segments: u64) -> Self {
        Self {
            total_segments: AtomicU64::new(total_segments),
            done_segments: AtomicU64::new(0),
            done_bytes: AtomicU64::new(0),
            start: Instant::now(),
        }
    }
    fn segment_done(&self, raw_bytes: u64) {
        self.done_segments.fetch_add(1, Ordering::Relaxed);
        self.done_bytes.fetch_add(raw_bytes, Ordering::Relaxed);
    }
}

struct Shared {
    config: Config,
    domain: String,
    results: Mutex<Vec<PostedSegment>>,
    failures: Mutex<Vec<String>>,
    progress: Progress,
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
        let (subject_name, yenc_name) = match config.obfuscate {
            ObfuscateMode::None => (real_name.clone(), real_name.clone()),
            ObfuscateMode::Subject => (obfuscated_name(), real_name.clone()),
            ObfuscateMode::Full => {
                let obfuscated = obfuscated_name();
                (obfuscated.clone(), obfuscated)
            }
        };
        metas.push(Arc::new(FileMeta {
            path: path.clone(),
            real_name,
            subject_name,
            yenc_name,
            size: md.len(),
        }));
    }

    let mut initial_segments = 0;
    for meta in &metas {
        initial_segments += yenc::segments(meta.size, config.article_size).len() as u64;
    }

    let worker_count = if config.par2_only {
        0
    } else {
        config
            .connections
            .max(1)
            .min(initial_segments.max(1) as usize)
    };

    if config.par2_only {
        eprintln!(
            "PAR2 ONLY: generating parity for {} file(s) as {} segment(s) (no network, no .nzb)",
            metas.len(),
            initial_segments,
        );
    } else if config.dry_run {
        eprintln!(
            "DRY RUN: processing {} file(s) as {} segment(s) over {} thread(s) (no network)",
            metas.len(),
            initial_segments,
            worker_count,
        );
    } else {
        eprintln!(
            "posting {} file(s) as {} segment(s) over {} connection(s) to {}:{}",
            metas.len(),
            initial_segments,
            worker_count,
            config.host,
            config.port,
        );
    }

    let shared = Arc::new(Shared {
        config: config.clone(),
        domain: domain_from(&config.from),
        results: Mutex::new(Vec::new()),
        failures: Mutex::new(Vec::new()),
        progress: Progress::new(initial_segments),
        cancelled: AtomicBool::new(false),
    });

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
    let tx_opt = if worker_count > 0 {
        let (tx, rx) = tokio::sync::mpsc::channel(worker_count * 2);
        let rx = Arc::new(tokio::sync::Mutex::new(rx));
        for _ in 0..worker_count {
            handles.push(tokio::spawn(worker(shared.clone(), rx.clone())));
        }
        Some(tx)
    } else {
        None
    };

    // Producer runs in this thread
    if let Err(e) = producer(metas, tx_opt, shared.clone()).await {
        shared.cancelled.store(true, Ordering::Relaxed);
        eprintln!("producer error: {e:#}");
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

fn domain_from(from: &str) -> String {
    if let Some(rest) = from.rsplit('@').next().filter(|_| from.contains('@')) {
        let domain = rest.trim_end_matches('>').trim();
        if !domain.is_empty() {
            return domain.to_string();
        }
    }
    "pesto".to_string()
}

async fn producer(
    metas: Vec<Arc<FileMeta>>,
    tx_opt: Option<tokio::sync::mpsc::Sender<PostTask>>,
    shared: Arc<Shared>,
) -> Result<()> {
    let mut total_slices = 0;
    for meta in &metas {
        total_slices += yenc::segments(meta.size, shared.config.article_size).len();
    }

    let recovery_count = (total_slices * shared.config.par2 as usize) / 100;
    let slices_per_pass = (MAX_PAR2_MEMORY / shared.config.article_size).max(1);

    let mut passes = Vec::new();
    if recovery_count > 0 {
        let mut start = 0;
        while start < recovery_count {
            let count = (recovery_count - start).min(slices_per_pass);
            passes.push((start as u32, count));
            start += count;
        }
    } else {
        passes.push((0, 0));
    }

    let mut par2_files = Vec::new();
    let mut all_checksums: Vec<Vec<SliceChecksum>> = vec![Vec::new(); metas.len()];
    for _ in &metas {
        par2_files.push(FileHasher::new());
    }

    let mut par2_dir = None;
    let mut base_packets = Vec::new();
    let mut rsid = [0u8; 16];

    for (pass_idx, (exp_start, rec_count)) in passes.iter().copied().enumerate() {
        let mut encoder = if rec_count > 0 {
            Some(RecoveryEncoder::new(
                shared.config.article_size,
                total_slices,
                exp_start,
                rec_count,
            ))
        } else {
            None
        };

        for (file_idx, meta) in metas.iter().enumerate() {
            let segments = yenc::segments(meta.size, shared.config.article_size);
            let total_parts = segments.len() as u32;

            let mut file = File::open(&meta.path)
                .await
                .with_context(|| format!("opening `{}`", meta.path.display()))?;

            for (i, (offset, len)) in segments.into_iter().enumerate() {
                if shared.cancelled.load(Ordering::Relaxed) {
                    return Ok(());
                }

                let mut buf = vec![0u8; len];
                file.read_exact(&mut buf)
                    .await
                    .with_context(|| format!("reading `{}`", meta.path.display()))?;

                if pass_idx == 0 {
                    par2_files[file_idx].update(&buf);

                    let mut padded = buf.clone();
                    padded.resize(shared.config.article_size, 0);
                    all_checksums[file_idx].push(slice_checksum(&padded));

                    if let Some(enc) = &mut encoder {
                        tokio::task::block_in_place(|| enc.add_slice(&padded));
                    }

                    if let Some(tx) = &tx_opt {
                        if tx.send(PostTask {
                            meta: meta.clone(),
                            part: i as u32 + 1,
                            total: total_parts,
                            offset,
                            data: buf,
                        }).await.is_err() {
                            return Ok(()); // channel closed
                        }
                    } else {
                        shared.progress.segment_done(buf.len() as u64);
                    }
                } else if let Some(enc) = &mut encoder {
                    let mut padded = buf;
                    padded.resize(shared.config.article_size, 0);
                    tokio::task::block_in_place(|| enc.add_slice(&padded));
                }
            }
        }

        if let Some(enc) = encoder {
            let recovery_slices = enc.finish();

            if pass_idx == 0 {
                let mut file_ids = Vec::new();
                let mut hashes = Vec::new();

                for (idx, hasher) in par2_files.drain(..).enumerate() {
                    let fh = hasher.finish();
                    let fid = packet::compute_file_id(&fh.md5_16k, fh.length, &metas[idx].real_name);
                    file_ids.push(fid);
                    hashes.push(fh);
                }

                let main_b = packet::main_body(shared.config.article_size as u64, &file_ids);
                rsid = packet::recovery_set_id(&main_b);
                let pkt_main = packet::serialize_packet(&rsid, &packet::TYPE_MAIN, &main_b);
                let pkt_creator = packet::serialize_packet(&rsid, &packet::TYPE_CREATOR, &packet::creator_body("pesto"));

                base_packets.extend(pkt_main);
                base_packets.extend(pkt_creator);

                for (idx, fh) in hashes.iter().enumerate() {
                    let fid = &file_ids[idx];
                    let pkt_file_desc = packet::serialize_packet(&rsid, &packet::TYPE_FILE_DESC, &packet::file_description_body(fid, &fh.md5_full, &fh.md5_16k, fh.length, &metas[idx].real_name));
                    let pkt_ifsc = packet::serialize_packet(&rsid, &packet::TYPE_IFSC, &packet::ifsc_body(fid, &all_checksums[idx]));
                    base_packets.extend(pkt_file_desc);
                    base_packets.extend(pkt_ifsc);
                }

                if shared.config.par2_only {
                    par2_dir = Some(metas[0].path.parent().unwrap_or(std::path::Path::new("")).to_path_buf());
                } else {
                    par2_dir = Some(std::env::temp_dir().join(format!("pesto_par2_{}", std::process::id())));
                    tokio::fs::create_dir_all(par2_dir.as_ref().unwrap()).await?;
                }

                let index_name = layout::index_name(&metas[0].real_name);
                let index_path = par2_dir.as_ref().unwrap().join(&index_name);
                tokio::fs::write(&index_path, &base_packets).await?;
                if let Some(tx) = &tx_opt {
                    push_par2_file(&index_path, index_name, &shared, tx).await?;
                }
            }

            let volumes = layout::plan_volumes(recovery_count as u32);
            for slice in recovery_slices {
                let vol = volumes.iter().find(|v| slice.exponent >= v.first && slice.exponent < v.first + v.count).unwrap();
                let vol_name = layout::volume_name(&metas[0].real_name, *vol);
                let vol_path = par2_dir.as_ref().unwrap().join(&vol_name);

                let mut file = tokio::fs::OpenOptions::new().create(true).append(true).open(&vol_path).await?;

                if slice.exponent == vol.first {
                    file.write_all(&base_packets).await?;
                }

                let pkt = packet::serialize_packet(&rsid, &packet::TYPE_RECOVERY, &packet::recovery_body(slice.exponent, &slice.data));
                file.write_all(&pkt).await?;

                if slice.exponent == vol.first + vol.count - 1 {
                    if let Some(tx) = &tx_opt {
                        push_par2_file(&vol_path, vol_name, &shared, tx).await?;
                    }
                }
            }
        }
    }
    
    Ok(())
}

async fn push_par2_file(
    path: &PathBuf,
    real_name: String,
    shared: &Arc<Shared>,
    tx: &tokio::sync::mpsc::Sender<PostTask>,
) -> Result<()> {
    let size = tokio::fs::metadata(path).await?.len();
    let segments = yenc::segments(size, shared.config.article_size);
    let total = segments.len() as u32;

    shared.progress.total_segments.fetch_add(total as u64, Ordering::Relaxed);

    let (subject_name, yenc_name) = match shared.config.obfuscate {
        ObfuscateMode::None => (real_name.clone(), real_name.clone()),
        ObfuscateMode::Subject => (obfuscated_name(), real_name.clone()),
        ObfuscateMode::Full => {
            let obfuscated = obfuscated_name();
            (obfuscated.clone(), obfuscated)
        }
    };

    let meta = Arc::new(FileMeta {
        path: path.clone(),
        real_name,
        subject_name,
        yenc_name,
        size,
    });

    let mut file = tokio::fs::File::open(path).await?;
    for (i, (offset, len)) in segments.into_iter().enumerate() {
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf).await?;
        if tx.send(PostTask {
            meta: meta.clone(),
            part: i as u32 + 1,
            total,
            offset,
            data: buf,
        }).await.is_err() {
            break;
        }
    }
    Ok(())
}

async fn worker(shared: Arc<Shared>, rx: Arc<tokio::sync::Mutex<tokio::sync::mpsc::Receiver<PostTask>>>) {
    let mut conn: Option<Connection> = None;

    loop {
        if shared.cancelled.load(Ordering::Relaxed) {
            break;
        }
        let task = {
            let mut rx = rx.lock().await;
            match rx.recv().await {
                Some(t) => t,
                None => break,
            }
        };

        let encoded = yenc::encode_part(
            &task.meta.yenc_name,
            task.meta.size,
            yenc::PartSpec {
                number: task.part,
                total: task.total,
                offset: task.offset,
            },
            &task.data,
            yenc::DEFAULT_LINE_LENGTH,
            None,
        );
        let message_id = generate_message_id(&shared.domain);
        let article = Article {
            message_id: message_id.clone(),
            from: shared.config.from.clone(),
            newsgroups: shared.config.groups.clone(),
            subject: default_subject(&task.meta.subject_name, task.part, task.total),
        };
        let payload = article.serialize(&encoded.body);

        let mut posted = false;
        let mut last_err = String::from("unknown error");

        if shared.config.dry_run {
            posted = true;
        } else {
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
                        conn = None;
                        if attempt < MAX_POST_ATTEMPTS {
                            tokio::time::sleep(RETRY_BACKOFF).await;
                        }
                    }
                }
            }
        }

        if posted {
            shared.results.lock().unwrap().push(PostedSegment {
                file_name: task.meta.real_name.clone(),
                subject_name: task.meta.subject_name.clone(),
                file_size: task.meta.size,
                part: task.part,
                total: task.total,
                message_id,
                bytes: payload.len() as u64,
            });
        } else {
            record_failure(&shared, &task.meta, &task, &last_err);
        }
        shared.progress.segment_done(task.data.len() as u64);
    }

    if let Some(mut connection) = conn {
        connection.quit().await;
    }
}

async fn connect_and_auth(config: &Config) -> Result<Connection> {
    let mut conn = Connection::connect(&config.host, config.port, config.ssl).await?;
    if let Some(username) = &config.username {
        let password = config.password.as_deref().unwrap_or("");
        conn.authenticate(username, password).await?;
    }
    Ok(conn)
}

fn record_failure(shared: &Shared, meta: &FileMeta, task: &PostTask, error: &str) {
    shared.failures.lock().unwrap().push(format!(
        "{} part {}/{}: {error}",
        meta.real_name, task.part, task.total
    ));
}

async fn monitor(shared: Arc<Shared>, done: Arc<AtomicBool>) {
    loop {
        let total = shared.progress.total_segments.load(Ordering::Relaxed);
        let segments = shared.progress.done_segments.load(Ordering::Relaxed);
        let bytes = shared.progress.done_bytes.load(Ordering::Relaxed);
        let elapsed = shared.progress.start.elapsed().as_secs_f64().max(0.001);
        let rate = (bytes as f64 / elapsed) as u64;
        eprint!(
            "\rposting: {}/{} segments · {} · {}/s    ",
            segments,
            total,
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
