//! Source cleanup policy shared by direct, batch and watch uploads.

use std::path::{Path, PathBuf};

use anyhow::Context;

/// Source cleanup behavior after a successful upload.
#[derive(Clone, Debug)]
pub(super) enum CleanupMode {
    /// Leave sources in place (default).
    Leave,
    /// Delete sources.
    Delete,
    /// Move sources to a directory.
    MoveTo(PathBuf),
}

impl CleanupMode {
    /// Apply cleanup to a source path after successful upload.
    pub(super) fn cleanup(&self, source: &Path) -> anyhow::Result<()> {
        match self {
            Self::Leave => Ok(()),
            Self::Delete => {
                if source.is_dir() {
                    std::fs::remove_dir_all(source).with_context(|| {
                        format!("failed to delete directory `{}`", source.display())
                    })?;
                } else {
                    std::fs::remove_file(source)
                        .with_context(|| format!("failed to delete file `{}`", source.display()))?;
                }
                Ok(())
            }
            Self::MoveTo(dest_dir) => {
                std::fs::create_dir_all(dest_dir).with_context(|| {
                    format!(
                        "failed to create cleanup directory `{}`",
                        dest_dir.display()
                    )
                })?;
                let dest_path = dest_dir.join(source.file_name().unwrap_or_default());
                std::fs::rename(source, &dest_path).with_context(|| {
                    format!(
                        "failed to move `{}` to `{}`",
                        source.display(),
                        dest_path.display()
                    )
                })?;
                Ok(())
            }
        }
    }
}

/// Apply post-upload source cleanup for one successful `--watch` entry.
///
/// `used_run_batch` must only be true when `entry` went through `run_batch`,
/// which does not apply `CleanupMode` itself. The legacy `--watch-done` move
/// remains independent of the upload path.
pub(super) fn apply_watch_cleanup(
    cleanup_mode: &CleanupMode,
    entry: &Path,
    watch_done: Option<&Path>,
    used_run_batch: bool,
) -> anyhow::Result<()> {
    match cleanup_mode {
        // --cleanup/--cleanup-to take priority over legacy --watch-done.
        CleanupMode::Delete | CleanupMode::MoveTo(_) => {
            if used_run_batch {
                cleanup_mode.cleanup(entry)?;
            }
            Ok(())
        }
        CleanupMode::Leave => {
            if let Some(done_dir) = watch_done {
                std::fs::create_dir_all(done_dir).with_context(|| {
                    format!(
                        "failed to create --watch-done directory `{}`",
                        done_dir.display()
                    )
                })?;
                let dest = done_dir.join(entry.file_name().unwrap_or_default());
                std::fs::rename(entry, &dest).with_context(|| {
                    format!(
                        "could not move `{}` to `{}`",
                        entry.display(),
                        dest.display()
                    )
                })?;
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn leave_mode_does_nothing() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("test.txt");
        fs::write(&file_path, "content").unwrap();

        assert!(CleanupMode::Leave.cleanup(&file_path).is_ok());
        assert!(file_path.exists(), "file should still exist in Leave mode");
    }

    #[test]
    fn delete_mode_removes_file() {
        let temp = TempDir::new().unwrap();
        let file_path = temp.path().join("test.txt");
        fs::write(&file_path, "content").unwrap();

        assert!(CleanupMode::Delete.cleanup(&file_path).is_ok());
        assert!(!file_path.exists(), "file should be deleted in Delete mode");
    }

    #[test]
    fn delete_mode_removes_directory() {
        let temp = TempDir::new().unwrap();
        let dir_path = temp.path().join("test_dir");
        fs::create_dir(&dir_path).unwrap();
        fs::write(dir_path.join("file.txt"), "content").unwrap();

        assert!(CleanupMode::Delete.cleanup(&dir_path).is_ok());
        assert!(!dir_path.exists(), "directory should be deleted");
    }

    #[test]
    fn move_to_mode_moves_file() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let archive = temp.path().join("archive");
        fs::write(&source, "content").unwrap();

        let mode = CleanupMode::MoveTo(archive.clone());
        assert!(mode.cleanup(&source).is_ok());

        let moved_file = archive.join("source.txt");
        assert!(!source.exists(), "source file should be removed");
        assert!(moved_file.exists(), "file should be moved to archive");
        assert_eq!(fs::read_to_string(moved_file).unwrap(), "content");
    }

    #[test]
    fn move_to_mode_moves_directory() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source_dir");
        let archive = temp.path().join("archive");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("file.txt"), "content").unwrap();

        let mode = CleanupMode::MoveTo(archive.clone());
        assert!(mode.cleanup(&source).is_ok());

        let moved_dir = archive.join("source_dir");
        assert!(!source.exists(), "source directory should be removed");
        assert!(moved_dir.exists(), "directory should be moved to archive");
        assert_eq!(
            fs::read_to_string(moved_dir.join("file.txt")).unwrap(),
            "content"
        );
    }

    #[test]
    fn move_to_mode_creates_archive_directory() {
        let temp = TempDir::new().unwrap();
        let source = temp.path().join("source.txt");
        let archive = temp.path().join("archive");
        fs::write(&source, "content").unwrap();

        assert!(!archive.exists());
        assert!(CleanupMode::MoveTo(archive.clone())
            .cleanup(&source)
            .is_ok());
        assert!(archive.join("source.txt").exists());
    }

    #[test]
    fn watch_cleanup_skips_delete_when_direct_upload_already_did_it() {
        let temp = TempDir::new().unwrap();
        let entry = temp.path().join("already_gone.mkv");

        let result = apply_watch_cleanup(&CleanupMode::Delete, &entry, None, false);
        assert!(result.is_ok(), "must not apply cleanup twice: {result:?}");
    }

    #[test]
    fn watch_cleanup_skips_move_when_direct_upload_already_did_it() {
        let temp = TempDir::new().unwrap();
        let entry = temp.path().join("already_moved.mkv");
        let archive = temp.path().join("archive");

        let result =
            apply_watch_cleanup(&CleanupMode::MoveTo(archive.clone()), &entry, None, false);
        assert!(result.is_ok());
        assert!(!archive.exists(), "must not touch the archive twice");
    }

    #[test]
    fn watch_cleanup_deletes_after_batch_upload() {
        let temp = TempDir::new().unwrap();
        let entry = temp.path().join("season_dir");
        fs::create_dir(&entry).unwrap();
        fs::write(entry.join("ep01.mkv"), "content").unwrap();

        let result = apply_watch_cleanup(&CleanupMode::Delete, &entry, None, true);
        assert!(result.is_ok(), "{result:?}");
        assert!(!entry.exists(), "batch upload leaves cleanup to watch mode");
    }

    #[test]
    fn legacy_watch_done_runs_for_both_upload_paths() {
        for used_run_batch in [false, true] {
            let temp = TempDir::new().unwrap();
            let entry = temp.path().join("legacy.mkv");
            let done_dir = temp.path().join("done");
            fs::write(&entry, "content").unwrap();

            let result =
                apply_watch_cleanup(&CleanupMode::Leave, &entry, Some(&done_dir), used_run_batch);
            assert!(result.is_ok(), "{result:?}");
            assert!(done_dir.join("legacy.mkv").exists());
        }
    }
}
