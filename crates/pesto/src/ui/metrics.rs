//! Pure progress-rate and remaining-time calculations.

use std::time::Instant;

pub(super) fn elapsed_secs(start: Instant) -> f64 {
    start.elapsed().as_secs_f64().max(0.001)
}

pub(super) fn progress_fraction(done: u64, total: u64) -> f64 {
    if total == 0 {
        0.0
    } else {
        (done as f64 / total as f64).clamp(0.0, 1.0)
    }
}

pub(super) fn bytes_per_second(done_bytes: u64, elapsed_secs: f64) -> f64 {
    done_bytes as f64 / elapsed_secs.max(0.001)
}

pub(super) fn ordered_samples<const N: usize>(
    history: &[f64; N],
    next: usize,
    len: usize,
) -> Vec<f64> {
    let len = len.min(N);
    if len == 0 {
        return Vec::new();
    }
    let start = if len < N { 0 } else { next % N };
    (0..len).map(|index| history[(start + index) % N]).collect()
}

/// Return `(low seconds, high seconds, unstable)` from recent throughput.
pub(super) fn eta_range(remaining_bytes: u64, samples: &[f64]) -> Option<(f64, f64, bool)> {
    if remaining_bytes == 0 || samples.is_empty() {
        return None;
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    if mean < 1.0 {
        return None;
    }
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    let sigma = variance.sqrt();
    let variation = sigma / mean;
    let remaining = remaining_bytes as f64;
    let midpoint = remaining / mean;
    if variation < 0.1 {
        return Some((midpoint, midpoint, false));
    }
    let low = remaining / (mean + sigma).max(1.0);
    let high = (remaining / (mean - sigma).max(1.0)).min(low * 10.0);
    Some((low, high, variation >= 0.3))
}

pub(super) fn remaining_secs(done: u64, total: u64, rate: f64) -> Option<f64> {
    if total == 0 || done >= total || rate <= 0.01 {
        None
    } else {
        Some((total - done) as f64 / rate)
    }
}

pub(super) fn overall_eta(
    upload_range: Option<(f64, f64, bool)>,
    upload_fallback: Option<f64>,
    phase_estimates: impl IntoIterator<Item = Option<f64>>,
) -> Option<(f64, bool)> {
    let (mut best, unstable) = upload_range
        .map(|(_, high, unstable)| (Some(high), unstable))
        .unwrap_or((upload_fallback, false));
    for estimate in phase_estimates.into_iter().flatten() {
        best = Some(best.map_or(estimate, |current| current.max(estimate)));
    }
    best.map(|seconds| (seconds, unstable))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn progress_fraction_is_bounded_and_handles_empty_work() {
        assert_eq!(progress_fraction(0, 0), 0.0);
        assert_eq!(progress_fraction(5, 10), 0.5);
        assert_eq!(progress_fraction(11, 10), 1.0);
    }

    #[test]
    fn ordered_samples_unrolls_a_full_ring_from_the_oldest_slot() {
        assert_eq!(
            ordered_samples(&[30.0, 40.0, 10.0, 20.0], 2, 4),
            vec![10.0, 20.0, 30.0, 40.0]
        );
    }

    #[test]
    fn overall_eta_uses_the_slowest_concurrent_phase() {
        assert_eq!(
            overall_eta(Some((8.0, 10.0, false)), None, [Some(12.0), Some(4.0)]),
            Some((12.0, false))
        );
    }
}
