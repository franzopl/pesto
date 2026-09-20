//! Commit, failure and end-of-run retry policy for a posting run.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tracing::warn;

use crate::article::{default_subject, Article};
use crate::config::Config;
use crate::nntp::pool::ConnectionSlot;
use crate::progress::{ProgressEvent, ProgressSender};
use crate::resume::SegmentRecord;
use crate::yenc;

use super::outcome::{FailedTask, PostedSegment};
use super::persisted_identity;
use super::shared::Shared;
use super::task::PostTask;
use super::FileMeta;

/// Persist a successfully posted segment or record a failure, then emit the
/// corresponding progress event and release the article buffer back to the pool.
#[allow(clippy::too_many_arguments)]
pub(super) fn commit_result(
    shared: &Shared,
    task: PostTask,
    message_id: String,
    wire_bytes: usize,
    posted: bool,
    last_err: &str,
    date: (Option<String>, Option<u64>),
    server_idx: usize,
) {
    if posted {
        if let Some(resume) = &shared.resume {
            // In-memory only — no disk write here. Every commit used to
            // rewrite the entire state file while holding this lock, which
            // serialized all workers through one lock and turned state
            // tracking into an O(n^2) hot-path cost on large uploads. Now
            // that resume state is tracked unconditionally (not just when
            // --resume is passed), persisting had to move off this path
            // regardless — the whole point of resume is to survive the *end*
            // of a run being incomplete, not every individual segment, so a
            // single persist decided by the final outcome (see the
            // still_missing handling and `run_single_upload`'s cleanup)
            // covers the same guarantee at a fraction of the cost.
            //
            // `confirmed` is false until STAT 223; `--no-check` never flips
            // it (that run never STATed).
            resume.lock().unwrap().record_with(
                &task.meta.real_name,
                task.part,
                SegmentRecord {
                    message_id: message_id.clone(),
                    bytes: wire_bytes as u64,
                    confirmed: false,
                    check_disabled: !shared.config.check,
                    server_idx,
                    wire_identity: Some(persisted_identity(
                        &task.subject_name,
                        &task.yenc_name,
                        &task.from,
                        &date,
                    )),
                },
            );
        }
        // Confirmed posted — any spooled copy has served its purpose.
        if let Some(dir) = &shared.spool_dir {
            crate::spool::remove(dir, &task.meta.real_name, task.part);
        }
        let seg = PostedSegment {
            file_name: task.meta.real_name.clone(),
            file_path: Arc::from(task.meta.path.as_path()),
            // NZB uses the real filename for proper client-side renaming.
            subject_name: Arc::from(task.meta.client_path.as_str()),
            wire_name: Arc::from(task.subject_name.as_str()),
            wire_yenc_name: Arc::from(task.yenc_name.as_str()),
            file_size: task.meta.size,
            part: task.part,
            total: task.total,
            message_id,
            bytes: wire_bytes as u64,
            from: Arc::from(task.from.as_str()),
            date,
            full_crc32: task.file_crc32.unwrap_or(0),
            server_idx,
            file_index: task.meta.file_index,
            total_files: shared.total_files,
        };
        shared.results.lock().unwrap().push(seg.clone());
        if let Some(tx) = shared.check_tx.lock().unwrap().as_ref() {
            let _ = tx.send(seg);
        }
    } else {
        record_failure(shared, &task.meta, &task, message_id, last_err);
    }
    let article_bytes = task.data.len() as u64;
    shared.release_buffer(task.data);
    shared.emit(ProgressEvent::SegmentDone {
        file: task.meta.real_name.clone(),
        bytes: article_bytes,
        ok: posted,
    });
}

/// Add ±50 % jitter to `base` to prevent synchronized reconnect bursts.
///
/// Uses `slot_id` mixed with the current nanosecond timestamp as a cheap
/// pseudo-random seed — no external crate required.
pub(super) fn jittered(base: Duration, slot_id: usize) -> Duration {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    // 0..=999 range → [1.0, 1.5) multiplier
    let noise = (ns.wrapping_add(slot_id as u64 * 2_654_435_761) % 1000) as u32;
    let extra_ms = (base.as_millis() as u64 * noise as u64 / 2000) as u32;
    base + Duration::from_millis(extra_ms as u64)
}

/// Whether an automatic final recovery pass (see `check::recover_missing`)
/// is worth attempting for `missing` still-unconfirmed articles out of
/// `total` posted this run. Gated by *both* an absolute cap
/// (`check_recover_max`) and a percentage of the release
/// (`check_recover_percent`) — whichever is smaller wins, so behaviour
/// scales sanely from a small release (where even a large fraction missing
/// is only a handful of articles) to a huge one (where 15% could still be
/// thousands of articles, no longer "cheap" to retry automatically).
pub(super) fn is_cheap_to_recover(missing: usize, total: usize, config: &Config) -> bool {
    if missing == 0 || config.check_recover_max == 0 {
        return false;
    }
    if missing > config.check_recover_max {
        return false;
    }
    let percent_cap = (total as f64 * config.check_recover_percent as f64 / 100.0).ceil() as usize;
    missing <= percent_cap.max(1)
}

/// Build the "→ ..." label shown in the live panel header and the `Started`
/// progress event's `target` field. Lists every configured server (not just
/// the primary) so a multi-server run doesn't look single-server for its
/// entire duration — see the call site's comment for why this is knowable
/// up front, unlike `groups`.
pub(super) fn target_label(
    servers: &[crate::config::ServerEntry],
    total_connections: usize,
) -> String {
    match servers {
        [] => String::new(),
        [only] => format!("{}:{}", only.host, only.port),
        _ if servers.len() <= 3 => servers
            .iter()
            .map(|s| s.host.as_str())
            .collect::<Vec<_>>()
            .join(" + "),
        _ => format!("{} servers ({total_connections} conn)", servers.len()),
    }
}

pub(super) fn record_failure(
    shared: &Shared,
    meta: &FileMeta,
    task: &PostTask,
    message_id: String,
    error: &str,
) {
    let description = format!(
        "{} part {}/{}: {error}",
        meta.real_name, task.part, task.total
    );
    shared.emit(ProgressEvent::PostRetryQueued);
    shared.emit(ProgressEvent::Failed {
        description: description.clone(),
    });
    shared.failures.lock().unwrap().push(description);
    shared.failed_tasks.lock().unwrap().push(FailedTask {
        file_name: meta.real_name.clone(),
        client_path: meta.client_path.clone(),
        file_path: meta.path.clone(),
        message_id,
        subject_name: task.subject_name.clone(),
        yenc_name: task.yenc_name.clone(),
        file_size: meta.size,
        part: task.part,
        total: task.total,
        from: task.from.clone(),
        date: task.date.clone(),
        full_crc32: task.file_crc32.unwrap_or(0),
        file_index: meta.file_index,
        total_files: shared.total_files,
    });
}

/// Post a fresh copy of each segment in `failed`, re-posting under the
/// *same* `Message-ID` the in-run attempt used (see the comment on
/// `message_id` below for why). Returns the `PostedSegment`s that were
/// successfully posted; tasks that exhaust all retries are silently dropped
/// (the caller can compare lengths to detect persistent failures).
pub async fn repost_failed_tasks(
    config: &Config,
    failed: &[FailedTask],
    groups: &[String],
    events: Option<&ProgressSender>,
    cancel: Option<&Arc<AtomicBool>>,
    slots: &mut [ConnectionSlot],
) -> Result<Vec<PostedSegment>> {
    if failed.is_empty() {
        return Ok(Vec::new());
    }

    // Never `ConnectionSlot::new` — extra TCP would exceed a budget already
    // held by this episode. No slot means nothing to retry on.
    let Some(slot) = slots.first_mut() else {
        return Ok(Vec::new());
    };

    let article_size = config.article_size as u64;
    let max_retries = config.retries.max(1);
    let mut recovered: Vec<PostedSegment> = Vec::new();

    for (i, task) in failed.iter().enumerate() {
        if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
            break;
        }
        let offset = (task.part as u64 - 1) * article_size;
        let read_len = (task.file_size - offset).min(article_size) as usize;

        // Re-read from the preserved absolute path, not `file_name` (which is
        // only the published/relative name and would resolve against the CWD).
        let path = task.file_path.clone();
        let mut file = match File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                warn!(file = %task.file_name, path = %path.display(), "retry: cannot open file: {e}");
                continue;
            }
        };

        use tokio::io::AsyncSeekExt;
        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
            warn!(file = %task.file_name, offset, "retry: seek failed: {e}");
            continue;
        }

        let mut buf = vec![0u8; read_len];
        if let Err(e) = file.read_exact(&mut buf).await {
            warn!(file = %task.file_name, "retry: read failed: {e}");
            continue;
        }

        let spec = yenc::PartSpec {
            number: task.part,
            total: task.total,
            offset,
        };
        let file_crc32 = (task.part == task.total).then_some(task.full_crc32);
        let encoded = yenc::encode_part(
            &task.yenc_name,
            task.file_size,
            spec,
            &buf,
            config.line_length,
            file_crc32,
        );
        // Re-post with the *same* Message-ID the in-run attempts used, so a
        // server that already has the article (lost `240` ack) deduplicates it
        // via `435 Already exists` instead of accepting a duplicate under a
        // fresh ID. See [`FailedTask::message_id`].
        let mut message_id = task.message_id.clone();
        let (rfc_date, _ts) = &task.date;
        let article = Article {
            message_id: message_id.clone(),
            from: task.from.clone(),
            newsgroups: groups.to_vec(),
            subject: default_subject(
                &task.subject_name,
                task.part,
                task.total,
                (task.total_files > 0).then_some((task.file_index, task.total_files)),
            ),
            date: rfc_date.clone(),
            no_archive: config.no_archive,
        };
        let headers = article.build_headers();
        let wire_bytes = (headers.len() + encoded.body.len()) as u64;

        let mut ok = false;
        for attempt in 1..=max_retries {
            match slot.ensure_connected().await {
                Ok(conn) => match conn.post_parts(&headers, &encoded.body).await {
                    Ok(returned_id) => {
                        // See the main post path for why: some servers
                        // substitute their own Message-ID at accept time.
                        if let Some(server_id) = returned_id {
                            if server_id != message_id {
                                warn!(
                                    sent = %message_id,
                                    returned = %server_id,
                                    "server returned a different Message-ID than sent; adopting it"
                                );
                                message_id = server_id;
                            }
                        }
                        ok = true;
                        break;
                    }
                    Err(e) => {
                        slot.invalidate("post_err");
                        warn!(file = %task.file_name, part = task.part, attempt, "retry attempt failed: {e}");
                        if attempt < max_retries {
                            if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
                                break;
                            }
                            tokio::time::sleep(Duration::from_secs(config.retry_delay)).await;
                        }
                    }
                },
                Err(e) => {
                    warn!(attempt, "retry: connect failed: {e}");
                    if attempt < max_retries {
                        if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(config.retry_delay)).await;
                    }
                }
            }
        }

        if ok {
            recovered.push(PostedSegment {
                file_name: task.file_name.clone(),
                file_path: Arc::from(task.file_path.as_path()),
                // NZB uses the real filename, not obfuscated wire subject.
                subject_name: Arc::from(task.client_path.as_str()),
                wire_name: Arc::from(task.subject_name.as_str()),
                wire_yenc_name: Arc::from(task.yenc_name.as_str()),
                file_size: task.file_size,
                part: task.part,
                total: task.total,
                message_id,
                bytes: wire_bytes,
                server_idx: slot.server_idx(),
                from: Arc::from(task.from.as_str()),
                date: task.date.clone(),
                full_crc32: task.full_crc32,
                file_index: task.file_index,
                total_files: task.total_files,
            });
            if let Some(tx) = events {
                let _ = tx.send(ProgressEvent::PostRetryRecovered {
                    count: 1,
                    previously_failed: true,
                });
                let _ = tx.send(ProgressEvent::Status {
                    text: format!("retry: {}/{} segment(s) recovered", recovered.len(), i + 1),
                });
            }
        } else {
            warn!(
                file = %task.file_name,
                part = task.part,
                "retry: gave up after all attempts"
            );
        }
    }

    if let Some(tx) = events {
        let _ = tx.send(ProgressEvent::Status {
            text: String::new(),
        });
    }

    Ok(recovered)
}
