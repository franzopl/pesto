//! Draining a [`DownloadQueue`] against configured servers.
//!
//! Fetches each queued segment's article body and decodes it with
//! `pesto::yenc::decode_part` (Phase 3), trying servers in priority order
//! per segment — so a primary provider missing, or serving a truncated copy
//! of, a handful of articles doesn't fail a file a backup server has intact.
//! File assembly (Phase 4) consumes the [`pesto::yenc::DecodedPart`]s this
//! returns; this module does no disk I/O of its own, beyond consulting
//! [`crate::cache`] for resume (Phase 8).
//!
//! Two resilience mechanisms live here (`ROADMAP.md` Phase 8):
//! - **Cache-first fetch:** before any network request, [`crate::cache`] is
//!   checked for a body already fetched in a previous, interrupted run of
//!   this same download. A cache hit skips the network entirely.
//! - **Retry with backoff:** a connection or fetch error against one server
//!   is retried up to `retries` times (each server's own `retry_delay`
//!   governs the pause) before moving on to the next configured server — a
//!   transient hiccup shouldn't immediately write off a server that
//!   otherwise has the article.
//!
//! One connection per server is opened lazily (only if that server is
//! actually needed) and reused for the rest of the run. True N-parallel-
//! connections-per-server concurrency, mirroring `pesto::nntp::pool`, is not
//! implemented yet — see `ROADMAP.md` Phase 2.

use std::collections::HashMap;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use pesto::config::ServerEntry;
use pesto::yenc::{decode_part, DecodedPart};

use crate::cache;
use crate::client::DownloadClient;
use crate::progress::{ProgressEvent, ProgressSender};
use crate::queue::DownloadQueue;

/// A segment that no configured server had.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingSegment {
    pub file_name: String,
    pub part: u32,
    pub message_id: String,
}

/// A segment that was fetched but could not be decoded as yEnc by any server
/// that had it (a truncated or otherwise corrupted transfer). Distinct from
/// [`MissingSegment`]: the article exists somewhere, but no copy retrieved
/// was structurally valid yEnc.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorruptSegment {
    pub file_name: String,
    pub part: u32,
    pub message_id: String,
    /// The last decode error seen, from whichever server's copy was tried
    /// last.
    pub error: String,
}

/// Result of draining a [`DownloadQueue`] against a set of servers.
#[derive(Debug, Default)]
pub struct DownloadOutcome {
    /// Successfully fetched and decoded segments, keyed by Message-ID. Check
    /// [`DecodedPart::crc_matches`] before trusting the content — a segment
    /// can decode structurally fine yet still fail its own checksum.
    pub segments: HashMap<String, DecodedPart>,
    /// Segments no configured server had.
    pub missing: Vec<MissingSegment>,
    /// Segments fetched but not decodable from any server that had them.
    pub corrupt: Vec<CorruptSegment>,
}

/// Fetch and decode every segment in `queue` from `servers`, tried in
/// priority order per segment. A decode failure on one server's copy is not
/// fatal: the next configured server is tried before giving up on the
/// segment, since the failure may be specific to that one transfer.
///
/// `dest_dir` is used only to consult/populate the resume cache
/// ([`crate::cache`]) — no other file I/O happens here. `retries` bounds how
/// many times a connection/fetch error against one server is retried (with
/// that server's own `retry_delay` between attempts) before moving to the
/// next server.
pub async fn download_queue(
    queue: &DownloadQueue,
    servers: &[ServerEntry],
    dest_dir: &Path,
    retries: u32,
    progress: Option<ProgressSender>,
) -> Result<DownloadOutcome> {
    anyhow::ensure!(!servers.is_empty(), "no servers configured");

    let mut clients: Vec<Option<DownloadClient>> = (0..servers.len()).map(|_| None).collect();
    let mut outcome = DownloadOutcome::default();

    for file in &queue.files {
        for seg in &file.segments {
            let mut found = None;
            let mut last_decode_err: Option<String> = None;

            if let Some(cached) = cache::load(dest_dir, &seg.message_id) {
                if let Ok(decoded) = decode_part(&cached) {
                    found = Some(decoded);
                }
                // A corrupted cache entry (shouldn't happen, but a killed
                // write mid-flush is possible) falls through to a normal
                // network fetch below rather than failing the segment.
            }

            if found.is_none() {
                for (idx, server) in servers.iter().enumerate() {
                    let body = match fetch_from_server(
                        &mut clients,
                        idx,
                        server,
                        &seg.message_id,
                        retries,
                    )
                    .await
                    {
                        Ok(Some(body)) => body,
                        Ok(None) => continue,
                        Err(_) => continue, // exhausted retries; try the next server
                    };
                    match decode_part(&body) {
                        Ok(decoded) => {
                            // Cache the raw body, not the decoded form — see
                            // the module docs on why.
                            let _ = cache::store(dest_dir, &seg.message_id, &body);
                            found = Some(decoded);
                            break;
                        }
                        Err(e) => {
                            last_decode_err = Some(e.to_string());
                            continue;
                        }
                    }
                }
            }

            match found {
                Some(decoded) => {
                    if let Some(tx) = &progress {
                        let _ = tx.send(ProgressEvent::SegmentDownloaded {
                            file_name: file.name.clone(),
                            part: seg.part,
                            bytes: decoded.data.len() as u64,
                        });
                    }
                    outcome.segments.insert(seg.message_id.clone(), decoded);
                }
                None => match last_decode_err {
                    Some(error) => {
                        if let Some(tx) = &progress {
                            let _ = tx.send(ProgressEvent::SegmentCorrupt {
                                file_name: file.name.clone(),
                                part: seg.part,
                                error: error.clone(),
                            });
                        }
                        outcome.corrupt.push(CorruptSegment {
                            file_name: file.name.clone(),
                            part: seg.part,
                            message_id: seg.message_id.clone(),
                            error,
                        });
                    }
                    None => {
                        if let Some(tx) = &progress {
                            let _ = tx.send(ProgressEvent::SegmentMissing {
                                file_name: file.name.clone(),
                                part: seg.part,
                            });
                        }
                        outcome.missing.push(MissingSegment {
                            file_name: file.name.clone(),
                            part: seg.part,
                            message_id: seg.message_id.clone(),
                        });
                    }
                },
            }
        }
    }

    for client in clients.into_iter().flatten() {
        client.quit().await;
    }

    Ok(outcome)
}

/// Fetch `message_id` from server `idx`, retrying a connection or transport
/// error up to `retries` times (sleeping `server.retry_delay` seconds
/// between attempts), reconnecting each time since an error likely means
/// the connection is now dead.
///
/// `Ok(None)` (the server explicitly doesn't have the article, `430`) is
/// never retried — that is a definitive answer, not a transient failure.
async fn fetch_from_server(
    clients: &mut [Option<DownloadClient>],
    idx: usize,
    server: &ServerEntry,
    message_id: &str,
    retries: u32,
) -> Result<Option<Vec<u8>>> {
    let mut last_err = None;

    for attempt in 0..=retries {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_secs(server.retry_delay)).await;
        }

        if clients[idx].is_none() {
            match DownloadClient::connect(server).await {
                Ok(client) => clients[idx] = Some(client),
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            }
        }

        let client = clients[idx].as_mut().expect("just connected above");
        match client.body(message_id).await {
            Ok(result) => return Ok(result),
            Err(e) => {
                // Connection likely dead; drop it so the next attempt (or
                // the next segment) reconnects instead of reusing it.
                clients[idx] = None;
                last_err = Some(e);
            }
        }
    }

    Err(last_err.expect("loop always runs at least once and only exits early on Ok"))
}
