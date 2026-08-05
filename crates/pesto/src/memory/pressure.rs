//! Pressure level state machine: computed and logged, not yet acted on.
//!
//! This is deliberately Phase 1 scope only — see
//! `docs/memory-management.md` §5. Nothing here throttles connections or
//! pauses PAR2 (that's Phase 3); the value right now is *knowing* when a run
//! is under pressure, before backpressure policy gets written against real
//! numbers.

use std::time::{Duration, Instant};

/// A coarse pressure level, evaluated against the [`super::Ceiling`] and PSI.
///
/// Ordered so escalation/de-escalation can compare levels directly
/// (`Emergency > Critical > Elevated > Normal`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Pressure {
    Normal = 0,
    Elevated = 1,
    Critical = 2,
    Emergency = 3,
}

impl Pressure {
    pub fn name(self) -> &'static str {
        match self {
            Pressure::Normal => "normal",
            Pressure::Elevated => "elevated",
            Pressure::Critical => "critical",
            Pressure::Emergency => "emergency",
        }
    }

    /// Decode an `AtomicU8`-stored discriminant; unrecognised values degrade
    /// to `Normal` rather than panicking, matching `Phase::from_u8`'s
    /// convention in the parent module.
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Pressure::Elevated,
            2 => Pressure::Critical,
            3 => Pressure::Emergency,
            _ => Pressure::Normal,
        }
    }
}

// Enter-thresholds, straight from docs/memory-management.md §2.3.
const ELEVATED_PCT: f64 = 60.0;
const ELEVATED_PSI: f64 = 10.0;
const CRITICAL_PCT: f64 = 75.0;
const CRITICAL_PSI: f64 = 30.0;
const EMERGENCY_PCT: f64 = 90.0;

/// De-escalation requires dropping this many points below the *current*
/// level's own entry threshold before stepping down — otherwise a reading
/// oscillating right at a boundary would flip the level (and log a line)
/// every sample.
const DEESCALATE_MARGIN: f64 = 10.0;

/// De-escalate no faster than once per this interval, regardless of how far
/// the reading has dropped. Recovering instantly to Normal after a brief dip
/// is how you get a sawtooth that spends half its time re-entering Critical.
const MIN_DEESCALATE_INTERVAL: Duration = Duration::from_secs(5);

/// The level implied by this single reading alone, with no hysteresis. Used
/// only for escalation, which must not lag behind reality.
fn raw_level(pct: f64, psi_avg10: f64) -> Pressure {
    if pct >= EMERGENCY_PCT {
        Pressure::Emergency
    } else if pct >= CRITICAL_PCT || psi_avg10 > CRITICAL_PSI {
        Pressure::Critical
    } else if pct >= ELEVATED_PCT || psi_avg10 > ELEVATED_PSI {
        Pressure::Elevated
    } else {
        Pressure::Normal
    }
}

/// Whether the reading has dropped far enough below `level`'s own entry
/// threshold to justify stepping down out of it.
///
/// The margin is only subtracted from the percentage threshold. PSI's own
/// enter-thresholds (10/30) are already sparse enough that subtracting a
/// further 10 points would push the Elevated case to "< 0", which no real
/// PSI reading can ever satisfy — so PSI just has to be back under its own
/// enter-threshold, with the percentage margin doing the work of preventing
/// flapping at the boundary.
fn below_deescalate_threshold(level: Pressure, pct: f64, psi_avg10: f64) -> bool {
    let (pct_threshold, psi_threshold) = match level {
        Pressure::Elevated => (ELEVATED_PCT, ELEVATED_PSI),
        Pressure::Critical => (CRITICAL_PCT, CRITICAL_PSI),
        Pressure::Emergency => (EMERGENCY_PCT, f64::INFINITY),
        Pressure::Normal => return false, // nothing below Normal to leave
    };
    pct < pct_threshold - DEESCALATE_MARGIN && psi_avg10 < psi_threshold
}

fn step_down(level: Pressure) -> Pressure {
    match level {
        Pressure::Emergency => Pressure::Critical,
        Pressure::Critical => Pressure::Elevated,
        Pressure::Elevated => Pressure::Normal,
        Pressure::Normal => Pressure::Normal,
    }
}

/// Tracks the current pressure level with hysteresis and the de-escalation
/// ratchet described above.
///
/// `update` takes `now` explicitly rather than reading `Instant::now()`
/// internally so the ratchet is testable with a synthetic clock.
pub struct PressureTracker {
    current: Pressure,
    last_change: Instant,
}

impl PressureTracker {
    pub fn new(now: Instant) -> Self {
        Self {
            current: Pressure::Normal,
            last_change: now,
        }
    }

    pub fn current(&self) -> Pressure {
        self.current
    }

    /// Feed one reading. Returns `Some(level)` iff the level just changed —
    /// callers only need to log transitions, not every sample.
    pub fn update(
        &mut self,
        pct_of_ceiling: f64,
        psi_avg10: Option<f64>,
        now: Instant,
    ) -> Option<Pressure> {
        let psi = psi_avg10.unwrap_or(0.0);
        let raw = raw_level(pct_of_ceiling, psi);

        let next = if raw > self.current {
            // Escalate immediately: detection must not lag behind reality.
            // Hysteresis only applies on the way back down.
            raw
        } else if raw < self.current
            && below_deescalate_threshold(self.current, pct_of_ceiling, psi)
            && now.saturating_duration_since(self.last_change) >= MIN_DEESCALATE_INTERVAL
        {
            step_down(self.current)
        } else {
            self.current
        };

        if next != self.current {
            self.current = next;
            self.last_change = now;
            Some(next)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escalates_immediately_with_no_hysteresis() {
        let t0 = Instant::now();
        let mut tracker = PressureTracker::new(t0);
        assert_eq!(tracker.update(80.0, None, t0), Some(Pressure::Critical));
        assert_eq!(tracker.current(), Pressure::Critical);
        // A single tick later, Emergency escalates too, well inside what
        // would otherwise be a de-escalation ratchet window.
        let t1 = t0 + Duration::from_millis(250);
        assert_eq!(tracker.update(95.0, None, t1), Some(Pressure::Emergency));
    }

    #[test]
    fn psi_alone_can_escalate_even_at_low_byte_usage() {
        let t0 = Instant::now();
        let mut tracker = PressureTracker::new(t0);
        // 20% of ceiling but PSI says tasks are stalling on memory anyway.
        assert_eq!(
            tracker.update(20.0, Some(35.0), t0),
            Some(Pressure::Critical)
        );
    }

    #[test]
    fn holds_the_level_while_dipping_only_slightly_below_threshold() {
        let t0 = Instant::now();
        let mut tracker = PressureTracker::new(t0);
        tracker.update(80.0, None, t0); // -> Critical (entry 75%)
        let t1 = t0 + Duration::from_secs(10);
        // 70% is below 75% but not below the 65% de-escalate line.
        assert_eq!(tracker.update(70.0, None, t1), None);
        assert_eq!(tracker.current(), Pressure::Critical);
    }

    #[test]
    fn deescalates_one_level_at_a_time_once_past_threshold_and_ratchet() {
        let t0 = Instant::now();
        let mut tracker = PressureTracker::new(t0);
        tracker.update(95.0, None, t0); // -> Emergency
        assert_eq!(tracker.current(), Pressure::Emergency);

        // Deep drop to Normal-range usage, but the ratchet hasn't elapsed.
        let t1 = t0 + Duration::from_secs(1);
        assert_eq!(tracker.update(10.0, None, t1), None);
        assert_eq!(tracker.current(), Pressure::Emergency);

        // 5s since entering Emergency: one step down, to Critical.
        let t2 = t0 + Duration::from_secs(5);
        assert_eq!(tracker.update(10.0, None, t2), Some(Pressure::Critical));

        // Ratchet hasn't elapsed since *this* transition yet.
        let t3 = t2 + Duration::from_secs(1);
        assert_eq!(tracker.update(10.0, None, t3), None);
        assert_eq!(tracker.current(), Pressure::Critical);

        // 5s since entering Critical: one more step down, to Elevated.
        let t4 = t2 + Duration::from_secs(5);
        assert_eq!(tracker.update(10.0, None, t4), Some(Pressure::Elevated));

        // 5s since entering Elevated: last step down, to Normal.
        let t5 = t4 + Duration::from_secs(5);
        assert_eq!(tracker.update(10.0, None, t5), Some(Pressure::Normal));
    }

    #[test]
    fn every_level_has_a_stable_name_and_round_trips_through_u8() {
        for p in [
            Pressure::Normal,
            Pressure::Elevated,
            Pressure::Critical,
            Pressure::Emergency,
        ] {
            assert_eq!(Pressure::from_u8(p as u8), p);
            assert!(!p.name().is_empty());
        }
        assert_eq!(Pressure::from_u8(200), Pressure::Normal);
    }
}
