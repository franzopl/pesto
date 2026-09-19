//! Single-upload context, results and behavior-neutral planning stages.

pub(super) mod compression;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::PostedSegment;

use super::cleanup::CleanupMode;
use super::output::expand_tilde;

/// Parameters for a single upload job that do not change between entries.
#[derive(Clone)]
pub(super) struct UploadParams {
    pub(super) config: Arc<Config>,
    /// The raw `--password` value, used to distinguish a bare flag.
    pub(super) archive_password_raw: Option<String>,
    pub(super) nzb_default: Option<String>,
    pub(super) json_mode: bool,
    pub(super) out: Option<PathBuf>,
    pub(super) write_history: bool,
    pub(super) renderer_opts: pesto::progress::RendererOptions,
    /// Lowercased extensions with any leading dot removed.
    pub(super) ext_filter: Vec<String>,
    pub(super) cleanup_mode: CleanupMode,
}

/// Result of one entry upload, including data needed by season orchestration.
pub(super) struct UploadResult {
    pub(super) segments: Vec<PostedSegment>,
    pub(super) groups: Vec<String>,
    pub(super) cancelled: bool,
    pub(super) had_failures: bool,
    /// STAT checks that failed without confirming an article as missing.
    pub(super) inconclusive: Vec<String>,
    pub(super) total_bytes: u64,
    pub(super) nzb_path: Option<PathBuf>,
    /// Files actually posted, after optional compression.
    pub(super) posted_paths: Vec<PathBuf>,
    /// Deferred compression directory retained for season-wide PAR2.
    pub(super) compress_temp_dir: Option<PathBuf>,
}

/// Per-phase wall-clock timing accumulated during one upload.
#[derive(Default)]
pub(super) struct PhaseTimings {
    pub(super) compress_ms: Option<u128>,
    /// Includes the concurrent streaming check/repost drain.
    pub(super) post_ms: Option<u128>,
}

/// Paths derived before compression changes the input list or filenames.
pub(super) struct UploadPaths {
    pub(super) nzb_out_path: Option<String>,
    pub(super) nzb_user_dest: Option<PathBuf>,
    pub(super) resume_path: Option<PathBuf>,
}

/// Resolve the archive password for one upload.
///
/// A season-forced password wins over an explicit password. A bare
/// `--password` generates a fresh value per entry so independent `--each` and
/// `--watch` uploads do not silently share credentials.
pub(super) fn resolve_entry_password(
    forced: Option<&str>,
    explicit: Option<&str>,
    raw: Option<&str>,
) -> Option<String> {
    forced
        .or(explicit)
        .map(str::to_string)
        .or_else(|| (raw == Some("")).then(pesto::compress::random_password))
}

/// Plan NZB, user-destination and resume-state paths for one entry.
pub(super) fn plan_upload_paths(
    params: &UploadParams,
    entry_paths: &[PathBuf],
    inputs: &[pesto::walk::InputFile],
) -> UploadPaths {
    let nzb_stem = params
        .out
        .as_ref()
        .map(|path| {
            path.with_extension("")
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        })
        .or_else(|| {
            params.nzb_default.as_deref().map(|default| {
                PathBuf::from(default)
                    .with_extension("")
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .or_else(|| {
            entry_paths
                .first()
                .and_then(|path| {
                    path.file_name().map(|name| {
                        if path.is_dir() {
                            name.to_string_lossy().into_owned()
                        } else {
                            Path::new(name)
                                .file_stem()
                                .unwrap_or(name)
                                .to_string_lossy()
                                .into_owned()
                        }
                    })
                })
                .or_else(|| upload_root(inputs))
                .or_else(|| {
                    inputs.first().map(|input| {
                        let top = input.name.split('/').next().unwrap_or(&input.name);
                        if input.name.contains('/') {
                            top.to_owned()
                        } else {
                            PathBuf::from(top)
                                .file_stem()
                                .unwrap_or_default()
                                .to_string_lossy()
                                .into_owned()
                        }
                    })
                })
        });

    let nzb_user_dest = params.out.clone().or_else(|| {
        nzb_stem.as_deref().and_then(|stem| {
            if let Some(directory) = params.config.nzb_dir.as_deref() {
                Some(expand_tilde(directory).join(format!("{stem}.nzb")))
            } else {
                entry_paths
                    .first()
                    .and_then(|path| {
                        if path.is_dir() {
                            Some(path.as_path())
                        } else {
                            path.parent()
                        }
                    })
                    .map(|directory| directory.join(format!("{stem}.nzb")))
            }
        })
    });

    let resume_path = nzb_user_dest
        .as_ref()
        .map(|path| path.with_extension("pesto-state"))
        .or_else(|| {
            nzb_stem
                .as_deref()
                .map(|stem| PathBuf::from(stem).with_extension("pesto-state"))
        });

    UploadPaths {
        nzb_out_path: nzb_stem,
        nzb_user_dest,
        resume_path,
    }
}

/// Collect the unique filesystem paths to pass to the compressor.
pub(super) fn collect_compress_roots(inputs: &[pesto::walk::InputFile]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for input in inputs {
        let depth = input.name.split('/').count();
        let root = if depth <= 1 {
            input.path.clone()
        } else {
            // Strip `depth - 1` trailing components (everything in `name`
            // after the top-level folder) to land on the top-level folder
            // itself, not its parent. `ancestors().nth(k)` strips `k`
            // trailing components, so `nth(depth)` was one level too high —
            // it landed on the folder's *parent*, which under `--watch`
            // silently pulled in sibling top-level entries (issue #67).
            input
                .path
                .ancestors()
                .nth(depth - 1)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| input.path.clone())
        };
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    if roots.is_empty() {
        inputs.iter().map(|f| f.path.clone()).collect()
    } else {
        roots
    }
}

/// The single root folder shared by every input, or `None` for loose files.
pub(super) fn upload_root(inputs: &[pesto::walk::InputFile]) -> Option<String> {
    let mut root: Option<&str> = None;
    for input in inputs {
        let (candidate, _) = input.name.split_once('/')?;
        match root {
            Some(existing) if existing != candidate => return None,
            _ => root = Some(candidate),
        }
    }
    root.map(str::to_string)
}

/// Decide the obfuscated archive stem for a `--compress`+`--obfuscate` run:
/// reuse the value a compatible prior `--resume` run recorded, or generate a
/// fresh one and record it for a *future* `--resume` to find.
///
/// Only touches the resume-state file when `--resume` is passed. Doing this
/// unconditionally would conflict with
/// `poster::post_files_with_progress_and_cancel`'s own handling of the same
/// file: when `--resume` is *not* passed, that function deliberately starts
/// from a fresh, empty state (see issue #18 — trusting whatever happens to
/// be on disk without being asked is the exact hazard it guards against),
/// which would silently erase whatever this function wrote moments earlier.
/// The practical result is the same rule as everywhere else in resume
/// handling: a stem only survives into a later run when every run in the
/// chain, including the first, passes `--resume`.
pub(super) fn reuse_or_generate_archive_stem(
    resume_path: Option<&Path>,
    config: &Config,
) -> String {
    if !config.resume {
        return pesto::article::obfuscated_name();
    }
    let Some(rp) = resume_path else {
        return pesto::article::obfuscated_name();
    };
    let fingerprint = pesto::resume::RunFingerprint::from_config(config);
    let mut state = pesto::resume::ResumeState::load(rp).unwrap_or_default();
    // Normalizes the loaded state first: a fingerprint mismatch clears any
    // stale archive_stem (and segments/files) before we look at it, so an
    // incompatible prior run's name is never reused.
    state.validate_run(&fingerprint);
    if let Some(stem) = state.archive_stem() {
        return stem.to_string();
    }
    let stem = pesto::article::obfuscated_name();
    state.set_archive_stem(stem.clone());
    let _ = state.save(rp);
    stem
}

/// The posting flags that `resume::RunFingerprint` actually checks,
/// formatted for a copy-pasteable `--resume` retry command. A retry using
/// different values for any of these gets its resume state silently (and
/// safely) discarded by `validate_run` — printing them explicitly means a
/// copy-pasted retry command actually resumes instead of quietly re-posting
/// everything from scratch. Closes the gap issue #18 called out: "the
/// printed resume hint only suggests `pesto <file> --resume` and drops the
/// original flags".
pub(super) fn resume_flags_string(config: &Config) -> String {
    let obfuscate = match config.obfuscate {
        ObfuscateMode::None => "none",
        ObfuscateMode::Full => "full",
        ObfuscateMode::Light => "light",
        ObfuscateMode::FullShared => "full-shared",
        ObfuscateMode::Article => "article",
    };
    let mut flags = format!(
        "--article-size {} --obfuscate={obfuscate} --par2 {}",
        config.article_size, config.par2
    );
    if let Some(fmt) = &config.compress_format {
        flags.push_str(&format!(" --compress={fmt}"));
    }
    if config.file_counter {
        flags.push_str(" --file-counter");
    }
    if let Some(n) = config.par2_slice_size {
        flags.push_str(&format!(" --par2-slice-size {n}"));
    }
    if let Some(n) = config.par2_slice_count {
        flags.push_str(&format!(" --par2-slice-count {n}"));
    }
    if let Some(n) = config.par2_recovery_count {
        flags.push_str(&format!(" --par2-recovery-count {n}"));
    }
    if let Some(v) = &config.compress_volume_size {
        flags.push_str(&format!(" --compress-volume-size {v}"));
    }
    if config.line_length != pesto::yenc::DEFAULT_LINE_LENGTH {
        flags.push_str(&format!(" --line-length {}", config.line_length));
    }
    flags
}

#[cfg(test)]
mod tests {
    use super::*;
    use pesto::config::{FileConfig, Overrides};
    use pesto::walk::InputFile;

    fn test_config(
        article_size: usize,
        obfuscate: ObfuscateMode,
        compress_format: Option<&str>,
        par2: u8,
    ) -> Config {
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        Config::resolve(
            file,
            Overrides {
                article_size: Some(article_size),
                obfuscate: Some(obfuscate),
                compress_format: compress_format.map(str::to_string),
                par2: Some(par2),
                ..Default::default()
            },
        )
        .unwrap()
    }

    #[test]
    fn resume_flags_string_includes_every_fingerprinted_flag() {
        let config = test_config(384_000, ObfuscateMode::Full, None, 10);
        assert_eq!(
            resume_flags_string(&config),
            "--article-size 384000 --obfuscate=full --par2 10"
        );
    }

    #[test]
    fn resume_flags_string_includes_compress_only_when_set() {
        let none_compressed = test_config(768_000, ObfuscateMode::None, None, 0);
        assert!(!resume_flags_string(&none_compressed).contains("--compress"));

        let compressed = test_config(768_000, ObfuscateMode::FullShared, Some("7z"), 5);
        assert_eq!(
            resume_flags_string(&compressed),
            // `file_counter` defaults to true for `full-shared` — see
            // `Config::resolve`'s obfuscate-mode-dependent default.
            "--article-size 768000 --obfuscate=full-shared --par2 5 --compress=7z --file-counter"
        );
    }

    // ── reuse_or_generate_archive_stem ─────────────────────────────────────

    #[test]
    fn archive_stem_without_resume_is_always_fresh_and_untracked() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = false;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let a = reuse_or_generate_archive_stem(Some(&state_path), &config);
        let b = reuse_or_generate_archive_stem(Some(&state_path), &config);

        assert_ne!(
            a, b,
            "without --resume, every call must generate a fresh name"
        );
        assert!(
            !state_path.exists(),
            "without --resume, nothing should be written to disk"
        );
    }

    #[test]
    fn archive_stem_without_a_resume_path_is_fresh() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let a = reuse_or_generate_archive_stem(None, &config);
        let b = reuse_or_generate_archive_stem(None, &config);
        assert_ne!(a, b);
    }

    #[test]
    fn archive_stem_is_generated_and_recorded_on_first_resume_run() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let stem = reuse_or_generate_archive_stem(Some(&state_path), &config);

        let state = pesto::resume::ResumeState::load(&state_path).unwrap();
        assert_eq!(state.archive_stem(), Some(stem.as_str()));
    }

    #[test]
    fn archive_stem_is_reused_on_a_compatible_resume_run() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let first = reuse_or_generate_archive_stem(Some(&state_path), &config);
        let second = reuse_or_generate_archive_stem(Some(&state_path), &config);

        assert_eq!(
            first, second,
            "a compatible resume run must reuse the same stem"
        );
    }

    #[test]
    fn archive_stem_is_regenerated_when_posting_parameters_changed() {
        let mut config = test_config(768_000, ObfuscateMode::Full, Some("7z"), 0);
        config.resume = true;
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("release.pesto-state");

        let first = reuse_or_generate_archive_stem(Some(&state_path), &config);

        // A later run using a different --article-size: the old stem
        // (recorded under a now-mismatched fingerprint) must not be reused.
        config.article_size = 384_000;
        let second = reuse_or_generate_archive_stem(Some(&state_path), &config);

        assert_ne!(
            first, second,
            "a fingerprint mismatch must not reuse the old stem"
        );
        let state = pesto::resume::ResumeState::load(&state_path).unwrap();
        assert_eq!(state.archive_stem(), Some(second.as_str()));
    }

    fn inputs(names: &[&str]) -> Vec<InputFile> {
        names
            .iter()
            .map(|n| InputFile {
                path: PathBuf::from(n),
                name: n.to_string(),
            })
            .collect()
    }

    #[test]
    fn upload_root_finds_a_single_shared_directory() {
        assert_eq!(
            upload_root(&inputs(&["Show/ep01.bin", "Show/extras/clip.bin"])),
            Some("Show".to_string())
        );
    }

    #[test]
    fn upload_root_is_none_for_loose_or_mixed_inputs() {
        assert_eq!(upload_root(&inputs(&["a.bin"])), None);
        assert_eq!(upload_root(&inputs(&["A/x.bin", "B/y.bin"])), None);
        assert_eq!(upload_root(&inputs(&["Show/ep01.bin", "loose.bin"])), None);
    }

    #[test]
    fn collect_compress_roots_loose_file_is_the_file_itself() {
        let files = vec![InputFile {
            path: PathBuf::from("/media/downloads/movie.mkv"),
            name: "movie.mkv".to_string(),
        }];
        assert_eq!(
            collect_compress_roots(&files),
            vec![PathBuf::from("/media/downloads/movie.mkv")]
        );
    }

    #[test]
    fn collect_compress_roots_directory_input_strips_correctly() {
        let files = vec![
            InputFile {
                path: PathBuf::from("/media/Show/ep01.mkv"),
                name: "Show/ep01.mkv".to_string(),
            },
            InputFile {
                path: PathBuf::from("/media/Show/ep02.mkv"),
                name: "Show/ep02.mkv".to_string(),
            },
        ];
        assert_eq!(
            collect_compress_roots(&files),
            vec![PathBuf::from("/media/Show")]
        );
    }

    #[test]
    fn collect_compress_roots_nested_subfolder_strips_to_top_level() {
        // Regression test for issue #67: a file nested two levels deep
        // inside the top-level folder (e.g. `Test1/Subs/en.srt`) must still
        // resolve to `Test1`, not to `Test1`'s parent.
        let files = vec![InputFile {
            path: PathBuf::from("/home/user/upload/Test1/Subs/en.srt"),
            name: "Test1/Subs/en.srt".to_string(),
        }];
        assert_eq!(
            collect_compress_roots(&files),
            vec![PathBuf::from("/home/user/upload/Test1")]
        );
    }

    #[test]
    fn collect_compress_roots_relative_folder_resolves_to_folder_itself() {
        // A directory passed with a bare relative path (e.g. `pesto Test1
        // --compress` run from Test1's parent) must still resolve to
        // `Test1`, not fall back to per-file roots or an empty path.
        let files = vec![
            InputFile {
                path: PathBuf::from("Test1/movie.mkv"),
                name: "Test1/movie.mkv".to_string(),
            },
            InputFile {
                path: PathBuf::from("Test1/movie.nfo"),
                name: "Test1/movie.nfo".to_string(),
            },
        ];
        assert_eq!(collect_compress_roots(&files), vec![PathBuf::from("Test1")]);
    }

    #[test]
    fn collect_compress_roots_does_not_leak_sibling_top_level_folders() {
        // Regression test for issue #67: compressing `Test1` under
        // `--watch` must never resolve to the watch directory itself, or
        // sibling entries like `Test2` end up bundled into the same
        // archive.
        let files = vec![
            InputFile {
                path: PathBuf::from("/home/user/upload/Test1/movie.mkv"),
                name: "Test1/movie.mkv".to_string(),
            },
            InputFile {
                path: PathBuf::from("/home/user/upload/Test1/movie.nfo"),
                name: "Test1/movie.nfo".to_string(),
            },
        ];
        let roots = collect_compress_roots(&files);
        assert_eq!(roots, vec![PathBuf::from("/home/user/upload/Test1")]);
        assert!(!roots.contains(&PathBuf::from("/home/user/upload")));
    }

    #[test]
    fn no_password_flag_means_no_password() {
        assert_eq!(resolve_entry_password(None, None, None), None);
    }

    #[test]
    fn explicit_password_is_reused_verbatim() {
        for raw in [None, Some(""), Some("mypass")] {
            assert_eq!(
                resolve_entry_password(None, Some("mypass"), raw),
                Some("mypass".to_string())
            );
        }
    }

    #[test]
    fn bare_password_flag_generates_a_password() {
        let password = resolve_entry_password(None, None, Some(""));
        assert_eq!(password.as_deref().map(str::len), Some(24));
    }

    #[test]
    fn bare_password_flag_is_unique_per_entry() {
        let first = resolve_entry_password(None, None, Some(""));
        let second = resolve_entry_password(None, None, Some(""));
        assert_ne!(first, second);
    }

    #[test]
    fn season_password_wins_over_explicit_and_bare_values() {
        assert_eq!(
            resolve_entry_password(Some("season-pw"), Some("explicit-pw"), Some("")),
            Some("season-pw".to_string())
        );
        assert_eq!(
            resolve_entry_password(Some("season-pw"), None, Some("")),
            Some("season-pw".to_string())
        );
    }
}
