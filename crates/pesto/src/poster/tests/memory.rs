use super::*;

#[test]
fn message_id_domain_is_random() {
    let a = crate::article::generate_message_id(None);
    let b = crate::article::generate_message_id(None);
    assert_ne!(a, b);
    assert!(a.contains('@'));
    assert!(!a.contains("blocknews") && !a.contains("pesto"));
}

// ── address_space_limit / connection_overhead_reserve ────────────────────

#[test]
fn address_space_limit_does_not_panic() {
    // Value is whatever the test host's `ulimit -v` happens to be — just
    // assert the call is sane, not a specific number.
    if let Some(limit) = address_space_limit() {
        assert!(limit > 0);
    }
}

#[test]
fn connection_overhead_reserve_scales_with_connections_and_threads() {
    let base = connection_overhead_reserve(0, 0);
    assert_eq!(base, 1024 * 1024 * 1024);

    let with_200_conns = connection_overhead_reserve(200, 0);
    assert_eq!(with_200_conns, base + 200 * 8 * 1024 * 1024);
    assert!(with_200_conns > base);

    let with_threads = connection_overhead_reserve(0, 128);
    assert_eq!(with_threads, base + 128 * 4 * 1024 * 1024);
    assert!(with_threads > base);
}

#[test]
fn per_thread_reserve_reflects_measured_stack_size() {
    // Regression guard for the constant that made the old formula wrong:
    // 128 threads used to reserve 4 GiB for stacks that measure 1 MiB
    // each. Anyone raising this again should have measurements in hand.
    let threads = 128usize;
    let per_thread = (connection_overhead_reserve(0, threads) - connection_overhead_reserve(0, 0))
        / threads as u64;
    assert_eq!(per_thread, 4 * 1024 * 1024);
    assert!(
        per_thread <= 8 * 1024 * 1024,
        "per-thread reserve drifted back up; it directly costs PAR2 budget"
    );
}

/// Reproduce `address_space_budget`'s arithmetic for a given ceiling.
///
/// The real function reads `RLIMIT_AS`, which cannot be faked without
/// mutating process-global state shared with every other test, so the
/// model is exercised here against the same constants.
fn budget_for(ceiling: u64, reserve: u64, slice_size: usize, recovery: usize) -> u64 {
    let headroom = (ceiling as f64 * CEILING_TARGET) - reserve as f64;
    if headroom <= 0.0 {
        return 0;
    }
    let single = headroom / PASS_WORKING_SET_FACTOR;
    if slice_size == 0 || recovery == 0 || (single as u64) / (slice_size as u64) >= recovery as u64
    {
        return single as u64;
    }
    (headroom / (PASS_WORKING_SET_FACTOR * (1.0 + CROSS_PASS_RETENTION))) as u64
}

#[test]
fn single_pass_budget_beats_multi_pass_budget() {
    let ceiling = 10 * 1024 * 1024 * 1024u64;
    let reserve = 1024 * 1024 * 1024u64;
    let slice = 40 * 1024 * 1024usize;

    // 4 recovery blocks fit in one pass; 4096 cannot.
    let single = budget_for(ceiling, reserve, slice, 4);
    let multi = budget_for(ceiling, reserve, slice, 4096);
    assert!(
        single > multi,
        "a single-pass run pays no retention cost and must get the larger budget"
    );
    // The multi-pass branch is exactly the retention discount.
    let expected = (single as f64 / (1.0 + CROSS_PASS_RETENTION)) as u64;
    assert!(multi.abs_diff(expected) < 1024 * 1024);
}

#[test]
fn multi_pass_peak_stays_under_the_ceiling() {
    // The whole point of the model: budget x working-set x (1 + retention),
    // plus the reserve, must land under the ceiling. This is the assertion
    // the old flat-50% formula could not make, because it had no term for
    // what a finished pass leaves behind.
    let ceiling = 10 * 1024 * 1024 * 1024u64;
    let reserve = 1024 * 1024 * 1024u64;
    let budget = budget_for(ceiling, reserve, 40 * 1024 * 1024, 4096);

    let predicted_peak =
        reserve as f64 + budget as f64 * PASS_WORKING_SET_FACTOR * (1.0 + CROSS_PASS_RETENTION);
    assert!(
        predicted_peak <= ceiling as f64 * CEILING_TARGET + 1.0,
        "predicted peak {predicted_peak} exceeds the {CEILING_TARGET} target of {ceiling}"
    );
    assert!(predicted_peak < ceiling as f64);
}

#[test]
fn budget_is_zero_when_reserve_swallows_the_ceiling() {
    // Degenerate case: a tiny `ulimit -v` with many threads/connections.
    // Must not wrap around or return a microscopic budget that would imply
    // thousands of passes — the caller surfaces zero as an error.
    let ceiling = 512 * 1024 * 1024u64;
    let reserve = 4 * 1024 * 1024 * 1024u64;
    assert_eq!(budget_for(ceiling, reserve, 40 * 1024 * 1024, 100), 0);
}
