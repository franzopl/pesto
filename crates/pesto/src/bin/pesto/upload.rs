//! Single-upload context, results and behavior-neutral planning stages.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pesto::config::Config;
use pesto::poster::PostedSegment;

use super::cleanup::CleanupMode;
use super::output::expand_tilde;
use super::upload_root;

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

#[cfg(test)]
mod tests {
    use super::*;

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
