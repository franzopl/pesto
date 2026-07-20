//! Post-extraction cleanup for `penne download --clean`.
//!
//! Once every archive under a release's directory has been extracted
//! ([`crate::extract`]) and PAR2 has already had its chance to verify/repair
//! ([`crate::repair`]), the compressed volumes and PAR2 recovery data no
//! longer serve any purpose for a user who just wants the release's actual
//! content — this deletes both, leaving everything else (the extracted
//! media, subtitles, `.nfo`, etc.) untouched.
//!
//! Scoped to `known_files` for the same reason PAR2 discovery is — see
//! [`crate::repair::find_par2_index`]'s doc comment: `dest_dir` can be
//! shared across every `penne download` run (it defaults to one directory
//! for the whole configuration), so a file belonging to a different,
//! unrelated release might be sitting right next to this one's own. A
//! destructive operation like this must never touch anything outside what
//! this run's own queue actually produced.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};

use crate::extract::classify;

/// Delete every archive-volume and PAR2 file directly under `dir` whose
/// name is in `known_files`. Returns the names actually deleted (not full
/// paths), in arbitrary order.
pub async fn purge_archives_and_par2(
    dir: &Path,
    known_files: &HashSet<String>,
) -> Result<Vec<String>> {
    let dir = dir.to_path_buf();
    let known_files = known_files.clone();
    tokio::task::spawn_blocking(move || purge_blocking(&dir, &known_files))
        .await
        .context("cleanup task panicked")?
}

fn purge_blocking(dir: &Path, known_files: &HashSet<String>) -> Result<Vec<String>> {
    let mut deleted = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !known_files.contains(file_name) {
            continue;
        }

        let is_archive_volume = classify(file_name).is_some();
        let is_par2 = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("par2"));
        if !is_archive_volume && !is_par2 {
            continue;
        }

        std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()))?;
        deleted.push(file_name.to_string());
    }
    deleted.sort();
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(dir: &Path) -> HashSet<String> {
        std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect()
    }

    #[tokio::test]
    async fn deletes_archive_volumes_and_par2_but_keeps_everything_else() {
        let dir = tempfile::tempdir().unwrap();
        for name in [
            "movie.rar",
            "movie.r00",
            "movie.par2",
            "movie.vol000+001.par2",
            "movie.mkv",
            "movie.nfo",
        ] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let known: HashSet<String> = names(dir.path());

        let deleted = purge_archives_and_par2(dir.path(), &known).await.unwrap();
        assert_eq!(
            deleted,
            vec![
                "movie.par2".to_string(),
                "movie.r00".to_string(),
                "movie.rar".to_string(),
                "movie.vol000+001.par2".to_string(),
            ]
        );

        let remaining = names(dir.path());
        assert_eq!(
            remaining,
            ["movie.mkv".to_string(), "movie.nfo".to_string()]
                .into_iter()
                .collect::<HashSet<_>>()
        );
    }

    #[tokio::test]
    async fn never_touches_a_file_outside_known_files() {
        // Simulates a shared download_dir: `other-release.rar` belongs to a
        // different, unrelated run and must survive even though it matches
        // the archive extension rule.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("movie.rar"), b"x").unwrap();
        std::fs::write(dir.path().join("other-release.rar"), b"x").unwrap();

        let known: HashSet<String> = ["movie.rar".to_string()].into_iter().collect();
        let deleted = purge_archives_and_par2(dir.path(), &known).await.unwrap();
        assert_eq!(deleted, vec!["movie.rar".to_string()]);
        assert!(dir.path().join("other-release.rar").exists());
    }

    #[tokio::test]
    async fn nothing_to_delete_is_not_an_error() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("movie.mkv"), b"x").unwrap();
        let known: HashSet<String> = names(dir.path());

        let deleted = purge_archives_and_par2(dir.path(), &known).await.unwrap();
        assert!(deleted.is_empty());
        assert!(dir.path().join("movie.mkv").exists());
    }
}
