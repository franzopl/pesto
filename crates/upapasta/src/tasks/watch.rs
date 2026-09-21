//! Watch-mode background tasks: directory scanning and upload dispatch.

use std::path::{Path, PathBuf};
use std::time::Instant;

use tokio::sync::mpsc;

use crate::app::{self, App};
use crate::events::AppEvent;
use crate::tasks::upload::{build_dry_run_config, run_real_upload};

/// Dispatch the next watch-mode item, if any, through the same upload
/// pipeline manual uploads use. No-op when the ready queue is empty or an
/// upload (manual or watch) is already running — the caller is expected to
/// have checked `!app.upload_in_progress` already, but a stale/vanished path
/// at the front of the queue is skipped in a loop rather than stalling watch
/// mode until the next poll interval notices it too.
pub(crate) fn start_watch_upload(app: &mut App, tx: mpsc::UnboundedSender<AppEvent>) {
    if app.upload_in_progress {
        return;
    }
    while let Some(path) = app.watch.ready.pop_front() {
        if !path.exists() {
            app.log_panel.push(format!(
                "[watch] {} vanished before upload, skipping",
                path.display()
            ));
            continue;
        }

        let (label, token, pause_flag) = app.begin_watch_upload(&path);

        let config = if let Some(mut real_cfg) = app.effective_config_with_overrides() {
            real_cfg.dry_run = false;
            real_cfg
        } else {
            build_dry_run_config()
        };
        // Same "uploaded/" destination manual uploads use, so watch-sourced
        // NZBs land in the same place and the Vault/History treat them alike.
        let nzb_out_dir: Option<PathBuf> = app
            .pesto_config
            .as_ref()
            .and_then(|c| c.nzb_dir.as_deref())
            .map(|d| app::expand_tilde(d).join("uploaded"));
        if let Some(ref d) = nzb_out_dir {
            let _ = std::fs::create_dir_all(d);
        }

        let tx2 = tx.clone();
        tokio::spawn(async move {
            let start = Instant::now();
            let result = run_real_upload(
                config,
                vec![path.clone()],
                label,
                nzb_out_dir,
                tx2.clone(),
                token,
                pause_flag,
            )
            .await;
            let duration_s = start.elapsed().as_secs_f64();
            match result {
                Ok(outcome) => {
                    let _ = tx2.send(AppEvent::WatchUploadDone {
                        path,
                        success: !outcome.had_failures,
                        cancelled: outcome.cancelled,
                        size_bytes: outcome.total_bytes,
                        nzb_path: outcome.nzb_path,
                        duration_s,
                    });
                }
                Err(e) => {
                    let _ = tx2.send(AppEvent::UploadError(format!(
                        "[watch] {}: {e}",
                        path.display()
                    )));
                    let _ = tx2.send(AppEvent::WatchUploadDone {
                        path,
                        success: false,
                        cancelled: false,
                        size_bytes: 0,
                        nzb_path: None,
                        duration_s,
                    });
                }
            }
        });
        return;
    }
}

/// Snapshot of `dir`'s top-level entries as (path, size) pairs, for watch
/// mode's stability tracking. `.nfo`/`.nzb` artifacts and dotfiles are
/// skipped; `ext_filter` (comma-separated, case-insensitive, empty = no
/// filtering) narrows which *files* count — subdirectories are always kept
/// since matching files may live inside them.
pub(crate) fn scan_watch_dir(dir: &Path, ext_filter: &str) -> Vec<(PathBuf, u64)> {
    let filters: Vec<&str> = ext_filter
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();

    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    read_dir
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let hidden = p
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with('.'));
            if hidden {
                return false;
            }
            let ext = p.extension().and_then(|e| e.to_str());
            let is_artifact =
                ext.is_some_and(|e| e.eq_ignore_ascii_case("nfo") || e.eq_ignore_ascii_case("nzb"));
            if is_artifact {
                return false;
            }
            p.is_dir()
                || filters.is_empty()
                || ext.is_some_and(|e| filters.iter().any(|f| f.eq_ignore_ascii_case(e)))
        })
        .map(|p| {
            let size = watch_entry_size(&p);
            (p, size)
        })
        .collect()
}

/// Total size of a watch-mode candidate: direct metadata for a file, a
/// recursive walk for a directory (same helper the queue uses for folder
/// sizing).
fn watch_entry_size(path: &Path) -> u64 {
    if path.is_file() {
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    } else {
        app::dir_stats(path).1
    }
}

#[cfg(test)]
mod watch_scan_dir_tests {
    use super::scan_watch_dir;

    /// `.nfo`/`.nzb` artifacts and dotfiles never show up as candidates,
    /// extension filtering only narrows *files* (subdirectories always pass
    /// through since a matching file may live inside), and sizes are real.
    #[test]
    fn skips_artifacts_and_hidden_entries_applies_ext_filter() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("movie.mkv"), b"hello").unwrap();
        std::fs::write(dir.path().join("subs.srt"), b"x").unwrap();
        std::fs::write(dir.path().join("Show.nfo"), b"x").unwrap();
        std::fs::write(dir.path().join("Show.nzb"), b"x").unwrap();
        std::fs::write(dir.path().join(".hidden"), b"x").unwrap();
        std::fs::create_dir(dir.path().join("Season 01")).unwrap();

        let entries = scan_watch_dir(dir.path(), "mkv");
        let names: Vec<String> = entries
            .iter()
            .map(|(p, _)| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();

        assert!(names.contains(&"movie.mkv".to_string()));
        assert!(names.contains(&"Season 01".to_string()));
        assert!(!names.contains(&"subs.srt".to_string()));
        assert!(!names.contains(&"Show.nfo".to_string()));
        assert!(!names.contains(&"Show.nzb".to_string()));
        assert!(!names.contains(&".hidden".to_string()));

        let movie = entries
            .iter()
            .find(|(p, _)| p.file_name().unwrap() == "movie.mkv")
            .unwrap();
        assert_eq!(movie.1, 5);
    }

    /// An empty `ext_filter` matches every non-artifact, non-hidden file.
    #[test]
    fn empty_ext_filter_matches_everything() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mkv"), b"x").unwrap();
        std::fs::write(dir.path().join("b.srt"), b"x").unwrap();

        let entries = scan_watch_dir(dir.path(), "");
        assert_eq!(entries.len(), 2);
    }
}
