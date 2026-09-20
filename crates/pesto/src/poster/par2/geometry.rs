//! PAR2 recovery-set geometry shared by the per-file and season paths.

use std::path::PathBuf;
use std::sync::Arc;

use crate::config::Config;
use parmesan::ops::{
    calculate_geometry, CreateOptions as Par2CreateOptions, InputFile as Par2InputFile,
};

use super::super::FileMeta;

/// Compute the PAR2 recovery-set geometry `(slice_size_bytes,
/// total_input_slices, recovery_block_count)` that `producer` will use for
/// this batch of files, given the current config. Pure and cheap — only
/// reads file sizes already collected in `metas`, no I/O — so it can be
/// called before encoding actually starts to seed an exact (not estimated)
/// progress total. Mirrors the geometry logic in `producer` exactly; keep
/// the two in sync.
pub(crate) fn par2_geometry(metas: &[Arc<FileMeta>], config: &Config) -> (usize, usize, usize) {
    let sizes: Vec<u64> = metas.iter().map(|m| m.size).collect();
    par2_geometry_from_sizes(&sizes, config)
}

/// Shared PAR2 geometry for the per-file path and the season path so
/// `--par2-slice-size` / `--par2-slice-count` / `--par2-recovery-count`
/// cannot drift between them.
pub(crate) fn par2_geometry_from_sizes(sizes: &[u64], config: &Config) -> (usize, usize, usize) {
    let files: Vec<Par2InputFile> = sizes
        .iter()
        .enumerate()
        .map(|(i, &size)| Par2InputFile {
            path: PathBuf::new(),
            display_name: i.to_string(),
            size,
        })
        .collect();
    let options = Par2CreateOptions {
        slice_size: config.par2_slice_size,
        slice_count: config.par2_slice_count,
        recovery_count: config.par2_recovery_count,
        recovery_pct: config.par2,
        ..Par2CreateOptions::default()
    };
    match calculate_geometry(&files, &options) {
        Ok(geometry) => geometry,
        Err(_) => {
            // Overflow of the PAR2 slice/recovery ceilings: return the counts
            // so the caller can emit the same error it always has.
            let s = config
                .par2_slice_size
                .map(|s| (s / 64 * 64).max(64))
                .unwrap_or(64);
            let n: usize = sizes
                .iter()
                .map(|sz| (*sz as usize).div_ceil(s.max(1)))
                .sum();
            let rec = config
                .par2_recovery_count
                .unwrap_or(n.saturating_mul(config.par2 as usize) / 100);
            (s, n, rec)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{FileConfig, Overrides};

    fn geometry_config(article_size: usize, redundancy_pct: u8) -> Config {
        let mut file = FileConfig::default();
        file.posting.groups = Some(vec!["alt.test".into()]);
        let mut config = Config::resolve(
            file,
            Overrides {
                dry_run: Some(true),
                par2: Some(redundancy_pct),
                ..Default::default()
            },
        )
        .unwrap();
        config.article_size = article_size;
        config
    }

    fn optimal_par2_slice_size(
        per_file_articles: &[usize],
        article_size: usize,
        redundancy_pct: u8,
    ) -> (usize, usize) {
        if per_file_articles.is_empty() || per_file_articles.iter().all(|&n| n == 0) {
            return (article_size, 0);
        }
        let sizes: Vec<u64> = per_file_articles
            .iter()
            .map(|&n| n as u64 * article_size as u64)
            .collect();
        let config = geometry_config(article_size, redundancy_pct);
        let (sz, slices, _) = par2_geometry_from_sizes(&sizes, &config);
        (sz, slices)
    }

    #[test]
    fn optimal_slice_single_file_within_target() {
        // 500 articles with 10% redundancy: well within limits.
        let (sz, slices) = optimal_par2_slice_size(&[500], 750_000, 10);
        assert!(slices <= 32768);
        assert!((slices * 10 / 100) <= 65535);
        assert!(sz >= 64);
    }

    #[test]
    fn optimal_slice_no_redundancy_respects_32768_limit() {
        // 5000 files × 1 article: well within 32768, should satisfy the limit.
        let per_file = vec![1usize; 5_000];
        let (sz, slices) = optimal_par2_slice_size(&per_file, 100, 0);
        assert!(slices <= 32768, "slices={slices}");
        assert!(sz >= 100);
    }

    #[test]
    fn optimal_slice_too_many_files_returns_best_effort() {
        // 50 000 files × 1 article each: minimum possible is 50 000 slices > 32 768.
        // The function must not panic and should return the minimum achievable.
        let per_file = vec![1usize; 50_000];
        let (_sz, slices) = optimal_par2_slice_size(&per_file, 100, 0);
        assert!(slices >= 50_000, "slices={slices}");
    }

    #[test]
    fn optimal_slice_high_redundancy_respects_65535_recovery_limit() {
        // 200% redundancy: max input slices = 65535 * 100 / 200 = 32767.
        // 100 files × 400 articles each = 40 000 total articles.
        // Grouping can reduce to ~1000 slices, well within 32767.
        let per_file = vec![400usize; 100];
        let (sz, slices) = optimal_par2_slice_size(&per_file, 100, 200);
        let recovery = slices * 200 / 100;
        assert!(slices <= 32767, "slices={slices}");
        assert!(recovery <= 65535, "recovery={recovery}");
        assert!(sz >= 100);
    }

    #[test]
    fn optimal_slice_mixed_sizes() {
        // One large file (10 000 articles) and many tiny files (1 article each).
        let mut per_file = vec![1usize; 5_000];
        per_file.push(10_000);
        let (sz, slices) = optimal_par2_slice_size(&per_file, 750_000, 10);
        assert!(slices <= 32768, "slices={slices}");
        assert!((slices * 10 / 100) <= 65535);
        assert!(sz >= 64);
    }

    #[test]
    fn many_small_files_do_not_inflate_slice_to_article_groups() {
        // The many-small corpus: 2000 × 256 KiB files, 768 KiB articles.
        // Grouping *articles* as if they could be merged across files used to
        // pick a 3 MiB slice (4 articles) and still emit 2000 slices — 12×
        // padding. Slice size must stay near the file size.
        let file_size = 256 * 1024u64;
        let sizes = vec![file_size; 2000];
        let config = geometry_config(768_000, 10);
        let (slice_size, slices, recovery) = par2_geometry_from_sizes(&sizes, &config);
        assert_eq!(slices, 2000, "slices={slices}");
        assert!(
            slice_size <= file_size as usize,
            "slice_size={slice_size} padded each 256 KiB file"
        );
        let padded = slices * slice_size;
        let actual = 2000 * file_size as usize;
        assert!(
            padded as f64 / actual as f64 <= 1.15,
            "padding {} / {actual}",
            padded
        );
        assert_eq!(recovery, 200);
    }

    #[test]
    fn optimal_slice_empty_input() {
        let (sz, slices) = optimal_par2_slice_size(&[], 750_000, 10);
        assert_eq!(slices, 0);
        assert_eq!(sz, 750_000);
    }

    #[test]
    fn optimal_slice_single_article() {
        let (_sz, slices) = optimal_par2_slice_size(&[1], 750_000, 5);
        assert!(slices >= 1);
    }
}
