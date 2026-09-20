//! Posting results and the pure policies that decide whether to publish them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tracing::{debug, info, warn};

/// A posted segment, retained for later `.nzb` generation.
///
/// `file_path`, `subject_name` and `from` are `Arc`-shared rather than owned
/// `PathBuf`/`String`: every segment is held twice at once — once in
/// `Shared::results`, once again as a `check::QueueItem` in the streaming
/// check queue's per-server heap while it awaits its `STAT` — and these three
/// fields are identical across every segment of the same file (or, for
/// `from` outside article mode, the whole run). Measured on an
/// 83.4 GiB / 116 619-segment run, the two copies together cost ~150 MiB;
/// sharing these three turns the second copy's allocation for them into a
/// refcount bump. `file_name`/`message_id` stay owned `String` — they're
/// unique per segment, so there's nothing to share.
#[derive(Debug, Clone)]
pub struct PostedSegment {
    pub file_name: String,
    /// Absolute filesystem path of the source file, preserved so a post-check
    /// repost can re-read the segment regardless of the current working
    /// directory. `file_name` alone (the published/relative name) is
    /// insufficient — see `FailedTask::file_path` (issue #23), which this
    /// mirrors for the `--check` repost path.
    pub file_path: Arc<Path>,
    pub subject_name: Arc<str>,
    /// The wire identity (Subject/yEnc `name=`) actually used to post this
    /// segment — independent of `subject_name`, which is always the real
    /// filename for NZB purposes regardless of `--obfuscate` (see
    /// `generate`'s doc comment in `nzb.rs`). A `--check` repost of a
    /// missing article must reuse *this*, not `subject_name`, or an
    /// obfuscated release leaks its real name back onto the wire the moment
    /// one article needs reposting. Empty for segments reconstructed from a
    /// parsed `.nzb` (`nzb::parse`), which never re-encode.
    pub wire_name: Arc<str>,
    /// The exact yEnc `=ybegin name=` used for this segment. This is separate
    /// from `wire_name` because every mode except `none`/`light` deliberately
    /// avoids making Subject and yEnc name identical.
    pub wire_yenc_name: Arc<str>,
    pub file_size: u64,
    pub part: u32,
    pub total: u32,
    pub message_id: String,
    pub bytes: u64,
    pub from: Arc<str>,
    /// Date header as `(rfc_string, unix_timestamp)`. Both parts are preserved
    /// so fixed dates survive round-trips and retries.
    pub date: (Option<String>, Option<u64>),
    /// CRC-32 of the whole file this segment belongs to. Only meaningful (and
    /// only ever emitted on the `=yend` line) when `part == total` — see
    /// `PostTask::file_crc32`.
    pub full_crc32: u32,
    /// Index into this run's server list (`Config::all_servers()` order) of
    /// the server that actually accepted this article's `240`. The
    /// streaming check queue (`poster::check`) uses this to `STAT` the same
    /// server the article was posted to, instead of guessing — with a
    /// multi-server failover config, different articles from the same run
    /// can legitimately land on different servers, and a provider that
    /// never received an article obviously can't confirm it. Copied from
    /// `.pesto-state` on a resume re-STAT so the check targets the same host.
    /// Left as `0` for dry-run segments (nothing was actually posted) and
    /// pre-schema resume records.
    pub server_idx: usize,
    /// This file's 1-based position among every file in the release, and the
    /// release's total file count — the `--file-counter` subject prefix.
    /// `(0, 0)` when the flag is off; see `Shared::total_files`. Denormalized
    /// here (rather than looked up via `Shared`) because both the NZB writer
    /// and a `--check` repost rebuild the subject from a `PostedSegment`
    /// alone, long after `Shared` is gone.
    pub file_index: u32,
    pub total_files: u32,
}

/// A segment that failed to post during the upload run. Carries enough
/// information to re-post the *same* article on the end-of-run retry pass.
#[derive(Debug, Clone)]
pub struct FailedTask {
    /// Published name (relative path / base name) used for NZB metadata and
    /// logging. Not a filesystem path — see [`FailedTask::file_path`].
    pub file_name: String,
    /// Canonical relative path restored by NZB/PAR2 clients.
    pub client_path: String,
    /// Absolute filesystem path of the source file, preserved so the end-of-run
    /// retry can re-read the segment regardless of the current working
    /// directory. `file_name` alone is insufficient (issue #23).
    pub file_path: PathBuf,
    /// The Message-ID the in-run attempts used. The end-of-run retry re-posts
    /// with this *same* ID so that, if the article actually reached the server
    /// during the run (e.g. the `240` ack was lost when the connection died),
    /// the server can deduplicate it: it answers `441 … 435 Already exists`,
    /// which is now treated as success instead of producing a duplicate article
    /// under a fresh ID. Mirrors nyuu's same-Message-ID repost strategy.
    pub message_id: String,
    pub subject_name: String,
    /// The yEnc `=ybegin ... name=` value the in-run attempt used —
    /// independent of `subject_name` under `Full`/`Article`/`FullShared`
    /// obfuscation. Carried through so a repost doesn't fall back to reusing
    /// `subject_name` for both, which would reintroduce the exact-match
    /// signature those modes deliberately avoid.
    pub yenc_name: String,
    pub file_size: u64,
    pub part: u32,
    pub total: u32,
    pub from: String,
    /// Date header as `(rfc_string, unix_timestamp)`. Both are preserved so
    /// fixed dates (which have `Some` RFC but `None` timestamp) are not lost.
    pub date: (Option<String>, Option<u64>),
    /// CRC-32 of the whole file this segment belongs to — see
    /// `PostedSegment::full_crc32`. Only meaningful when `part == total`.
    pub full_crc32: u32,
    /// See `PostedSegment::file_index`/`total_files` — carried through so the
    /// end-of-run retry can rebuild the identical subject.
    pub file_index: u32,
    pub total_files: u32,
}

/// The result of a posting run.
#[derive(Debug)]
pub struct PostOutcome {
    pub segments: Vec<PostedSegment>,
    pub failures: Vec<String>,
    /// Segments that never got a `240` even after the in-run blind retry
    /// pass, preserved so the caller can report them.
    pub failed_tasks: Vec<FailedTask>,
    pub cancelled: bool,
    /// The newsgroup(s) actually used for this upload.
    pub groups: Vec<String>,
    /// The server(s) that actually accepted at least one article this run.
    pub servers: Vec<String>,
    /// Message-IDs that were posted but remained missing after every check.
    pub still_missing: Vec<String>,
    /// Message-IDs whose check path ended without a conclusive server answer.
    pub inconclusive: Vec<String>,
    /// Producer failure that stopped the run, distinct from user cancellation.
    pub failure_reason: Option<String>,
    /// This run's isolated PAR2 scratch directory.
    pub par2_temp_dir: PathBuf,
}

impl PostOutcome {
    /// Remove this run's PAR2 scratch directory after every consumer has
    /// finished reading it. Cleanup failures do not invalidate the outcome.
    pub async fn cleanup_par2_temp_dir(&self) {
        cleanup_par2_temp_dir(&self.par2_temp_dir).await;
    }
}

/// Whether the NZB (and NFO / post-hooks) should be written for this run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NzbWriteDecision {
    Write,
    Refuse,
}

/// Decide whether the run is complete enough to publish its NZB artifacts.
pub fn nzb_write_decision(
    post_failures: bool,
    missing_confirmed: bool,
    inconclusive: bool,
    allow_incomplete_nzb: bool,
) -> NzbWriteDecision {
    if post_failures || inconclusive || (missing_confirmed && !allow_incomplete_nzb) {
        NzbWriteDecision::Refuse
    } else {
        NzbWriteDecision::Write
    }
}

/// Decide whether a combined `--season` NZB should be written.
pub fn should_write_season_nzb(
    any_cancelled: bool,
    any_episode_incomplete: bool,
    all_segments_empty: bool,
) -> bool {
    !any_cancelled && !any_episode_incomplete && !all_segments_empty
}

async fn cleanup_par2_temp_dir(path: &Path) {
    let started = Instant::now();
    match tokio::fs::remove_dir_all(path).await {
        Ok(()) => info!(
            path = %path.display(),
            elapsed_ms = started.elapsed().as_millis(),
            "PAR2 scratch directory removed"
        ),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            debug!(path = %path.display(), "PAR2 scratch directory was not created")
        }
        Err(error) => warn!(
            path = %path.display(),
            error = %error,
            "failed to remove PAR2 scratch directory"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nzb_write_decision_writes_when_everything_confirmed() {
        assert_eq!(
            nzb_write_decision(false, false, false, false),
            NzbWriteDecision::Write
        );
    }

    #[test]
    fn nzb_write_decision_refuses_post_failures_even_with_allow() {
        assert_eq!(
            nzb_write_decision(true, false, false, true),
            NzbWriteDecision::Refuse
        );
    }

    #[test]
    fn nzb_write_decision_allow_incomplete_unblocks_missing_only() {
        assert_eq!(
            nzb_write_decision(false, true, false, true),
            NzbWriteDecision::Write
        );
        assert_eq!(
            nzb_write_decision(false, true, false, false),
            NzbWriteDecision::Refuse
        );
    }

    #[test]
    fn nzb_write_decision_inconclusive_always_refuses() {
        assert_eq!(
            nzb_write_decision(false, false, true, true),
            NzbWriteDecision::Refuse
        );
        assert_eq!(
            nzb_write_decision(false, true, true, true),
            NzbWriteDecision::Refuse
        );
    }

    #[test]
    fn should_write_season_nzb_requires_every_episode_complete() {
        assert!(should_write_season_nzb(false, false, false));
        for inputs in [
            (true, false, false),
            (false, true, false),
            (false, false, true),
            (true, true, false),
            (false, true, true),
            (true, false, true),
            (true, true, true),
        ] {
            assert!(!should_write_season_nzb(inputs.0, inputs.1, inputs.2));
        }
    }

    #[test]
    fn season_pack_uses_had_failures_not_nzb_write_decision() {
        let post_failures = false;
        let missing_confirmed = true;
        let inconclusive = false;
        let allow_incomplete_nzb = true;
        assert_eq!(
            nzb_write_decision(
                post_failures,
                missing_confirmed,
                inconclusive,
                allow_incomplete_nzb
            ),
            NzbWriteDecision::Write
        );
        let had_failures = post_failures || missing_confirmed || inconclusive;
        assert!(had_failures);
        assert!(!should_write_season_nzb(false, had_failures, false));
    }
}
