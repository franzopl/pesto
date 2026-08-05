//! Stage shares of the effective ceiling (see [`super::Ceiling`]).
//!
//! Splitting the ceiling into named shares is what makes it possible to talk
//! about "the budget" as one coherent number instead of PAR2 being the only
//! managed stage and everything else being unmanaged (see
//! `docs/memory-management.md` §4.2 and §8). The shares are **ceilings, not
//! reservations**: an idle stage's share is lendable, nothing actually
//! reserves it. Only [`Stage::Par2`] is consumed anywhere today — the others
//! are defined now so Phase 3's backpressure has them ready without another
//! round of bikeshedding the split.

/// A budget consumer sharing the process's effective ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// The largest single consumer — PAR2 recovery-encoding buffers.
    Par2,
    /// In-flight article bodies across the connection pool
    /// (`connections × pipeline_depth × article_size`).
    Upload,
    /// The streaming check queue's per-server heaps and repost read buffers.
    Check,
    /// Everything not attributed to a stage: the tokio runtime itself,
    /// `results`/NZB assembly, and general fragmentation slack.
    Reserve,
}

impl Stage {
    fn share(self) -> f64 {
        match self {
            Stage::Par2 => 0.60,
            Stage::Upload => 0.25,
            Stage::Check => 0.10,
            Stage::Reserve => 0.05,
        }
    }
}

/// This stage's share of `ceiling`, in bytes.
pub fn share_of(ceiling: u64, stage: Stage) -> u64 {
    (ceiling as f64 * stage.share()) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shares_sum_to_one() {
        let total: f64 = [Stage::Par2, Stage::Upload, Stage::Check, Stage::Reserve]
            .iter()
            .map(|s| s.share())
            .sum();
        assert!(
            (total - 1.0).abs() < 1e-9,
            "shares must partition the ceiling: {total}"
        );
    }

    #[test]
    fn par2_gets_the_largest_share() {
        let ceiling = 10 * 1024 * 1024 * 1024u64;
        assert_eq!(share_of(ceiling, Stage::Par2), 6 * 1024 * 1024 * 1024);
        assert!(share_of(ceiling, Stage::Par2) > share_of(ceiling, Stage::Upload));
        assert!(share_of(ceiling, Stage::Upload) > share_of(ceiling, Stage::Check));
        assert!(share_of(ceiling, Stage::Check) > share_of(ceiling, Stage::Reserve));
    }

    #[test]
    fn share_of_zero_ceiling_is_zero() {
        assert_eq!(share_of(0, Stage::Par2), 0);
    }
}
