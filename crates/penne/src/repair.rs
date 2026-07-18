//! PAR2 verification and repair for assembled downloads.
//!
//! Wraps [`pesto::par2`] (the re-exported `parmesan` crate), which already
//! implements PAR2 verify and repair — see `crates/parmesan/ROADMAP.md`
//! Phase 22. `penne` does not reimplement PAR2; it only needs to point the
//! existing engine at the downloaded file set. See `ROADMAP.md` Phase 6.

use std::path::Path;

use anyhow::Result;

/// Verify a downloaded file set against its `.par2` recovery files, and
/// repair it if damaged and enough recovery data was downloaded.
///
/// Not implemented yet — see the module docs and `ROADMAP.md` Phase 6.
pub async fn verify_and_repair(dir: &Path) -> Result<()> {
    anyhow::bail!(
        "PAR2 verify/repair not wired up yet ({}); see ROADMAP.md Phase 6",
        dir.display()
    )
}
