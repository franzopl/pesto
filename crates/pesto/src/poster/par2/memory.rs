//! PAR2 memory budgeting: address-space-derived per-pass limits.

use anyhow::{bail, Result};

use crate::config::Config;

/// This process's own virtual address-space ceiling — see
/// [`crate::memory::address_space_limit`], which owns the implementation now
/// that startup tuning needs it too.
pub(crate) fn address_space_limit() -> Option<u64> {
    crate::memory::address_space_limit()
}

/// Rough reservation for the process overhead that `par2_memory_limit`
/// doesn't account for — per-connection TLS/article buffers, the tokio
/// runtime, the check-connection pool — so the PAR2 pass doesn't get sized
/// right up to the address-space ceiling and starve everything else that
/// also has to fit inside it.
pub(crate) fn connection_overhead_reserve(connections: usize, threads: usize) -> u64 {
    const PER_CONNECTION: u64 = 8 * 1024 * 1024; // generous: TLS + article buffers
                                                 // Was 32 MiB, chosen before anything measured per-thread cost. A live musl
                                                 // run on a 128-core seedbox reserved 2.0 GiB under that figure (64 threads)
                                                 // for stacks whose real size, after `crate::memory` bounded them, is 1 MiB
                                                 // — a ~32x over-estimate, and 69% of the entire reserve. 4 MiB keeps 3 MiB
                                                 // of headroom per thread for SIMD/GF16 scratch on top of the measured
                                                 // stack. Over-reserving is not free: the budget below is derived from what
                                                 // this leaves, so every wasted GiB here costs PAR2 budget and can force an
                                                 // extra pass — and each pass is another full read of the input.
    const PER_THREAD: u64 = 4 * 1024 * 1024; // stack (measured 1 MiB) + scratch
                                             // Was 512 MiB. Raised after a completed 83.4 GiB / 116 619-segment musl run
                                             // showed the process's true peak is *not* in the PAR2 passes: those topped
                                             // out at 7.38 GiB, then the tail of the run (final posting, the accumulated
                                             // `results` vector, the check queue's heap, NZB assembly) added another
                                             // 0.75 GiB to reach 8.13 GiB. None of that is PAR2 budget, so it belongs
                                             // here — sizing the budget as if the passes were the high-water mark
                                             // understates the real ceiling pressure by exactly that much.
                                             //
                                             // This term scales with segment count in reality; a flat 1 GiB covers the
                                             // ~116 k-segment case measured. Phase 1's sampler should replace it with a
                                             // real per-segment figure (see docs/memory-management.md).
    const BASELINE: u64 = 1024 * 1024 * 1024; // runtime, results/NZB/check tail
    BASELINE + (connections as u64) * PER_CONNECTION + (threads as u64) * PER_THREAD
}

/// Share of the address-space ceiling the whole process is allowed to reach.
///
/// `RLIMIT_AS` is a hard, zero-tolerance wall — one allocation across it aborts
/// via `handle_alloc_error`, and with `panic = "abort"` nothing unwinds far
/// enough to log why. The margin is deliberately wide.
const CEILING_TARGET: f64 = 0.85;

/// A pass's real working set as a multiple of its memory budget.
///
/// The budget sizes the recovery buffers; on top of those the encoder gets a
/// flush queue of `memory_limit / 4` (see the `queue_limit` computation in
/// `producer`), so a pass actually occupies ~1.25x what it is budgeted.
const PASS_WORKING_SET_FACTOR: f64 = 1.25;

/// Share of a finished pass's working set still held by the allocator when the
/// next pass allocates.
///
/// The pass loop drops each `Par2Worker` before creating the next, so this is
/// not a leak — it is musl's allocator not returning freed spans to the OS, and
/// the next pass's differently-shaped allocations not fitting the holes left
/// behind. `RLIMIT_AS` counts the retained mapping regardless.
///
/// Measured on a live 75.8 GiB / 3-pass musl run: VmPeak went 4.98 GiB during
/// pass 1 to 7.15 GiB during pass 2, a 2.17 GiB step against a 4.15 GiB pass
/// working set — 52% retained. Rounded up to 0.55.
///
/// This is the term the old formula omitted entirely, and omitting it is why
/// the over-sized `PER_THREAD` above was load-bearing: two errors cancelled.
/// Correcting only one of them would have raised the budget to ~4.2 GiB/pass
/// and pushed predicted peak use to ~92% of the ceiling.
const CROSS_PASS_RETENTION: f64 = 0.55;

/// Safe per-pass PAR2 budget, derived from this process's own `RLIMIT_AS`
/// rather than host/cgroup RAM (see [`address_space_limit`]).
///
/// The model is:
///
/// ```text
/// peak ≈ reserve + budget × PASS_WORKING_SET_FACTOR × (1 + retention)
/// ```
///
/// where `retention` is [`CROSS_PASS_RETENTION`] for a multi-pass run and zero
/// for a single-pass one — a run that never starts a second pass cannot be
/// holding a first pass's memory. Solving for `budget` under
/// `peak ≤ ceiling × CEILING_TARGET` gives the two branches below.
///
/// Splitting the single-pass case out matters: it is both the common case and
/// the one the old flat 50% penalised hardest. On a 9.5 GiB ceiling the budget
/// for a single-pass run goes from 3.3 GiB to ~5.6 GiB, which is itself the
/// cheapest way to *avoid* multi-pass runs — and every pass avoided is one
/// less full read of the input.
///
/// `recovery_count == 0` (no PAR2 requested) takes the single-pass branch; the
/// budget is unused in that case but must still be a sane number.
fn address_space_budget(reserve: u64, slice_size: usize, recovery_count: usize) -> Option<u64> {
    let as_limit = address_space_limit()?;
    let headroom = (as_limit as f64 * CEILING_TARGET) - reserve as f64;
    if headroom <= 0.0 {
        // The reserve alone already exceeds the target. Return 0 and let the
        // caller surface it — silently handing back a tiny budget here would
        // produce thousands of passes instead of an actionable error.
        return Some(0);
    }

    let single_pass = headroom / PASS_WORKING_SET_FACTOR;
    let fits_in_one_pass = slice_size == 0
        || recovery_count == 0
        || (single_pass as u64) / (slice_size as u64) >= recovery_count as u64;
    if fits_in_one_pass {
        return Some(single_pass as u64);
    }

    Some((headroom / (PASS_WORKING_SET_FACTOR * (1.0 + CROSS_PASS_RETENTION))) as u64)
}

/// Shared PAR2 memory budget + pass list used by the per-file producer and
/// the season path so `--memory-limit` cannot drift between them.
pub(crate) fn par2_memory_plan(
    config: &Config,
    par2_slice_size: usize,
    recovery_count: usize,
    active_connections: usize,
) -> Result<(usize, Vec<(u32, usize)>)> {
    let reserve_threads = if config.threads > 0 {
        config.threads
    } else {
        parmesan::performance_core_count()
    };
    let overhead_reserve = connection_overhead_reserve(active_connections, reserve_threads);
    let as_budget = address_space_budget(overhead_reserve, par2_slice_size, recovery_count);
    if as_budget == Some(0) && recovery_count > 0 {
        bail!(
            "not enough address space to generate PAR2: this session's limit \
             (RLIMIT_AS = {}) is already exceeded by the ~{} reserved for {} \
             connections and {} PAR2 threads. Lower --connections/--threads, \
             disable PAR2 with --par2 0, or raise `ulimit -v` for this session.",
            crate::progress::format_size(address_space_limit().unwrap_or_default()),
            crate::progress::format_size(overhead_reserve),
            active_connections,
            reserve_threads,
        );
    }
    let ceiling = crate::memory::Ceiling::discover(config.memory_limit);
    let non_as_par2_share = crate::memory::budget::share_of(
        ceiling.effective_excluding_address_space(),
        crate::memory::budget::Stage::Par2,
    );
    let binding_budget = as_budget.map_or(non_as_par2_share, |b| b.min(non_as_par2_share));

    let memory_limit = match config.par2_memory_limit {
        Some(limit) => {
            if limit as u64 > binding_budget {
                let bound_by_as = as_budget.is_some_and(|b| b <= non_as_par2_share);
                bail!(
                    "--par2-memory-limit {} won't fit safely: {} leaves a safe budget of \
                     only {} once ~{} is reserved for {} connections and {} PAR2 threads. \
                     Lower --par2-memory-limit (or --memory-limit / --connections/--threads), \
                     or {}.",
                    crate::progress::format_size(limit as u64),
                    if bound_by_as {
                        format!(
                            "this session's address-space limit (RLIMIT_AS = {})",
                            crate::progress::format_size(address_space_limit().unwrap_or_default())
                        )
                    } else {
                        format!(
                            "the global --memory-limit budget (effective ceiling {})",
                            crate::progress::format_size(ceiling.effective)
                        )
                    },
                    crate::progress::format_size(binding_budget),
                    crate::progress::format_size(overhead_reserve),
                    active_connections,
                    reserve_threads,
                    if bound_by_as {
                        "raise `ulimit -v` for this session"
                    } else {
                        "raise --memory-limit"
                    },
                );
            }
            limit
        }
        None => (binding_budget as usize).max(256 * 1024 * 1024),
    };

    let slices_per_pass = (memory_limit / par2_slice_size.max(1)).max(1);
    let mut passes = Vec::new();
    if recovery_count > 0 {
        let mut start = 0;
        while start < recovery_count {
            let count = (recovery_count - start).min(slices_per_pass);
            passes.push((start as u32, count));
            start += count;
        }
    } else {
        passes.push((0, 0));
    }
    Ok((memory_limit, passes))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let per_thread = (connection_overhead_reserve(0, threads)
            - connection_overhead_reserve(0, 0))
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
        if slice_size == 0
            || recovery == 0
            || (single as u64) / (slice_size as u64) >= recovery as u64
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
}
