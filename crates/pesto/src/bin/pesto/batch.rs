//! Pure input and naming policy for batch and season uploads.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::output::expand_tilde;

fn is_artifact_entry(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| ext == "nfo" || ext == "nzb")
}

/// Whether `path`'s extension is one of `ext_filter` (case-insensitive). An
/// empty `ext_filter` matches everything (the `--ext` default: no filtering).
fn matches_ext_filter(path: &Path, ext_filter: &[String]) -> bool {
    if ext_filter.is_empty() {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ext_filter
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(ext))
        })
}

/// Apply `--ext` to an already-expanded input list, in place. A no-op when
/// `ext_filter` is empty. Errors out if the filter drops every input, so a
/// mistyped extension (or an entry that is 100% subtitles/extras) fails
/// loudly instead of silently posting nothing.
pub(super) fn apply_ext_filter(
    inputs: &mut Vec<pesto::walk::InputFile>,
    ext_filter: &[String],
    entry_label: &str,
) -> Result<()> {
    if ext_filter.is_empty() {
        return Ok(());
    }
    inputs.retain(|f| matches_ext_filter(&f.path, ext_filter));
    if inputs.is_empty() {
        anyhow::bail!(
            "no files matching --ext {} found in `{entry_label}`",
            ext_filter.join(",")
        );
    }
    Ok(())
}

/// Enumerate top-level entries of `dir` (files and subdirectories), sorted by
/// name using natural lexical ordering (so `E02` comes before `E10`).
///
/// `ext_filter` (from `--ext`) drops non-matching *files*; subdirectories are
/// always kept regardless of their name, since matching files may live inside
/// them.
pub(super) fn top_level_entries(dir: &Path, ext_filter: &[String]) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory `{}`", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| !is_artifact_entry(p))
        .filter(|p| p.is_dir() || matches_ext_filter(p, ext_filter))
        .collect();
    entries.sort_by(|a, b| pesto::walk::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    Ok(entries)
}

/// Post season PAR2 volumes and return the resulting segments.
///
/// Generates and posts global PAR2 recovery volumes covering all episodes,
/// then collects the posted segments for inclusion in the consolidated season NZB.
pub(super) fn derive_season_nzb_path(
    explicit_out: Option<&Path>,
    entry: &Path,
    nzb_dir: Option<&str>,
) -> PathBuf {
    if let Some(out) = explicit_out {
        return out.to_path_buf();
    }
    // `Path::file_name()` deliberately returns `None` for paths ending in
    // `.` or `..`. Resolve those paths first so `pesto . --season` names the
    // pack after its input directory.
    let name = entry
        .file_name()
        .map(|name| name.to_owned())
        .or_else(|| {
            entry
                .canonicalize()
                .ok()
                .and_then(|resolved| resolved.file_name().map(|name| name.to_owned()))
        })
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "season".to_string());
    let stem = format!("{name}.nzb");
    match nzb_dir {
        Some(dir) => expand_tilde(dir).join(&stem),
        None => PathBuf::from(&stem),
    }
}

/// For a `--season` batch, force every episode's `Config::groups` onto the
/// same pre-picked single-entry target, so they all land on the same
/// newsgroup(s) instead of each episode's own internal `pick_post_group`
/// call (inside `poster::post_files`) re-rolling independently — which used
/// to leave the merged season NZB's `<groups>` list as just the union of
/// whatever each episode randomly landed on, rather than one group (or
/// cross-post set) every episode actually shares.
///
/// Resolved once here, exactly like `season_password` in `run_batch`, then
/// forced onto `Config::groups` as a single already-picked entry: with only
/// one configured entry, `pick_post_group`'s own call inside `post_files`
/// has nothing left to randomize, so every episode deterministically
/// reproduces this same pick. A no-op (returns `params` unchanged) outside
/// `--season`, or when there are no configured groups to pick from.
pub(super) fn release_label(path: &Path) -> String {
    const STRIP_EXTS: &[&str] = &[
        "mkv", "mp4", "avi", "ts", "m2ts", "mov", "wmv", "flv", "webm", "mpg", "mpeg", "vob",
        "iso", "nzb", "zip", "rar", "7z", "tar", "gz", "bz2", "cbz", "cbr", "pdf", "epub",
    ];
    path.file_name()
        .map(|s| {
            let name = s.to_string_lossy();
            let p = Path::new(s);
            match p.extension().and_then(|e| e.to_str()) {
                Some(ext) if STRIP_EXTS.contains(&ext.to_ascii_lowercase().as_str()) => {
                    p.file_stem().unwrap_or(s).to_string_lossy().into_owned()
                }
                _ => name.into_owned(),
            }
        })
        .unwrap_or_else(|| "entry".to_string())
}

#[cfg(test)]
mod release_label_tests {
    use super::release_label;
    use std::path::Path;

    #[test]
    fn strips_mkv_from_season_episode_file() {
        assert_eq!(
            release_label(Path::new("/tv/Show.S01E01.1080p.mkv")),
            "Show.S01E01.1080p"
        );
    }

    #[test]
    fn keeps_scene_name_without_media_extension() {
        assert_eq!(
            release_label(Path::new("/tv/Show.S01E01.720p.BluRay-Group")),
            "Show.S01E01.720p.BluRay-Group"
        );
    }

    #[test]
    fn strips_extension_case_insensitively() {
        assert_eq!(release_label(Path::new("Movie.MKV")), "Movie");
    }

    #[test]
    fn keeps_directory_like_names() {
        assert_eq!(release_label(Path::new("/season/Episode01")), "Episode01");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_artifact_entry_matches_nfo_and_nzb_case_insensitively() {
        assert!(is_artifact_entry(Path::new("Show.nfo")));
        assert!(is_artifact_entry(Path::new("Show.NZB")));
        assert!(is_artifact_entry(Path::new("/a/b/c.NfO")));
        assert!(!is_artifact_entry(Path::new("Show.mkv")));
        assert!(!is_artifact_entry(Path::new("Show")));
        assert!(!is_artifact_entry(Path::new("nfo")));
    }

    #[test]
    fn top_level_entries_skips_generated_artifacts() {
        let dir = std::env::temp_dir().join(format!(
            "pesto_each_artifact_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ep01.mkv"), b"x").unwrap();
        // Orphan artifacts left in the input directory by a previous run.
        std::fs::write(dir.join("ep01.nfo"), b"x").unwrap();
        std::fs::write(dir.join("ep01.nzb"), b"x").unwrap();

        let names: Vec<String> = top_level_entries(&dir, &[])
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["ep01.mkv"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn matches_ext_filter_is_case_insensitive_and_empty_means_everything() {
        assert!(matches_ext_filter(Path::new("Show.MKV"), &["mkv".into()]));
        assert!(matches_ext_filter(Path::new("Show.mkv"), &["MKV".into()]));
        assert!(!matches_ext_filter(Path::new("Show.srt"), &["mkv".into()]));
        assert!(matches_ext_filter(Path::new("Show.srt"), &[]));
        assert!(!matches_ext_filter(Path::new("Show"), &["mkv".into()]));
    }

    #[test]
    fn top_level_entries_filters_loose_files_by_ext_but_keeps_directories() {
        let dir = std::env::temp_dir().join(format!(
            "pesto_each_ext_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("Extras")).unwrap();
        std::fs::write(dir.join("ep01.mkv"), b"x").unwrap();
        std::fs::write(dir.join("ep01.srt"), b"x").unwrap();

        let names: Vec<String> = top_level_entries(&dir, &["mkv".to_string()])
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // The loose .srt sibling is dropped; the subdirectory is kept even
        // though "Extras" has no matching extension of its own, since a
        // matching file could live inside it.
        assert_eq!(names, ["ep01.mkv", "Extras"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn apply_ext_filter_drops_non_matching_and_errors_when_nothing_left() {
        let mut inputs = vec![
            pesto::walk::InputFile {
                path: PathBuf::from("ep01.mkv"),
                name: "ep01.mkv".to_string(),
            },
            pesto::walk::InputFile {
                path: PathBuf::from("ep01.srt"),
                name: "ep01.srt".to_string(),
            },
        ];
        apply_ext_filter(&mut inputs, &["mkv".to_string()], "entry").unwrap();
        assert_eq!(inputs.len(), 1);
        assert_eq!(inputs[0].name, "ep01.mkv");

        let mut only_subs = vec![pesto::walk::InputFile {
            path: PathBuf::from("ep01.srt"),
            name: "ep01.srt".to_string(),
        }];
        assert!(apply_ext_filter(&mut only_subs, &["mkv".to_string()], "entry").is_err());

        // Empty filter is a no-op.
        let mut untouched = vec![pesto::walk::InputFile {
            path: PathBuf::from("ep01.srt"),
            name: "ep01.srt".to_string(),
        }];
        apply_ext_filter(&mut untouched, &[], "entry").unwrap();
        assert_eq!(untouched.len(), 1);
    }

    #[test]
    fn derive_season_nzb_path_prefers_explicit_out() {
        let path = derive_season_nzb_path(
            Some(Path::new("/custom/out.nzb")),
            Path::new("/downloads/Show.S01"),
            Some("/nzbs"),
        );
        assert_eq!(path, PathBuf::from("/custom/out.nzb"));
    }

    #[test]
    fn derive_season_nzb_path_names_after_entry_under_nzb_dir() {
        let path = derive_season_nzb_path(None, Path::new("/downloads/Show.S01"), Some("/nzbs"));
        assert_eq!(path, PathBuf::from("/nzbs/Show.S01.nzb"));
    }

    #[test]
    fn derive_season_nzb_path_names_dot_after_current_directory() {
        let cwd = std::env::current_dir().unwrap();
        let expected_name = cwd
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "season".to_string());

        let path = derive_season_nzb_path(None, Path::new("."), Some("/nzbs"));

        assert_eq!(
            path,
            PathBuf::from("/nzbs").join(format!("{expected_name}.nzb"))
        );
    }

    #[test]
    fn derive_season_nzb_path_falls_back_to_cwd_relative_name() {
        let path = derive_season_nzb_path(None, Path::new("/downloads/Show.S01"), None);
        assert_eq!(path, PathBuf::from("Show.S01.nzb"));
    }
}
