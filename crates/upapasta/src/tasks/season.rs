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

/// Returns true if season pack generation is allowed given encryption configuration.
/// Encrypted season consolidation is rejected because individual episode uploads
/// use independent session salts and segment indices.
pub(crate) fn is_season_encryption_supported(is_encrypted: bool) -> bool {
    !is_encrypted
}

pub(crate) const ENCRYPTED_SEASON_UNSUPPORTED_MSG: &str =
    "encrypted season consolidation is not supported: individual episode uploads use independent session salts and segment indices; individual per-episode NZBs have been generated";

/// Preflight check for season mode uploads: rejects encrypted season consolidation
/// before any episode upload or NNTP transfer begins.
pub(crate) fn check_season_encryption_preflight(
    folder_mode: crate::app::FolderMode,
    is_encrypted: bool,
) -> Result<(), &'static str> {
    if folder_mode == crate::app::FolderMode::Season && is_encrypted {
        Err(ENCRYPTED_SEASON_UNSUPPORTED_MSG)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod season_nzb_gate_tests {
    use super::{
        check_season_encryption_preflight, is_season_encryption_supported,
        season_pack_skip_message, should_write_season_pack, ENCRYPTED_SEASON_UNSUPPORTED_MSG,
    };
    use crate::app::FolderMode;

    #[test]
    fn test_season_encryption_rejection() {
        assert!(is_season_encryption_supported(false));
        assert!(!is_season_encryption_supported(true));
    }

    #[test]
    fn test_check_season_encryption_preflight() {
        assert!(check_season_encryption_preflight(FolderMode::Single, false).is_ok());
        assert!(check_season_encryption_preflight(FolderMode::Single, true).is_ok());
        assert!(check_season_encryption_preflight(FolderMode::PerFile, false).is_ok());
        assert!(check_season_encryption_preflight(FolderMode::PerFile, true).is_ok());
        assert!(check_season_encryption_preflight(FolderMode::Season, false).is_ok());
        assert!(check_season_encryption_preflight(FolderMode::Season, true).is_err());
        assert_eq!(
            check_season_encryption_preflight(FolderMode::Season, true),
            Err(ENCRYPTED_SEASON_UNSUPPORTED_MSG)
        );
    }

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
