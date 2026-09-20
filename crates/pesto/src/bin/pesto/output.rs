//! Output-path policy for generated NZB files.

use std::path::{Path, PathBuf};

/// Resolve the final user-destination path for the NZB according to the
/// conflict policy.
pub(super) async fn resolve_nzb_dest(
    dest: &Path,
    conflict: pesto::config::NzbConflict,
) -> anyhow::Result<PathBuf> {
    use pesto::config::NzbConflict;

    if !dest.exists() {
        return Ok(dest.to_path_buf());
    }
    match conflict {
        NzbConflict::Overwrite => Ok(dest.to_path_buf()),
        NzbConflict::Rename => {
            let base = dest.with_extension("");
            let stem = base.to_string_lossy();
            let mut n = 1u32;
            loop {
                let candidate = PathBuf::from(format!("{stem}-{n}.nzb"));
                if !candidate.exists() {
                    return Ok(candidate);
                }
                n += 1;
            }
        }
        NzbConflict::Fail => {
            anyhow::bail!(
                "nzb file already exists: {} (set nzb_conflict = \"overwrite\" or \"rename\" to allow)",
                dest.display()
            )
        }
    }
}

/// Return the canonical NZB archive path under the Pesto configuration
/// directory and create its parent directory when needed.
pub(super) async fn nzb_archive_path(stem: &str) -> PathBuf {
    let timestamp = chrono::Utc::now().format("%Y%m%dT%H%M%SZ");
    let filename = format!("{timestamp}_{stem}.nzb");

    if let Some(dir) = pesto::config::config_dir().map(|d| d.join("nzb")) {
        let _ = tokio::fs::create_dir_all(&dir).await;
        dir.join(filename)
    } else {
        PathBuf::from(filename)
    }
}

/// Expand a leading `~` to the user's home directory.
///
/// The path is unchanged when it does not start with `~` or `$HOME` is unset.
pub(super) fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}
