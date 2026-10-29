//! PAR2 recovery-set planning: geometry, memory budget, ingestion and season
//! sets. Kept separate from the posting orchestrator so the numbers that size
//! the encoder do not require reading the hot path.

pub(super) mod geometry;
pub(super) mod memory;
mod season;

pub(crate) use geometry::par2_geometry;
pub(crate) use memory::{
    address_space_limit, connection_overhead_reserve, flush_queue_limit, par2_memory_plan,
};
pub use season::{generate_and_write_season_par2, generate_and_write_season_par2_with_progress};

/// Most PAR2 compute threads pesto starts on its own.
///
/// Measured on a shared 64-core EPYC 7742 seedbox, a 2 GiB file at the
/// default geometry encodes no faster on 64 threads than on 16 (5.2 s against
/// 5.0 s) but burns twice the CPU (91 s against 46 s), taken from neighbours
/// and from pesto's own TLS and yEnc work. 16 threads already encode at about
/// 410 MB/s, well above any seedbox uplink, and PAR2 runs alongside the
/// upload. An explicit `--threads` is not capped.
const DEFAULT_MAX_PAR2_THREADS: usize = 16;

/// PAR2 compute threads for a configured count, where 0 means automatic: one
/// per usable physical core, at most [`DEFAULT_MAX_PAR2_THREADS`].
pub(crate) fn par2_threads(configured: usize) -> usize {
    if configured > 0 {
        configured
    } else {
        parmesan::performance_core_count().min(DEFAULT_MAX_PAR2_THREADS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn par2_threads_caps_only_the_automatic_count() {
        assert_eq!(par2_threads(64), 64);
        assert_eq!(par2_threads(3), 3);
        let auto = par2_threads(0);
        assert!((1..=DEFAULT_MAX_PAR2_THREADS).contains(&auto));
        assert!(auto <= parmesan::performance_core_count());
    }
}
