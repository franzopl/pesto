//! Archive extraction for downloads that arrive compressed (`.rar`, `.7z`, `.zip`).
//!
//! `pesto::compress` only *creates* archives before posting; it has no
//! extraction path to build on. This module needs its own implementation.
//! See `ROADMAP.md` Phase 7.

use std::path::Path;

use anyhow::Result;

/// Extract every supported archive found in `dir` in place.
///
/// Not implemented yet — see the module docs and `ROADMAP.md` Phase 7.
pub async fn extract_all(dir: &Path) -> Result<()> {
    anyhow::bail!(
        "archive extraction not implemented yet ({}); see ROADMAP.md Phase 7",
        dir.display()
    )
}
