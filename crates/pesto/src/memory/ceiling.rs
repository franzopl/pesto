//! Discover the effective memory ceiling: the tightest of everything that
//! can kill this process, each with its own haircut for how much margin its
//! failure mode deserves.
//!
//! `RLIMIT_AS`, cgroup `memory.max`, and host RAM fail differently — crossing
//! `RLIMIT_AS` aborts instantly with zero reclaim, crossing the cgroup limit
//! triggers reclaim before the cgroup OOM killer, and host RAM is the
//! softest of the three — so each keeps a different margin rather than one
//! flat percentage applied to whichever is smallest.

use super::cgroup;

/// `RLIMIT_AS` is a hard, zero-tolerance wall: one allocation across it
/// aborts via `handle_alloc_error`, and with `panic = "abort"` in the release
/// profile nothing unwinds far enough to log why. It's also consumed by
/// things this process doesn't fully control (allocator arena reservations —
/// see [`super::tune_allocator`]), so the margin is the widest of the three.
const ADDRESS_SPACE_HAIRCUT: f64 = 0.60;

/// Crossing the cgroup limit triggers kernel reclaim before the cgroup OOM
/// killer steps in, so there's some slack — less margin needed than
/// `RLIMIT_AS`.
const CGROUP_HAIRCUT: f64 = 0.75;

/// Host RAM is the softest constraint of the three (other processes,
/// buffers/cache the kernel will happily reclaim), matching the margin
/// `poster::producer`'s existing RAM auto-detection already uses.
const HOST_HAIRCUT: f64 = 0.70;

/// The effective memory budget for this process: the minimum of every
/// source that can constrain it, each already haircut for its own failure
/// mode. A user-supplied `explicit` ceiling is taken at face value — a
/// deliberately chosen number doesn't need a haircut on top of itself.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ceiling {
    /// Raw `RLIMIT_AS`, before the haircut. `None` if unlimited.
    pub address_space: Option<u64>,
    /// Raw cgroup `memory.max`, before the haircut. `None` if unconfined or
    /// unlimited.
    pub cgroup_max: Option<u64>,
    /// Raw total host RAM, before the haircut. Always available.
    pub host_total: u64,
    /// A user-supplied global ceiling, taken as-is. Reserved for Phase 2's
    /// global `--memory-limit`; nothing populates this in Phase 1 yet — see
    /// `docs/memory-management.md`.
    pub explicit: Option<u64>,
    /// The binding budget: the minimum of the haircut sources above.
    pub effective: u64,
}

impl Ceiling {
    /// Discover the ceiling from the current process/host state.
    ///
    /// Cheap (a `getrlimit` call, a handful of small file reads, one
    /// `sysinfo` refresh) and deliberately not cached — callers just call it
    /// again when they need a fresh reading, the same pattern
    /// [`super::address_space_limit`] and [`super::address_space_peak`]
    /// already use.
    pub fn discover(explicit: Option<u64>) -> Self {
        let address_space = super::address_space_limit();
        let cgroup_max = cgroup::read_cgroup_memory().and_then(|c| c.max);
        let host_total = host_total_memory();
        let effective = effective_ceiling(address_space, cgroup_max, host_total, explicit);
        Self {
            address_space,
            cgroup_max,
            host_total,
            explicit,
            effective,
        }
    }

    /// [`Self::effective`], but without the `RLIMIT_AS` source.
    ///
    /// For callers that already have their own RLIMIT_AS-specific budget
    /// model — PAR2's pass sizing (`poster::address_space_budget`), tuned and
    /// validated against a live 83.4 GiB run — and only want this struct's
    /// *other* sources (cgroup/host/explicit) as an additional cap. Using
    /// [`Self::effective`] there would haircut `RLIMIT_AS` twice: once here
    /// (`× 0.60`), again as that caller's own stage share on top, silently
    /// shrinking the budget far below what either model alone would produce.
    pub fn effective_excluding_address_space(&self) -> u64 {
        effective_ceiling(None, self.cgroup_max, self.host_total, self.explicit)
    }
}

fn host_total_memory() -> u64 {
    let mut sys = sysinfo::System::new();
    sys.refresh_memory();
    sys.total_memory()
}

fn effective_ceiling(
    address_space: Option<u64>,
    cgroup_max: Option<u64>,
    host_total: u64,
    explicit: Option<u64>,
) -> u64 {
    let mut candidates = Vec::with_capacity(4);
    if let Some(v) = address_space {
        candidates.push(haircut(v, ADDRESS_SPACE_HAIRCUT));
    }
    if let Some(v) = cgroup_max {
        candidates.push(haircut(v, CGROUP_HAIRCUT));
    }
    // Host RAM always contributes — it's the one source that's never
    // `None` — so `candidates` is never empty and the `unwrap_or` below is
    // unreachable in practice, just a defensive floor.
    candidates.push(haircut(host_total, HOST_HAIRCUT));
    if let Some(v) = explicit {
        candidates.push(v);
    }
    candidates.into_iter().min().unwrap_or(host_total)
}

fn haircut(value: u64, fraction: f64) -> u64 {
    (value as f64 * fraction) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_does_not_panic_and_host_total_is_always_present() {
        let ceiling = Ceiling::discover(None);
        assert!(ceiling.host_total > 0);
        assert!(ceiling.effective > 0);
    }

    #[test]
    fn effective_ceiling_takes_the_tightest_haircut_source() {
        let as_limit = 10 * 1024 * 1024 * 1024u64; // 10 GiB, haircut -> 6 GiB
        let cgroup_limit = 4 * 1024 * 1024 * 1024u64; // 4 GiB, haircut -> 3 GiB
        let host = 64 * 1024 * 1024 * 1024u64; // 64 GiB, haircut -> ~44.8 GiB
        let effective = effective_ceiling(Some(as_limit), Some(cgroup_limit), host, None);
        assert_eq!(effective, haircut(cgroup_limit, CGROUP_HAIRCUT));
    }

    #[test]
    fn missing_sources_are_simply_excluded() {
        let host = 8 * 1024 * 1024 * 1024u64;
        let effective = effective_ceiling(None, None, host, None);
        assert_eq!(effective, haircut(host, HOST_HAIRCUT));
    }

    #[test]
    fn explicit_ceiling_is_not_haircut_and_can_win() {
        let host = 64 * 1024 * 1024 * 1024u64;
        let explicit = 2 * 1024 * 1024 * 1024u64; // 2 GiB, deliberately tight
        let effective = effective_ceiling(None, None, host, Some(explicit));
        assert_eq!(effective, explicit);
    }

    #[test]
    fn explicit_does_not_win_if_a_haircut_source_is_still_tighter() {
        let as_limit = 1024 * 1024 * 1024u64; // 1 GiB, haircut -> 0.6 GiB
        let host = 64 * 1024 * 1024 * 1024u64;
        let explicit = 2 * 1024 * 1024 * 1024u64;
        let effective = effective_ceiling(Some(as_limit), None, host, Some(explicit));
        assert_eq!(effective, haircut(as_limit, ADDRESS_SPACE_HAIRCUT));
    }

    #[test]
    fn effective_excluding_address_space_ignores_a_tight_as_limit() {
        // A tiny RLIMIT_AS would otherwise dominate `effective`; the
        // AS-excluding variant must fall through to the next source instead
        // of returning 0 or the AS-haircut value.
        let ceiling = Ceiling {
            address_space: Some(1024 * 1024 * 1024), // 1 GiB — would haircut to 0.6 GiB
            cgroup_max: None,
            host_total: 64 * 1024 * 1024 * 1024,
            explicit: None,
            effective: 0, // not under test here
        };
        let host_share = haircut(ceiling.host_total, HOST_HAIRCUT);
        assert_eq!(ceiling.effective_excluding_address_space(), host_share);
        assert!(
            ceiling.effective_excluding_address_space()
                > haircut(1024 * 1024 * 1024, ADDRESS_SPACE_HAIRCUT),
            "excluding AS must not silently fall back to the AS-haircut value"
        );
    }
}
