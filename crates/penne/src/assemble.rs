//! File assembly: turning decoded segment bodies back into whole files.
//!
//! Not implemented yet. Depends on [`crate::client`] (article retrieval,
//! Phase 2) and a yEnc *decoder* (`pesto::yenc` currently only encodes —
//! adding `decode_into` there is Phase 3). See `ROADMAP.md` Phase 4.
//!
//! Planned shape: for each [`crate::queue::QueuedFile`], write each decoded
//! segment at its offset into the destination file (segments may finish out
//! of order across connections), then verify the file's CRC32 once every
//! part has landed.

use anyhow::Result;

use crate::queue::QueuedFile;

/// Assemble `file`'s segments (already fetched and yEnc-decoded) into a
/// single file under `dest_dir`.
///
/// Not implemented yet — see the module docs and `ROADMAP.md` Phase 4.
pub async fn assemble(file: &QueuedFile, dest_dir: &std::path::Path) -> Result<()> {
    anyhow::bail!(
        "assembly not implemented yet ({} -> {}); see ROADMAP.md Phase 4",
        file.name,
        dest_dir.display()
    )
}
