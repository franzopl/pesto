//! Draining a [`DownloadQueue`] against configured servers.
//!
//! Fetches each queued segment's raw (still yEnc-encoded) body, trying
//! servers in priority order per segment — so a primary provider missing a
//! handful of articles doesn't fail a file that a backup server has intact.
//! Decoding ([`crate::client`]'s bodies are still yEnc; see `ROADMAP.md`
//! Phase 3) and file assembly (Phase 4) consume the bytes this returns; this
//! module does no disk I/O.
//!
//! One connection per server is opened lazily (only if that server is
//! actually needed) and reused for the rest of the run. True N-parallel-
//! connections-per-server concurrency, mirroring `pesto::nntp::pool`, is not
//! implemented yet — see `ROADMAP.md` Phase 2.

use std::collections::HashMap;

use anyhow::Result;
use pesto::config::ServerEntry;

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

/// Result of draining a [`DownloadQueue`] against a set of servers.
#[derive(Debug, Default)]
pub struct DownloadOutcome {
    /// Fetched bodies keyed by Message-ID, still yEnc-encoded.
    pub bodies: HashMap<String, Vec<u8>>,
    /// Segments no configured server had.
    pub missing: Vec<MissingSegment>,
}

/// Fetch every segment in `queue` from `servers`, tried in priority order
/// per segment.
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

            for (idx, server) in servers.iter().enumerate() {
                let Some(client) = ensure_connected(&mut clients, idx, server).await else {
                    continue;
                };
                match client.body(&seg.message_id).await {
                    Ok(Some(body)) => {
                        found = Some(body);
                        break;
                    }
                    Ok(None) => continue,
                    Err(_) => {
                        // Connection likely dead; drop it and let the next
                        // segment reconnect lazily.
                        clients[idx] = None;
                        continue;
                    }
                }
            }

            match found {
                Some(body) => {
                    if let Some(tx) = &progress {
                        let _ = tx.send(ProgressEvent::SegmentDownloaded {
                            file_name: file.name.clone(),
                            part: seg.part,
                            bytes: body.len() as u64,
                        });
                    }
                    outcome.bodies.insert(seg.message_id.clone(), body);
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
