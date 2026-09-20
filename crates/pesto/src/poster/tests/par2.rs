use super::*;

// ── PAR2 geometry ────────────────────────────────────────────────────────

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
    let mut config = dry_run_config();
    config.article_size = article_size;
    config.par2 = redundancy_pct;
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
    let mut config = dry_run_config();
    config.article_size = 768_000;
    config.par2 = 10;
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

// ── par2_output_dir ───────────────────────────────────────────────────────

#[test]
fn par2_output_dir_loose_file_is_parent_dir() {
    // A single-component name like "movie.mkv" lives directly next to the file.
    let path = std::path::PathBuf::from("/data/movie.mkv");
    let meta = meta_with_name(&path, "movie.mkv");
    assert_eq!(par2_output_dir(&meta), std::path::Path::new("/data"));
}

#[test]
fn par2_output_dir_nested_file_strips_depth() {
    // "Season01/ep01.mkv" has depth 2, so par2 dir is 2 levels up.
    let path = std::path::PathBuf::from("/data/Season01/ep01.mkv");
    let meta = meta_with_name(&path, "Season01/ep01.mkv");
    assert_eq!(par2_output_dir(&meta), std::path::Path::new("/data"));
}

#[test]
fn par2_output_dir_three_levels_deep() {
    let path = std::path::PathBuf::from("/srv/a/b/c.bin");
    let meta = meta_with_name(&path, "a/b/c.bin");
    assert_eq!(par2_output_dir(&meta), std::path::Path::new("/srv"));
}
