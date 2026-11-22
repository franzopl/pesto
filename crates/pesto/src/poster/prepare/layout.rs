//! Release layout: the segment-identity planner for encrypted uploads.

use std::sync::Arc;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::yenc;
use parmesan::layout;

use super::super::outcome::nth_safe_segment_index;
pub(crate) use super::super::par2::geometry::{par2_base_packets_len, par2_volume_len};
use super::super::{par2_volume_ordinal, FileMeta};

/// An entry in a release layout representing one file's ordinal, part count,
/// and prefix part sum (total segments preceding this file in release order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct LayoutEntry {
    pub(crate) part_count: u32,
    pub(crate) prefix_parts: u64,
}

/// An immutable, complete release layout computed before concurrent task dispatch.
///
/// Maps every 1-based `file_index` `1..=total_files` to its part count and
/// preceding segment prefix sum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseLayout {
    pub(crate) total_files: u32,
    pub(crate) entries: Vec<LayoutEntry>,
}

impl ReleaseLayout {
    /// Construct a ReleaseLayout from a slice of `(file_index, part_count)`.
    ///
    /// Validates that:
    /// - `total_files > 0`
    /// - Every ordinal from 1 to `total_files` is present exactly once
    /// - Every file has `part_count >= 1`
    /// - Cumulative segments and individual indices do not overflow `u32::MAX`
    pub(crate) fn from_parts(total_files: u32, parts: &[(u32, u32)]) -> Result<Self> {
        if total_files == 0 {
            bail!("release layout cannot have 0 total files");
        }
        if parts.len() != total_files as usize {
            bail!(
                "release layout parts count ({}) does not match total files ({})",
                parts.len(),
                total_files
            );
        }

        let mut sorted = parts.to_vec();
        sorted.sort_by_key(|&(ord, _)| ord);

        let mut entries = Vec::with_capacity(sorted.len());
        let mut prefix_parts = 0u64;

        for (expected_idx, &(ord, count)) in sorted.iter().enumerate() {
            let expected_ord = expected_idx as u32 + 1;
            if ord != expected_ord {
                bail!("release layout missing or non-contiguous ordinal: expected {expected_ord}, got {ord}");
            }
            if count == 0 {
                bail!("release layout file {ord} has zero parts");
            }
            entries.push(LayoutEntry {
                part_count: count,
                prefix_parts,
            });
            prefix_parts = prefix_parts
                .checked_add(u64::from(count))
                .context("release layout cumulative segment count overflow")?;
            if prefix_parts > u64::from(u32::MAX) {
                bail!("release layout total segments exceeds u32::MAX: {prefix_parts}");
            }
            // Safe-index capacity: the index framing rule's skip mapping can push the
            // highest assigned safe index past u32::MAX earlier than the raw
            // cumulative count check above — fail layout construction with an
            // error (never panic) in that case.
            let last_rank = prefix_parts;
            anyhow::ensure!(
                nth_safe_segment_index(last_rank).is_some(),
                "release layout total segments ({last_rank}) exceed the safe \
                 segment-index capacity of u32"
            );
        }

        Ok(ReleaseLayout {
            total_files,
            entries,
        })
    }

    /// Get the layout entry for a given 1-based `file_index`.
    pub(crate) fn entry(&self, file_index: u32) -> Option<&LayoutEntry> {
        if file_index == 0 || file_index > self.total_files {
            None
        } else {
            self.entries.get((file_index - 1) as usize)
        }
    }

    /// Compute the globally unique segment index for a
    /// given file ordinal and part number.
    ///
    /// This is the CENTRAL segment-index allocator: the raw 1-based rank
    /// (`prefix_parts + part_number`) is mapped release-wide through
    /// [`nth_safe_segment_index`], which skips every value forbidden by the index framing rule
    /// (zero, or any index whose big-endian bytes contain `0x0A`/`0x0D` — see
    /// yEnc Body Encryption Standard v1.2 §8). Every
    /// identity consumer (data paths in `producer.rs`/`mod.rs` and the PAR2
    /// path) goes through this method, so all consumers receive safe,
    /// monotonic, injective indices. Identity is finalized here BEFORE any
    /// salt/KDF derivation happens downstream.
    pub(crate) fn segment_identity(&self, file_index: u32, part_number: u32) -> Option<u32> {
        let entry = self.entry(file_index)?;
        if part_number == 0 || part_number > entry.part_count {
            return None;
        }
        let rank = entry.prefix_parts.checked_add(u64::from(part_number))?;
        nth_safe_segment_index(rank)
    }

    /// The layout used by unencrypted uploads: no entries, no identity.
    ///
    /// `segment_identity` returns `None` for it, which is fine — an
    /// unencrypted article has no bootstrap and no index, and the plain
    /// upload path never calls this (see the encryption gate in
    /// `prepare_resources` and the `encrypt_password.is_some()` guards in
    /// `producer.rs`/`mod.rs`).
    pub(crate) fn empty() -> Self {
        ReleaseLayout {
            total_files: 0,
            entries: Vec::new(),
        }
    }

    /// Build the release layout from prepared input files and PAR2 configuration.
    pub(in crate::poster) fn build(
        metas: &[Arc<FileMeta>],
        config: &Config,
        recovery_count: usize,
        par2_slice_size: usize,
    ) -> Result<Self> {
        let article_size = config.article_size;
        let mut parts_list = Vec::with_capacity(metas.len() + 16);

        for meta in metas {
            if meta.size >= (1u64 << 32) {
                bail!(
                    "Encrypted file size must be less than 4 GiB (2^32 bytes): {}",
                    meta.client_path
                );
            }
            let segs = yenc::segments(meta.size, article_size);
            let part_count =
                u32::try_from(segs.len()).context("data file part count exceeds u32")?;
            parts_list.push((meta.file_index, part_count));
        }

        let par2_file_count = if recovery_count > 0 {
            let publish_index = config.obfuscate.policy().publish_par2_index;
            let base_len = par2_base_packets_len(metas, par2_slice_size);

            if publish_index {
                anyhow::ensure!(
                    base_len < (1u64 << 32),
                    "Encrypted PAR2 index size must be less than 4 GiB (2^32 bytes)"
                );
                let index_ordinal = metas.len() as u32 + 1;
                let index_parts = u32::try_from(yenc::segments(base_len, article_size).len())
                    .context("PAR2 index part count exceeds u32")?;
                parts_list.push((index_ordinal, index_parts));
            }

            let volumes = layout::plan_volumes(recovery_count as u32);
            for (vol_idx, vol) in volumes.iter().enumerate() {
                let vol_ordinal = par2_volume_ordinal(metas.len(), publish_index, vol_idx);
                let vol_len = par2_volume_len(base_len, vol.count, par2_slice_size);
                anyhow::ensure!(
                    vol_len < (1u64 << 32),
                    "Encrypted PAR2 volume size must be less than 4 GiB (2^32 bytes)"
                );
                let vol_parts = u32::try_from(yenc::segments(vol_len, article_size).len())
                    .context("PAR2 volume part count exceeds u32")?;
                parts_list.push((vol_ordinal, vol_parts));
            }

            usize::from(publish_index) + volumes.len()
        } else {
            0
        };

        let total_files = u32::try_from(metas.len() + par2_file_count)
            .context("total release files exceed u32")?;

        Self::from_parts(total_files, &parts_list)
    }
}
