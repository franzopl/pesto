use std::collections::HashMap;

use crate::queue::DownloadQueue;

#[derive(Debug)]
pub(super) enum ItemResolution {
    Found(WorkItem),
    Missing(WorkItem),
    Unreachable(WorkItem),
}

/// One segment still needing to be checked.
#[derive(Debug, Clone)]
pub(super) struct WorkItem {
    pub(super) queue_idx: usize,
    pub(super) file_name: String,
    pub(super) part: u32,
    pub(super) message_id: String,
    /// Set the instant any tried server gives a definitive `430`/`423`/
    /// `420` for this item. Carried across tiers so a *later* tier's mere
    /// connection failure never downgrades an earlier tier's conclusive
    /// denial into [`super::UnreachableSegment`] — only an item nobody ever
    /// managed to get a real answer for should end up there.
    pub(super) confirmed_missing: bool,
}

pub(super) struct CheckPlan {
    pub(super) totals: Vec<HashMap<String, u32>>,
    pub(super) total_items_per_q: Vec<u32>,
    pub(super) pending: Vec<WorkItem>,
}

pub(super) fn build(queues: &[DownloadQueue]) -> CheckPlan {
    let mut totals = vec![HashMap::new(); queues.len()];
    let mut pending = Vec::new();

    for (q_idx, queue) in queues.iter().enumerate() {
        for file in &queue.files {
            totals[q_idx].insert(file.name.clone(), file.segments.len() as u32);
            for seg in &file.segments {
                pending.push(WorkItem {
                    queue_idx: q_idx,
                    file_name: file.name.clone(),
                    part: seg.part,
                    message_id: seg.message_id.clone(),
                    confirmed_missing: false,
                });
            }
        }
    }

    let total_items_per_q = queues
        .iter()
        .map(|queue| {
            queue
                .files
                .iter()
                .map(|file| file.segments.len() as u32)
                .sum()
        })
        .collect();

    CheckPlan {
        totals,
        total_items_per_q,
        pending,
    }
}
