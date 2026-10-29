//! Slice size and slice/recovery counts for a recovery set.

use super::{CreateOptions, InputFile};
use anyhow::Result;

/// Computes total padded bytes for a given slice size across all files.
fn total_padded_bytes(files: &[InputFile], slice_size: usize) -> usize {
    files
        .iter()
        .map(|f| (f.size as usize).div_ceil(slice_size) * slice_size)
        .sum()
}

/// Computes total input slice count for a given slice size.
fn total_slice_count(files: &[InputFile], slice_size: usize) -> usize {
    files
        .iter()
        .map(|f| (f.size as usize).div_ceil(slice_size))
        .sum()
}

/// Slices the automatic geometry aims for at [`CreateOptions::effort`] 1.0.
/// Creation and repair CPU time per byte grow with the slice count, since
/// recovery blocks are a percentage of it.
const TARGET_SLICES: usize = 2_000;

/// With a [`CreateOptions::slice_unit`], a set smaller than this may spend the
/// slice budget of a set this size: more slices per byte, at about the total
/// cost of a set this size.
const FULL_BUDGET_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// PAR2 caps a recovery set at 32768 input slices.
const MAX_SLICES: usize = 32_768;

/// Upper bound on slices we allow when optimising for padding. Keeps GF
/// computation time bounded on slow machines (CPU cost ∝ total_slices).
const MAX_SLICES_PADDING_OPT: usize = 6_000;

/// Padded bytes over real bytes above which small files are considered to
/// inflate the set.
const PADDING_RATIO_THRESHOLD: f64 = 1.15;

/// Calculates slice size and slice/recovery counts for a PAR2 recovery set.
///
/// An explicit [`CreateOptions::slice_size`] or [`CreateOptions::slice_count`]
/// wins. Otherwise:
///
/// - With a [`CreateOptions::slice_unit`], the slice size is a whole multiple
///   of the unit, chosen by [`aligned_slice_size`]: a unit lost in transit
///   (pesto passes its article size) then damages exactly one slice.
/// - Without one, or when the aligned size would pad many small files, the
///   heuristic targets [`TARGET_SLICES`] × effort slices, then halves the
///   slice size while padding from small files (common on Blu-ray/DVD disc
///   structures) inflates the set by more than 15 %, up to
///   [`MAX_SLICES_PADDING_OPT`] slices.
///
/// Reducing slice size does not increase peak memory usage: recovery buffer
/// memory ≈ recovery_count × slice_size ≈ total_padded × pct/100, which is
/// invariant to slice size for a fixed data set.
pub fn calculate_geometry(
    files: &[InputFile],
    options: &CreateOptions,
) -> Result<(usize, usize, usize)> {
    let total_bytes: u64 = files.iter().map(|f| f.size).sum();
    let effort = options.effort;
    if !(effort.is_finite() && effort > 0.0) {
        anyhow::bail!("PAR2 effort must be a positive number, got {effort}");
    }

    let (slice_size, total_slices) = if let Some(s) = options.slice_size {
        let s = (s / 64 * 64).max(64);
        let n = total_slice_count(files, s);
        (s, n)
    } else if let Some(count) = options.slice_count {
        let s = ((total_bytes as usize).div_ceil(count.max(1)) / 64 * 64).max(64);
        let n = total_slice_count(files, s);
        (s, n)
    } else if let Some(s) = options
        .slice_unit
        .and_then(|unit| aligned_slice_size(files, total_bytes, unit, effort))
    {
        (s, total_slice_count(files, s))
    } else {
        heuristic_geometry(files, total_bytes, effort)
    };

    let recovery_count = if let Some(n) = options.recovery_count {
        n
    } else {
        let proportional = (total_slices * options.recovery_pct as usize) / 100;
        if options.recovery_pct > 0 && total_slices > 0 {
            proportional.max(1)
        } else {
            proportional
        }
    };

    if total_slices > MAX_SLICES {
        anyhow::bail!("too many input slices: {total_slices} (max {MAX_SLICES})");
    }
    if recovery_count > 65535 {
        anyhow::bail!("too many recovery blocks: {recovery_count} (max 65535)");
    }

    Ok((slice_size, total_slices, recovery_count))
}

/// The automatic geometry without a unit: about [`TARGET_SLICES`] × `effort`
/// slices of any multiple of 64 bytes.
fn heuristic_geometry(files: &[InputFile], total_bytes: u64, effort: f64) -> (usize, usize) {
    let target = (TARGET_SLICES as f64 * effort).clamp(1.0, MAX_SLICES as f64) as usize;
    let mut s = ((total_bytes as usize).div_ceil(target).max(64) / 64 * 64).max(64);
    let mut n = total_slice_count(files, s);

    // Grow slice size if we'd exceed the PAR2 hard limit.
    while n > MAX_SLICES {
        let next = s.saturating_mul(2);
        if next == s {
            break;
        }
        let n2 = total_slice_count(files, next);
        // One slice per non-empty file is the floor — growing the slice
        // cannot help a set with more files than the spec allows.
        if n2 >= n {
            break;
        }
        s = next;
        n = n2;
    }

    // Detect small-file padding inflation: many files smaller than slice_size
    // each consume a full slice, inflating the effective parity ratio well
    // beyond what the user requested. Halve the slice size until the ratio
    // is acceptable or we hit MAX_SLICES_PADDING_OPT.
    if total_bytes > 0 && padding_ratio(files, total_bytes, s) > PADDING_RATIO_THRESHOLD {
        loop {
            // Halve, keeping alignment to 64 bytes.
            let s2 = ((s / 2) / 64 * 64).max(64);
            if s2 >= s {
                break; // already at minimum granularity
            }
            let n2 = total_slice_count(files, s2);
            if n2 > MAX_SLICES_PADDING_OPT.min(MAX_SLICES) {
                break; // would cost too much CPU
            }
            s = s2;
            n = n2;
            if padding_ratio(files, total_bytes, s) <= PADDING_RATIO_THRESHOLD {
                break; // good enough
            }
        }
    }

    (s, n)
}

/// The automatic slice size aligned to `unit`, or `None` to fall back to
/// [`heuristic_geometry`].
///
/// The size is `k × unit` for the whole `k ≥ 1` whose slice count is closest,
/// on a log scale, to a budget of [`TARGET_SLICES`] × `effort`. A set smaller
/// than [`FULL_BUDGET_BYTES`] gets the budget scaled up by
/// `FULL_BUDGET_BYTES / total_bytes`, which keeps its total cost (slices ×
/// bytes) about that of a [`FULL_BUDGET_BYTES`] set. Up to about 2.4 GiB at
/// the default effort and a 768000-byte unit, that means one slice per unit,
/// so a lost unit costs exactly one recovery block.
///
/// A unit that is not a multiple of 64 is raised to the least common multiple
/// with 64, which is still a multiple of the unit. Returns `None` for an empty
/// set, a zero unit, or when padding small files to the aligned size would
/// inflate the set by more than [`PADDING_RATIO_THRESHOLD`]: files smaller
/// than a unit gain nothing from alignment, and the heuristic pads them less.
fn aligned_slice_size(
    files: &[InputFile],
    total_bytes: u64,
    unit: usize,
    effort: f64,
) -> Option<usize> {
    if unit == 0 || total_bytes == 0 {
        return None;
    }
    let unit = unit.checked_mul(64 / gcd(unit, 64))?;
    let scale = FULL_BUDGET_BYTES.max(total_bytes) as f64 / total_bytes as f64;
    let budget = (TARGET_SLICES as f64 * effort * scale).clamp(1.0, MAX_SLICES as f64);

    // The slice count falls as `k` grows, down to one slice per non-empty
    // file at `max_k`. It is at least `units / k`, so no `k` below
    // `units / budget` fits the budget. Binary search for the smallest `k`
    // that fits; a set with more files than the budget gets `max_k`.
    let units: usize = files.iter().map(|f| (f.size as usize).div_ceil(unit)).sum();
    let max_k = files
        .iter()
        .map(|f| (f.size as usize).div_ceil(unit))
        .max()
        .unwrap_or(1)
        .max(1);
    max_k.checked_mul(unit)?;
    let slices = |k: usize| total_slice_count(files, k * unit) as f64;
    let (mut lo, mut hi) = (((units as f64 / budget) as usize).clamp(1, max_k), max_k);
    while lo < hi {
        let mid = lo + (hi - lo) / 2;
        if slices(mid) <= budget {
            hi = mid;
        } else {
            lo = mid + 1;
        }
    }
    let mut k = lo;
    if k > 1 && slices(k) <= budget {
        let (below, above) = (slices(k), slices(k - 1));
        if above <= MAX_SLICES as f64 && above / budget < budget / below {
            k -= 1;
        }
    }

    let s = k * unit;
    let fits = slices(k) <= MAX_SLICES as f64;
    (fits && padding_ratio(files, total_bytes, s) <= PADDING_RATIO_THRESHOLD).then_some(s)
}

/// Padded bytes over real bytes at `slice_size`.
fn padding_ratio(files: &[InputFile], total_bytes: u64, slice_size: usize) -> f64 {
    total_padded_bytes(files, slice_size) as f64 / total_bytes as f64
}

fn gcd(mut a: usize, mut b: usize) -> usize {
    while b != 0 {
        (a, b) = (b, a % b);
    }
    a
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_files(sizes: &[u64]) -> Vec<InputFile> {
        sizes
            .iter()
            .enumerate()
            .map(|(i, &size)| InputFile {
                path: format!("file{i}").into(),
                display_name: format!("file{i}"),
                size,
            })
            .collect()
    }

    fn opts() -> CreateOptions {
        CreateOptions::default()
    }

    #[test]
    fn geometry_stays_within_par2_limits() {
        // 100 files of 1 GiB each — stresses the 32k slice ceiling.
        let files = make_files(&vec![1024 * 1024 * 1024; 100]);
        let (_, total_slices, recovery_count) = calculate_geometry(&files, &opts()).unwrap();
        assert!(total_slices <= 32768, "total_slices={total_slices}");
        assert!(recovery_count <= 65535, "recovery_count={recovery_count}");
    }

    #[test]
    fn geometry_reduces_slice_for_many_small_files() {
        // Simulates a Blu-ray disc: 3 large .m2ts + 200 tiny support files.
        let mut sizes: Vec<u64> = vec![5 * 1024 * 1024 * 1024; 3]; // 3 × 5 GiB
        sizes.extend(vec![50 * 1024; 200]); // 200 × 50 KiB

        let files = make_files(&sizes);
        let total_actual: u64 = sizes.iter().sum();

        let (slice_size, total_slices, _) = calculate_geometry(&files, &opts()).unwrap();

        let total_padded: u64 = files
            .iter()
            .map(|f| (f.size as usize).div_ceil(slice_size) as u64 * slice_size as u64)
            .sum();
        let ratio = total_padded as f64 / total_actual as f64;

        assert!(total_slices <= 32768, "total_slices={total_slices}");
        // Padding overhead should be within 15% of actual data.
        assert!(
            ratio <= 1.15,
            "padding ratio {ratio:.3} exceeds threshold (slice_size={slice_size}, slices={total_slices})"
        );
    }

    #[test]
    fn geometry_does_not_over_optimize_clean_sets() {
        // A single large file has negligible padding at the heuristic slice size —
        // the optimiser should leave the slice size alone.
        let files = make_files(&[8 * 1024 * 1024 * 1024]);
        let (_, total_slices, _) = calculate_geometry(&files, &opts()).unwrap();
        // Should stay near ~2000 slices, not balloon to 32768.
        assert!(total_slices <= 6_000, "total_slices={total_slices}");
    }

    #[test]
    fn explicit_slice_size_is_respected() {
        let files = make_files(&[100 * 1024 * 1024]);
        let mut o = opts();
        o.slice_size = Some(512 * 1024);
        let (slice_size, _, _) = calculate_geometry(&files, &o).unwrap();
        assert_eq!(slice_size, 512 * 1024);
    }

    #[test]
    fn positive_percentage_produces_recovery_for_tiny_set() {
        let files = make_files(&[1]);
        let mut o = opts();
        o.recovery_pct = 1;
        let (_, total_slices, recovery_count) = calculate_geometry(&files, &o).unwrap();
        assert_eq!(total_slices, 1);
        assert_eq!(recovery_count, 1);
    }

    #[test]
    fn explicit_zero_recovery_count_remains_zero() {
        let files = make_files(&[1]);
        let mut o = opts();
        o.recovery_pct = 10;
        o.recovery_count = Some(0);
        let (_, _, recovery_count) = calculate_geometry(&files, &o).unwrap();
        assert_eq!(recovery_count, 0);
    }

    const ARTICLE: usize = 768_000;

    fn with_unit(unit: usize) -> CreateOptions {
        CreateOptions {
            slice_unit: Some(unit),
            ..CreateOptions::default()
        }
    }

    const GIB: u64 = 1024 * 1024 * 1024;
    const MIB: u64 = 1024 * 1024;

    fn slices_at(sizes: &[u64], options: &CreateOptions) -> (usize, usize) {
        let (slice_size, total_slices, _) =
            calculate_geometry(&make_files(sizes), options).unwrap();
        (slice_size, total_slices)
    }

    #[test]
    fn unit_gives_small_sets_one_slice_per_unit() {
        for sizes in [vec![300 * MIB], vec![1200 * MIB], vec![2 * GIB]] {
            let (slice_size, total_slices) = slices_at(&sizes, &with_unit(ARTICLE));
            assert_eq!(slice_size, ARTICLE, "{sizes:?}");
            assert_eq!(
                total_slices,
                (sizes[0] as usize).div_ceil(ARTICLE),
                "{sizes:?}"
            );
        }
    }

    #[test]
    fn unit_keeps_large_sets_near_the_slice_target() {
        // 8389 articles: 4 per slice gives 2098 slices, closer to 2000 on a
        // log scale than 5 per slice (1678).
        assert_eq!(
            slices_at(&[6 * GIB], &with_unit(ARTICLE)),
            (4 * ARTICLE, 2098)
        );
        let (slice_size, total_slices) = slices_at(&[40 * GIB], &with_unit(ARTICLE));
        assert!(slice_size.is_multiple_of(ARTICLE));
        assert!((1800..=2200).contains(&total_slices), "{total_slices}");
    }

    #[test]
    fn unit_never_costs_more_than_a_full_budget_set() {
        // Cost ∝ slices × bytes. Below 4 GiB the budget grows as the set
        // shrinks, so the total never exceeds that of a 4 GiB set at the
        // target, give or take the log-scale rounding of one step of k.
        let full = TARGET_SLICES as f64 * FULL_BUDGET_BYTES as f64;
        for mib in [100, 700, 1500, 2048, 2600, 3072, 3500, 4096, 6144, 20480] {
            let (_, total_slices) = slices_at(&[mib * MIB], &with_unit(ARTICLE));
            let cost = total_slices as f64 * (mib * MIB) as f64;
            let ceiling = full.max(TARGET_SLICES as f64 * (mib * MIB) as f64);
            assert!(cost <= 1.5 * ceiling, "{mib} MiB: {total_slices} slices");
        }
    }

    #[test]
    fn unit_slices_are_whole_multiples_of_the_unit() {
        let sets: [&[u64]; 4] = [
            &[1200 * MIB; 8],
            &[500 * MIB; 6],
            &[3 * GIB, 7 * GIB, 123_456_789],
            &[1024 * MIB; 100],
        ];
        for sizes in sets {
            let (slice_size, total_slices) = slices_at(sizes, &with_unit(ARTICLE));
            assert!(slice_size.is_multiple_of(ARTICLE), "{slice_size}");
            assert!(total_slices <= MAX_SLICES);
        }
    }

    #[test]
    fn unit_keeps_alignment_with_a_few_small_files() {
        let mut sizes: Vec<u64> = vec![5 * GIB; 3];
        sizes.extend(vec![50 * 1024; 200]);
        let total_actual: u64 = sizes.iter().sum();
        let (slice_size, _) = slices_at(&sizes, &with_unit(ARTICLE));
        assert!(slice_size.is_multiple_of(ARTICLE), "{slice_size}");
        let files = make_files(&sizes);
        assert!(padding_ratio(&files, total_actual, slice_size) <= PADDING_RATIO_THRESHOLD);
    }

    #[test]
    fn unit_handles_more_files_than_the_slice_budget() {
        // More files than the budget allows: no aligned size fits, so the
        // search must stop at one slice per file instead of looping.
        let sizes = vec![ARTICLE as u64; 50_000];
        assert!(calculate_geometry(&make_files(&sizes), &with_unit(ARTICLE)).is_err());

        // Here the only aligned size within budget pads every small file to
        // the large one's size, so the unaligned heuristic is used instead.
        let mut sizes = vec![ARTICLE as u64; 5_000];
        sizes.push(10_000 * ARTICLE as u64);
        assert_eq!(
            slices_at(&sizes, &with_unit(ARTICLE)),
            slices_at(&sizes, &opts())
        );
    }

    #[test]
    fn unit_falls_back_when_small_files_would_pad_the_set() {
        let sizes = vec![256 * 1024; 2000];
        assert_eq!(
            slices_at(&sizes, &with_unit(ARTICLE)),
            slices_at(&sizes, &opts())
        );
    }

    #[test]
    fn unit_not_a_multiple_of_64_aligns_to_the_common_multiple() {
        // gcd(700000, 64) = 32, so slices are multiples of 1 400 000.
        let (slice_size, _) = slices_at(&[300 * MIB], &with_unit(700_000));
        assert_eq!(slice_size, 1_400_000);
        let (slice_size, _) = slices_at(&[6 * GIB], &with_unit(700_001));
        assert!(slice_size.is_multiple_of(700_001 * 64));
    }

    #[test]
    fn effort_scales_the_slice_budget() {
        let doubled = CreateOptions {
            effort: 2.0,
            ..with_unit(ARTICLE)
        };
        assert_eq!(slices_at(&[6 * GIB], &doubled), (2 * ARTICLE, 4195));
        let doubled = CreateOptions {
            effort: 2.0,
            ..opts()
        };
        let (_, total_slices) = slices_at(&[6 * GIB], &doubled);
        assert!((4000..=4001).contains(&total_slices), "{total_slices}");
    }

    #[test]
    fn effort_must_be_positive() {
        for effort in [0.0, -1.0, f64::NAN, f64::INFINITY] {
            let o = CreateOptions { effort, ..opts() };
            assert!(
                calculate_geometry(&make_files(&[1]), &o).is_err(),
                "{effort}"
            );
        }
    }

    #[test]
    fn explicit_slice_size_overrides_the_unit() {
        let files = make_files(&[100 * 1024 * 1024]);
        let mut o = with_unit(ARTICLE);
        o.slice_size = Some(512 * 1024);
        let (slice_size, _, _) = calculate_geometry(&files, &o).unwrap();
        assert_eq!(slice_size, 512 * 1024);
    }
}
