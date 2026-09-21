use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tracing::warn;

use crate::config::Config;
use crate::nntp::pool::ConnectionSlot;
use crate::progress::{ProgressEvent, ProgressSender};

use super::{is_post_refusal, repost_one, PostedSegment};

/// Recovered (STAT 223) vs Inconclusive (STAT `Err`, or recovery POST
/// transport/AUTH/5xx) subsets of a recovery pass.
/// Anything in neither is still MissingConfirmed (430, or a 441/4xx recovery
/// POST refusal). Recovery never runs on articles that were already
/// Inconclusive — the caller only passes MissingConfirmed.
pub(crate) struct RecoverOutcome {
    pub recovered: Vec<PostedSegment>,
    pub inconclusive: Vec<PostedSegment>,
    pub slots: Vec<ConnectionSlot>,
}

/// One extra, bounded recovery attempt for articles that are still missing
/// after every `check_post_retries` round already ran out — the caller (see
/// `poster::maybe_recover_missing`) has already decided this batch is small
/// enough to be worth it. Unlike the streaming coordinator's normal flow,
/// every item here is already *known* to be missing (that's how it ended up
/// here), so this skips straight to [`repost_one`] instead of re-running the
/// patient STAT-retry sequence first, then verifies once via a single STAT
/// after `check_delay_secs` — one round trip per article, not the full
/// `check_retries`-attempt cycle.
///
/// Runs the batch across the supplied slots (already in the connection
/// budget — typically those returned by
/// [`CheckCoordinatorHandle::finish_and_drain`]), never more workers than
/// there is work or slots. Unused slots stay held so `--jobs` cannot start
/// the next episode until this pass is done. This used to be a strict
/// one-article-at-a-time loop: with `check_recover_max` defaulting to 50
/// and each article costing a repost round trip plus `check_delay_secs`, a
/// stubborn batch could take minutes with the upload already at 100% and
/// every upload connection sitting idle — the exact "why is this frozen"
/// case the recovery pass exists to resolve *quickly*.
///
/// Returns the subset of `segments` that got reposted *and* confirmed
/// present, plus the same slots (caller checkins the whole set). Anything
/// not in the recovered list is still genuinely missing.
pub(crate) async fn recover_missing(
    config: &Config,
    groups: &[String],
    segments: Vec<PostedSegment>,
    events: Option<&ProgressSender>,
    mut slots: Vec<ConnectionSlot>,
) -> RecoverOutcome {
    if segments.is_empty() || slots.is_empty() {
        return RecoverOutcome {
            recovered: Vec::new(),
            inconclusive: Vec::new(),
            slots,
        };
    }

    let total = segments.len() as u64;
    if let Some(tx) = events {
        let _ = tx.send(ProgressEvent::CheckRecoverStarted { total });
    }

    let n_workers = slots.len().min(segments.len());
    let work_slots: Vec<_> = slots.drain(..n_workers).collect();

    let queue = Arc::new(Mutex::new(segments));
    let done = Arc::new(AtomicUsize::new(0));
    let recovered = Arc::new(Mutex::new(Vec::with_capacity(total as usize)));
    let inconclusive = Arc::new(Mutex::new(Vec::new()));

    let mut workers = Vec::with_capacity(n_workers);
    for mut slot in work_slots {
        let config = config.clone();
        let groups = groups.to_vec();
        let queue = Arc::clone(&queue);
        let done = Arc::clone(&done);
        let recovered = Arc::clone(&recovered);
        let inconclusive = Arc::clone(&inconclusive);
        let events = events.cloned();

        workers.push(tokio::spawn(async move {
            loop {
                let seg = queue.lock().unwrap().pop();
                let Some(seg) = seg else { break };

                slot.retarget(seg.server_idx);

                let ok = match repost_one(&config, &mut slot, &seg, &groups).await {
                    Ok(new_seg) => {
                        tokio::time::sleep(Duration::from_secs(config.check_delay_secs)).await;
                        match slot.ensure_connected().await {
                            Ok(conn) => match conn.stat(&new_seg.message_id).await {
                                Ok(true) => {
                                    recovered.lock().unwrap().push(new_seg);
                                    true
                                }
                                Ok(false) => {
                                    warn!(
                                        id = %new_seg.message_id,
                                        "check: recovery repost accepted but STAT 430 on final check"
                                    );
                                    false
                                }
                                Err(e) => {
                                    warn!(
                                        id = %new_seg.message_id,
                                        error = %e,
                                        "check: recovery STAT error; classifying as inconclusive"
                                    );
                                    inconclusive.lock().unwrap().push(new_seg);
                                    false
                                }
                            },
                            Err(e) => {
                                warn!(
                                    id = %new_seg.message_id,
                                    error = %e,
                                    "check: recovery STAT connect error; classifying as inconclusive"
                                );
                                inconclusive.lock().unwrap().push(new_seg);
                                false
                            }
                        }
                    }
                    Err(e) => {
                        if is_post_refusal(&e) {
                            warn!(
                                id = %seg.message_id,
                                error = %e,
                                "check: final recovery repost refused; still missing"
                            );
                            false
                        } else {
                            warn!(
                                id = %seg.message_id,
                                error = %e,
                                "check: recovery POST path failed; classifying as inconclusive"
                            );
                            inconclusive.lock().unwrap().push(seg);
                            false
                        }
                    }
                };

                // Emits `CheckRecoverProgress` for *every* resolution,
                // success or failure — unlike the old plain `Status` text,
                // which only fired on success. A batch with real,
                // unrecoverable misses used to go completely silent
                // (nothing but a `tracing::warn!`, invisible without `-v`)
                // for however long those repost/STAT round trips took.
                let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                if let Some(tx) = &events {
                    let _ = tx.send(ProgressEvent::CheckRecoverProgress {
                        done: n as u64,
                        total,
                        ok,
                    });
                }
            }
            slot
        }));
    }

    for w in workers {
        if let Ok(slot) = w.await {
            slots.push(slot);
        }
    }

    RecoverOutcome {
        recovered: Arc::try_unwrap(recovered)
            .expect("every worker task has finished by now")
            .into_inner()
            .unwrap(),
        inconclusive: Arc::try_unwrap(inconclusive)
            .expect("every worker task has finished by now")
            .into_inner()
            .unwrap(),
        slots,
    }
}
