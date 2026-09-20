use std::fmt;
use std::time::Duration;

use clap::ValueEnum;

/// Which NNTP command an availability check uses to decide "present or
/// not" for each segment — a three-way trade between wire cost and how
/// trustworthy the answer is.
///
/// `Stat` and `Head` never transfer an article body; `Body` does (and
/// discards it, never decoding or writing anything — see
/// [`check_queue`]'s doc comment). Lives here rather than in the CLI
/// binary, mirroring [`crate::config::ProcessingMode`]'s existing
/// precedent of a `clap::ValueEnum` in the library so both the flag and a
/// future library caller share one definition.
#[derive(ValueEnum, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[value(rename_all = "lowercase")]
pub enum CheckMethod {
    /// `STAT` (RFC 3977 §6.2.4): a bare existence check against the
    /// server's index. Cheapest by far, but the index can drift out of
    /// sync with what the server can actually deliver — see `Head`.
    #[default]
    Stat,
    /// `HEAD` (RFC 3977 §6.2.2): fetches just the header block. Still
    /// cheap (a few hundred bytes, not the full article), and on most
    /// servers reads from the same underlying article storage `BODY`
    /// does, catching a provider whose `STAT` index says "present" for an
    /// article its real storage doesn't have. Not guaranteed, though: some
    /// providers apparently serve `HEAD` from a more complete path than
    /// `BODY` — `Body` is the only method that's ever fully certain.
    Head,
    /// `BODY` (RFC 3977 §6.2.3): a full, real article fetch, discarded
    /// immediately. Maximum certainty, real bandwidth cost — the same as
    /// an actual download would pay for the same segment.
    Body,
}

impl fmt::Display for CheckMethod {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            CheckMethod::Stat => "STAT",
            CheckMethod::Head => "HEAD",
            CheckMethod::Body => "BODY",
        })
    }
}

/// Default pipeline depth for `STAT` commands per connection. Each `STAT`
/// is ~80 bytes on the wire, so 128 in-flight STATs buffer ~10 KB — trivial.
/// This hides significantly more RTT latency than nzbCheck's depth of 64,
/// roughly halving check wall-time on high-latency links.
pub const DEFAULT_STAT_PIPELINE_DEPTH: usize = 128;

/// Configuration for a `STAT`/`HEAD`/`BODY` availability check run.
///
/// Designed for both CLI and library callers (`sugo`, `upapasta`, or any
/// code that embeds `penne` as a library and wants to check article
/// availability programmatically).
#[derive(Debug, Clone)]
pub struct CheckConfig {
    /// Which NNTP command to use for the existence check.
    pub method: CheckMethod,
    /// How many `STAT` commands to pipeline per connection per round trip.
    /// Only used when `method` is [`CheckMethod::Stat`]; ignored for
    /// `Head`/`Body`. Clamped to `1..=256` internally.
    pub pipeline_depth: usize,
    /// Retry attempts per server before failing over to the next tier.
    pub retries: u32,
    /// Stop scheduling work after the first article is conclusively missing.
    /// A server failover pass still completes first, so a miss on a primary
    /// alone never produces a false failure.
    pub fail_fast: bool,
}

impl Default for CheckConfig {
    fn default() -> Self {
        Self {
            method: CheckMethod::default(),
            pipeline_depth: DEFAULT_STAT_PIPELINE_DEPTH,
            retries: 3,
            fail_fast: false,
        }
    }
}

impl CheckConfig {
    /// Build a [`CheckConfig`] from individual values, matching the old
    /// `check_queue(queue, tiers, method, retries, progress)` call pattern.
    /// Provided for ergonomic migration of callers that used the previous
    /// four-argument form.
    pub fn new(method: CheckMethod, retries: u32) -> Self {
        Self {
            method,
            retries,
            ..Default::default()
        }
    }
}

/// One segment's `STAT` just resolved, for a live progress bar —
/// deliberately its own small type rather than reusing
/// [`crate::progress::ProgressEvent`]: that enum's variants
/// (`SegmentDownloaded`, `FileAssembled`, ...) describe fetching and
/// writing bytes, none of which a `STAT`-only check ever does.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CheckProgress {
    pub present: bool,
}

pub type CheckProgressSender = tokio::sync::mpsc::UnboundedSender<CheckProgress>;
pub type CheckProgressReceiver = tokio::sync::mpsc::UnboundedReceiver<CheckProgress>;

/// Create a fresh check-progress channel.
pub fn channel() -> (CheckProgressSender, CheckProgressReceiver) {
    tokio::sync::mpsc::unbounded_channel()
}

/// A segment at least one tried server gave a definitive "not present"
/// answer for (`430`/`423`/`420`) and no server ever confirmed present —
/// as opposed to [`UnreachableSegment`], where nobody ever gave a real
/// answer at all. This is the only case that should ever be treated as
/// confirmed data loss.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct MissingSegment {
    pub file_name: String,
    pub part: u32,
    pub message_id: String,
}

/// A segment whose fate was never actually resolved: every server tried
/// for it either failed to connect or exhausted its `STAT`/`HEAD`/`BODY`
/// retries (see [`CheckConfig::retries`]) before returning a definitive
/// present/absent verdict. Deliberately kept out of [`CheckOutcome::missing`]
/// — folding this in would turn a transient network hiccup (a provider
/// timing out, a connection reset) into what looks like confirmed data
/// loss, which is exactly the false positive a caller deciding whether to
/// declare a release dead must not act on. A segment only ever lands here
/// if *no* tried server, across every configured tier, ever managed to say
/// yes or no to it — a single tier's definitive `430` for a segment still
/// counts as [`MissingSegment`] even if another tier later failed to
/// connect (see `WorkItem::confirmed_missing`, carried across tiers for
/// exactly this reason).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct UnreachableSegment {
    pub file_name: String,
    pub part: u32,
    pub message_id: String,
}

/// One file's completeness: how many of its segments a server confirmed.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct FileCheck {
    pub name: String,
    pub total_segments: u32,
    pub present_segments: u32,
}

impl FileCheck {
    pub fn is_complete(&self) -> bool {
        self.total_segments > 0 && self.present_segments >= self.total_segments
    }
}

/// Result of checking a [`crate::queue::DownloadQueue`] against a set of servers.
#[derive(Debug)]
pub struct CheckOutcome {
    /// One entry per file, in `.nzb` queue order.
    pub files: Vec<FileCheck>,
    /// Segments at least one server definitively denied and none confirmed
    /// present — the only segments that should be treated as confirmed
    /// data loss. See [`MissingSegment`].
    pub missing: Vec<MissingSegment>,
    /// Segments no tried server ever gave a real present/absent answer for
    /// — connection failures and exhausted retries, never a `430`. See
    /// [`UnreachableSegment`]; a non-empty list here means `missing` is not
    /// yet a trustworthy final verdict for the release as a whole.
    pub unreachable: Vec<UnreachableSegment>,
    /// Total bytes actually sent/received over the wire to perform this
    /// check (every `STAT <id>` command and its response, across every
    /// connection opened) — the whole point of `STAT` over a real fetch is
    /// that this stays tiny even for a release with thousands of segments.
    pub bytes_used: u64,
    /// Wall-clock duration of the check (from entering `check_queue` to
    /// the last response being read, not including NZB parsing or queue
    /// construction).
    pub elapsed: Duration,
    /// Total number of articles requested (segments in the queue), including
    /// any that a fail-fast run deliberately skipped.
    pub total_checked: u32,
    /// Articles confirmed present on at least one configured server.
    pub total_present: u32,
    /// `true` when `CheckConfig::fail_fast` stopped the run after a
    /// confirmed absence. Counts and per-file results then cover only the
    /// work completed before stopping.
    pub stopped_early: bool,
    /// Segments deliberately not checked because `fail_fast` stopped the
    /// scheduler. They are neither missing nor unreachable.
    pub skipped: u32,
}

impl CheckOutcome {
    /// True only when every segment was confirmed present by some server —
    /// `false` for both confirmed-missing segments and merely-unreachable
    /// ones, since neither case means the release is safely grabbable.
    pub fn is_complete(&self) -> bool {
        !self.stopped_early && self.missing.is_empty() && self.unreachable.is_empty()
    }

    /// True when every segment got a definitive answer (present or
    /// confirmed absent) from some server. `false` means at least one
    /// segment's fate is still unknown, so `missing` — even if empty —
    /// cannot yet be read as "this release is fully present".
    pub fn is_conclusive(&self) -> bool {
        !self.stopped_early && self.unreachable.is_empty()
    }

    /// Articles actually checked per second, based on wall-clock elapsed
    /// time. Deliberately skipped fail-fast items are not counted.
    /// Returns `0.0` when elapsed is zero (instantaneous or empty queue).
    pub fn articles_per_second(&self) -> f64 {
        let secs = self.elapsed.as_secs_f64();
        if secs > 0.0 {
            (self.total_checked - self.skipped) as f64 / secs
        } else {
            0.0
        }
    }

    /// Number of confirmed-missing segments — convenience alias for
    /// `self.missing.len()`.
    pub fn missing_count(&self) -> usize {
        self.missing.len()
    }

    /// Number of unreachable segments — convenience alias for
    /// `self.unreachable.len()`.
    pub fn unreachable_count(&self) -> usize {
        self.unreachable.len()
    }
}

/// Provide a sensible default for tests and callers that build a
/// `CheckOutcome` directly (the `elapsed`/`total_*` fields default to
/// zero, which is correct for an "empty queue checked" outcome).
impl Default for CheckOutcome {
    fn default() -> Self {
        Self {
            files: Vec::new(),
            missing: Vec::new(),
            unreachable: Vec::new(),
            bytes_used: 0,
            elapsed: Duration::ZERO,
            total_checked: 0,
            total_present: 0,
            stopped_early: false,
            skipped: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_config_default_has_expected_values() {
        let config = CheckConfig::default();
        assert_eq!(config.method, CheckMethod::Stat);
        assert_eq!(config.pipeline_depth, DEFAULT_STAT_PIPELINE_DEPTH);
        assert_eq!(config.pipeline_depth, 128);
        assert_eq!(config.retries, 3);
    }

    #[test]
    fn check_config_new_uses_default_pipeline_depth() {
        let config = CheckConfig::new(CheckMethod::Head, 5);
        assert_eq!(config.method, CheckMethod::Head);
        assert_eq!(config.retries, 5);
        assert_eq!(config.pipeline_depth, DEFAULT_STAT_PIPELINE_DEPTH);
    }

    #[test]
    fn check_outcome_articles_per_second() {
        let outcome = CheckOutcome {
            elapsed: Duration::from_secs(2),
            total_checked: 1000,
            total_present: 990,
            ..Default::default()
        };
        assert!((outcome.articles_per_second() - 500.0).abs() < 0.01);
    }

    #[test]
    fn check_outcome_articles_per_second_zero_elapsed() {
        let outcome = CheckOutcome {
            elapsed: Duration::ZERO,
            total_checked: 100,
            ..Default::default()
        };
        assert_eq!(outcome.articles_per_second(), 0.0);
    }

    #[test]
    fn check_outcome_is_complete_when_no_missing() {
        let outcome = CheckOutcome::default();
        assert!(outcome.is_complete());
    }

    #[test]
    fn check_outcome_not_complete_when_missing() {
        let outcome = CheckOutcome {
            missing: vec![MissingSegment {
                file_name: "test".to_string(),
                part: 1,
                message_id: "id@x".to_string(),
            }],
            ..Default::default()
        };
        assert!(!outcome.is_complete());
        assert_eq!(outcome.missing_count(), 1);
    }

    #[test]
    fn file_check_is_complete_logic() {
        let complete = FileCheck {
            name: "a".to_string(),
            total_segments: 5,
            present_segments: 5,
        };
        assert!(complete.is_complete());

        let incomplete = FileCheck {
            name: "b".to_string(),
            total_segments: 5,
            present_segments: 3,
        };
        assert!(!incomplete.is_complete());

        let empty = FileCheck {
            name: "c".to_string(),
            total_segments: 0,
            present_segments: 0,
        };
        assert!(!empty.is_complete());
    }
}
