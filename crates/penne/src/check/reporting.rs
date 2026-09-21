use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::task::JoinHandle;

use super::plan::ItemResolution;
use super::plan::WorkItem;
use super::{CheckOutcome, FileCheck, MissingSegment, UnreachableSegment};
use crate::queue::DownloadQueue;

pub(super) fn spawn_tracker(
    outcome_tx: Option<tokio::sync::mpsc::UnboundedSender<(usize, CheckOutcome)>>,
    queues: &[DownloadQueue],
    totals: &[HashMap<String, u32>],
    total_items_per_q: &[u32],
    start: Instant,
    stop: Arc<AtomicBool>,
    mut item_rx: tokio::sync::mpsc::UnboundedReceiver<ItemResolution>,
) -> Option<JoinHandle<()>> {
    if let Some(out_tx) = outcome_tx {
        let qs = queues.to_vec();
        let totals_clone = totals.to_vec();
        let total_items_per_q_clone = total_items_per_q.to_vec();
        let start_time = start;
        let tracker_stop = stop.clone();

        Some(tokio::spawn(async move {
            let mut resolved = vec![0u32; qs.len()];
            let mut present = vec![HashMap::<String, u32>::new(); qs.len()];
            let mut missing = vec![Vec::<MissingSegment>::new(); qs.len()];
            let mut unreachable = vec![Vec::<UnreachableSegment>::new(); qs.len()];
            let mut sent = vec![false; qs.len()];

            while let Some(res) = item_rx.recv().await {
                let q_idx = match &res {
                    ItemResolution::Found(item) => item.queue_idx,
                    ItemResolution::Missing(item) => item.queue_idx,
                    ItemResolution::Unreachable(item) => item.queue_idx,
                };

                match res {
                    ItemResolution::Found(item) => {
                        *present[q_idx].entry(item.file_name).or_insert(0) += 1;
                    }
                    ItemResolution::Missing(item) => {
                        missing[q_idx].push(MissingSegment {
                            file_name: item.file_name,
                            part: item.part,
                            message_id: item.message_id,
                        });
                    }
                    ItemResolution::Unreachable(item) => {
                        unreachable[q_idx].push(UnreachableSegment {
                            file_name: item.file_name,
                            part: item.part,
                            message_id: item.message_id,
                        });
                    }
                }

                resolved[q_idx] += 1;

                if resolved[q_idx] == total_items_per_q_clone[q_idx] {
                    let total_items = total_items_per_q_clone[q_idx];
                    let total_present = present[q_idx].values().sum();

                    let files = qs[q_idx]
                        .files
                        .iter()
                        .map(|f| FileCheck {
                            name: f.name.clone(),
                            total_segments: *totals_clone[q_idx].get(&f.name).unwrap_or(&0),
                            present_segments: *present[q_idx].get(&f.name).unwrap_or(&0),
                        })
                        .collect();

                    let outcome = CheckOutcome {
                        files,
                        missing: std::mem::take(&mut missing[q_idx]),
                        unreachable: std::mem::take(&mut unreachable[q_idx]),
                        bytes_used: 0,
                        elapsed: start_time.elapsed(),
                        total_checked: total_items,
                        total_present,
                        stopped_early: false,
                        skipped: 0,
                    };

                    let _ = out_tx.send((q_idx, outcome));
                    sent[q_idx] = true;
                }
            }

            // With fail-fast, deliberately skipped items do not emit a
            // resolution event. Send the partial per-NZB result once the
            // workers have stopped so JSON callers still get one line.
            if tracker_stop.load(Ordering::Relaxed) {
                for q_idx in 0..qs.len() {
                    if sent[q_idx] {
                        continue;
                    }
                    let total_items = total_items_per_q_clone[q_idx];
                    let files = qs[q_idx]
                        .files
                        .iter()
                        .map(|f| FileCheck {
                            name: f.name.clone(),
                            total_segments: *totals_clone[q_idx].get(&f.name).unwrap_or(&0),
                            present_segments: *present[q_idx].get(&f.name).unwrap_or(&0),
                        })
                        .collect();
                    let _ = out_tx.send((
                        q_idx,
                        CheckOutcome {
                            files,
                            missing: std::mem::take(&mut missing[q_idx]),
                            unreachable: std::mem::take(&mut unreachable[q_idx]),
                            bytes_used: 0,
                            elapsed: start_time.elapsed(),
                            total_checked: total_items,
                            total_present: present[q_idx].values().sum(),
                            stopped_early: true,
                            skipped: total_items - resolved[q_idx],
                        },
                    ));
                }
            }
        }))
    } else {
        None
    }
}

pub(super) struct OutcomeInputs<'a> {
    pub(super) queues: &'a [DownloadQueue],
    pub(super) totals: &'a [HashMap<String, u32>],
    pub(super) total_items_per_q: &'a [u32],
    pub(super) present: Vec<HashMap<String, u32>>,
    pub(super) pending: Vec<WorkItem>,
    pub(super) bytes_used: u64,
    pub(super) start: Instant,
    pub(super) stopped_early: bool,
}

pub(super) fn build_outcomes(inputs: OutcomeInputs<'_>) -> Vec<CheckOutcome> {
    let OutcomeInputs {
        queues,
        totals,
        total_items_per_q,
        present,
        pending,
        bytes_used,
        start,
        stopped_early,
    } = inputs;

    let mut missing_per_q: Vec<Vec<MissingSegment>> = vec![Vec::new(); queues.len()];
    let mut unreachable_per_q: Vec<Vec<UnreachableSegment>> = vec![Vec::new(); queues.len()];
    let mut skipped_per_q = vec![0u32; queues.len()];
    for item in pending {
        // `confirmed_missing` was set the instant any tried tier returned a
        // definitive 430/423/420 for this item. A later tier merely failing
        // to connect never clears it, so this is a true final verdict.
        if item.confirmed_missing {
            missing_per_q[item.queue_idx].push(MissingSegment {
                file_name: item.file_name,
                part: item.part,
                message_id: item.message_id,
            });
        } else if stopped_early {
            skipped_per_q[item.queue_idx] += 1;
        } else {
            unreachable_per_q[item.queue_idx].push(UnreachableSegment {
                file_name: item.file_name,
                part: item.part,
                message_id: item.message_id,
            });
        }
    }

    let mut outcomes = Vec::with_capacity(queues.len());
    let bytes_per_q = if queues.is_empty() {
        0
    } else {
        bytes_used / queues.len() as u64
    };

    for (q_idx, queue) in queues.iter().enumerate() {
        let total_items = total_items_per_q[q_idx];
        let total_present = present[q_idx].values().sum();
        let files = queue
            .files
            .iter()
            .map(|file| FileCheck {
                name: file.name.clone(),
                total_segments: *totals[q_idx].get(&file.name).unwrap_or(&0),
                present_segments: *present[q_idx].get(&file.name).unwrap_or(&0),
            })
            .collect();

        outcomes.push(CheckOutcome {
            files,
            missing: std::mem::take(&mut missing_per_q[q_idx]),
            unreachable: std::mem::take(&mut unreachable_per_q[q_idx]),
            bytes_used: bytes_per_q,
            elapsed: start.elapsed(),
            total_checked: total_items,
            total_present,
            stopped_early: stopped_early && skipped_per_q[q_idx] > 0,
            skipped: skipped_per_q[q_idx],
        });
    }

    outcomes
}
