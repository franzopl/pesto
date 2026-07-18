//! Draining a [`DownloadQueue`] against configured servers.
//!
//! Fetches each queued segment's article body and decodes it with
//! `pesto::yenc::decode_part` (Phase 3), trying servers in priority order
//! per segment — so a primary provider missing, or serving a truncated copy
//! of, a handful of articles doesn't fail a file a backup server has intact.
//! File assembly (Phase 4) consumes the [`pesto::yenc::DecodedPart`]s this
//! returns; this module does no disk I/O.
//!
//! One connection per server is opened lazily (only if that server is
//! actually needed) and reused for the rest of the run. True N-parallel-
//! connections-per-server concurrency, mirroring `pesto::nntp::pool`, is not
//! implemented yet — see `ROADMAP.md` Phase 2.

use std::collections::HashMap;

use anyhow::Result;
use pesto::config::ServerEntry;
use pesto::yenc::{decode_part, DecodedPart};

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
pub async fn download_queue(
    queue: &DownloadQueue,
    servers: &[ServerEntry],
    progress: Option<ProgressSender>,
) -> Result<DownloadOutcome> {
    anyhow::ensure!(!servers.is_empty(), "no servers configured");

    let mut clients: Vec<Option<DownloadClient>> = (0..servers.len()).map(|_| None).collect();
    let mut outcome = DownloadOutcome::default();

    for file in &queue.files {
        for seg in &file.segments {
            let mut found = None;
            let mut last_decode_err: Option<String> = None;

            for (idx, server) in servers.iter().enumerate() {
                let Some(client) = ensure_connected(&mut clients, idx, server).await else {
                    continue;
                };
                let body = match client.body(&seg.message_id).await {
                    Ok(Some(body)) => body,
                    Ok(None) => continue,
                    Err(_) => {
                        // Connection likely dead; drop it and let the next
                        // segment reconnect lazily.
                        clients[idx] = None;
                        continue;
                    }
                };
                match decode_part(&body) {
                    Ok(decoded) => {
                        found = Some(decoded);
                        break;
                    }
                    Err(e) => {
                        last_decode_err = Some(e.to_string());
                        continue;
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

/// Return a connected client for server `idx`, connecting lazily on first
/// use. `None` if this server could not be reached at all.
async fn ensure_connected<'a>(
    clients: &'a mut [Option<DownloadClient>],
    idx: usize,
    server: &ServerEntry,
) -> Option<&'a mut DownloadClient> {
    if clients[idx].is_none() {
        clients[idx] = DownloadClient::connect(server).await.ok();
    }
    clients[idx].as_mut()
}
