//! Season-pack write/skip gate shared by the upload tasks.

/// Combined season NZB is a distinct artefact from per-episode NZBs.
/// `--allow-incomplete-nzb` never unlocks the pack: `folder_ok` is already
/// false on `had_failures` (including MissingConfirmed with the flag).
pub(crate) fn should_write_season_pack(
    any_cancelled: bool,
    folder_ok: bool,
    all_segments_empty: bool,
) -> bool {
    pesto::poster::should_write_season_nzb(any_cancelled, !folder_ok, all_segments_empty)
}

/// Same skip reasons as pesto CLI `run_batch`. `None` means write the pack.
pub(crate) fn season_pack_skip_message(
    any_cancelled: bool,
    folder_ok: bool,
    all_segments_empty: bool,
) -> Option<&'static str> {
    if should_write_season_pack(any_cancelled, folder_ok, all_segments_empty) {
        None
    } else if any_cancelled {
        Some("interrupted — skipping season nzb output")
    } else if !folder_ok {
        Some("season pack was not created due to earlier upload failures")
    } else {
        Some("season pack was not created (no valid segments were uploaded)")
    }
}

#[cfg(test)]
mod season_nzb_gate_tests {
    use super::{season_pack_skip_message, should_write_season_pack};

    /// T16b: production `should_write_season_pack` is what the Season
    /// `fs::write` calls. `folder_ok == false` (had_failures, including
    /// MissingConfirmed with `--allow-incomplete-nzb`) is incomplete.
    #[test]
    fn t16b_folder_ok_writes_pack() {
        assert!(should_write_season_pack(false, true, false));
        assert_eq!(season_pack_skip_message(false, true, false), None);
    }

    #[test]
    fn t16b_folder_ok_false_does_not_write_pack() {
        assert!(!should_write_season_pack(false, false, false));
        assert_eq!(
            season_pack_skip_message(false, false, false),
            Some("season pack was not created due to earlier upload failures")
        );
    }

    #[test]
    fn t16b_cancelled_does_not_write_pack() {
        assert!(!should_write_season_pack(true, true, false));
        assert_eq!(
            season_pack_skip_message(true, true, false),
            Some("interrupted — skipping season nzb output")
        );
    }

    #[test]
    fn t16b_empty_segments_does_not_write_pack() {
        assert!(!should_write_season_pack(false, true, true));
        assert_eq!(
            season_pack_skip_message(false, true, true),
            Some("season pack was not created (no valid segments were uploaded)")
        );
    }
}
