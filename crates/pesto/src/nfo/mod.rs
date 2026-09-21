//! NFO file generation.
//!
//! Generates a plain-text `.nfo` summary describing the upload:
//! - Single media file → `mediainfo` output for that file.
//! - Series directory (name contains SXX pattern) → `mediainfo` of first episode.
//! - Generic directory (courses, documents, etc.) → banner + stats + directory tree.
//!
//! Module map: `detect` classifies the input, `mediainfo` runs the external
//! probes, `mpls` and `dvd` read disc metadata, and `render` formats the
//! banners, trees and listings.

use std::path::{Path, PathBuf};

use tracing::{debug, warn};

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "m4v", "mov", "wmv", "flv", "ts", "m2ts", "vob", "divx", "xvid",
];

const MAX_FILENAME_LEN: usize = 42;

/// Truncate `name` to at most `max_bytes` UTF-8 bytes, then append `...`.
///
/// Must never slice inside a multi-byte character (e.g. `é` is 2 bytes).
fn truncate_filename(name: &str, max_bytes: usize) -> String {
    if name.len() <= max_bytes {
        return name.to_string();
    }
    let end = name.floor_char_boundary(max_bytes);
    format!("{}...", &name[..end])
}

/// Generate NFO content for `paths` (the original input paths before any compression).
///
/// Runs `mediainfo` when a media file can be identified; for generic directories
/// produces a banner + statistics + directory tree. Returns `None` when there are
/// no paths.
pub fn generate(paths: &[PathBuf]) -> Option<String> {
    if paths.is_empty() {
        debug!("nfo::generate called with no paths — skipping");
        return None;
    }

    debug!(paths = paths.len(), "generating NFO");

    // Single file: mediainfo if video, plain listing otherwise.
    if paths.len() == 1 && paths[0].is_file() {
        if is_video(&paths[0]) {
            debug!(path = %paths[0].display(), "running mediainfo on single video file");
            match run_mediainfo(&paths[0]) {
                Ok(out) => return Some(out),
                Err(e) => {
                    warn!(path = %paths[0].display(), error = %e, "mediainfo failed; falling back to listing")
                }
            }
        }
        return Some(build_listing(paths));
    }

    // Directory: check if series → mediainfo; otherwise → rich tree.
    if paths.len() == 1 && paths[0].is_dir() {
        let dir = &paths[0];
        let folder_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Blu-ray disc structure: BDMV/index.bdmv
        let bd_roots = find_bluray_disc_roots(dir);
        if !bd_roots.is_empty() {
            debug!(discs = bd_roots.len(), folder = %folder_name, "detected Blu-ray structure");
            let mut sections: Vec<String> = Vec::new();
            for root in &bd_roots {
                let disc_label = root
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.display().to_string());
                let mi = match run_bdinfo(root) {
                    Some(bdinfo_out) => {
                        debug!(root = %root.display(), "bdinfo-rs-core succeeded for Blu-ray disc");
                        bdinfo_out
                    }
                    None => {
                        warn!(root = %root.display(), "bdinfo-rs-core failed — falling back to mediainfo");
                        match find_main_mpls(root).or_else(|| find_main_m2ts(root)) {
                            Some(media) => {
                                debug!(media = %media.display(), "running mediainfo on Blu-ray main feature");
                                let mi = match run_mediainfo(&media) {
                                    Ok(out) => out,
                                    Err(e) => {
                                        warn!(media = %media.display(), error = %e, "mediainfo failed for Blu-ray main feature");
                                        format!("[mediainfo failed for {}: {}]", media.display(), e)
                                    }
                                };
                                let is_mpls = media
                                    .extension()
                                    .and_then(|e| e.to_str())
                                    .map(|e| e.eq_ignore_ascii_case("mpls"))
                                    .unwrap_or(false);
                                if is_mpls {
                                    let lang_map = mpls_language_map(&media);
                                    debug!(pid_count = lang_map.len(), "parsed MPLS language map");
                                    inject_language_tags(&mi, &lang_map)
                                } else {
                                    mi
                                }
                            }
                            None => {
                                warn!(root = %root.display(), "no MPLS or M2TS found in BDMV");
                                "[no playable stream found]\n".to_owned()
                            }
                        }
                    }
                };
                sections.push(format!("=== Blu-ray Disc: {disc_label} ===\n{mi}\n"));
            }
            return Some(sections.join("\n"));
        }

        // DVD disc structure: VIDEO_TS/ with IFO files.
        let disc_roots = find_dvd_disc_roots(dir);
        if !disc_roots.is_empty() {
            debug!(discs = disc_roots.len(), folder = %folder_name, "detected DVD structure");
            let mut sections: Vec<String> = Vec::new();
            for root in &disc_roots {
                let title_ifo = find_title_ifo(root).unwrap_or_else(|| {
                    let video_ts = root.join("VIDEO_TS");
                    if video_ts.is_dir() {
                        video_ts.join("VTS_01_0.IFO")
                    } else {
                        root.join("VTS_01_0.IFO")
                    }
                });
                debug!(ifo = %title_ifo.display(), "running mediainfo on title IFO");
                let mi = match run_mediainfo(&title_ifo) {
                    Ok(out) => inject_dvd_language_tags(&out, &title_ifo),
                    Err(e) => {
                        warn!(ifo = %title_ifo.display(), error = %e, "mediainfo failed for DVD IFO");
                        format!("[mediainfo failed for {}: {}]", title_ifo.display(), e)
                    }
                };
                let disc_label = root
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.display().to_string());
                sections.push(format!("=== DVD Disc: {disc_label} ===\n{mi}\n"));
            }
            return Some(sections.join("\n"));
        }

        // Series folders (S01 / S01E01 pattern): run mediainfo on the first
        // episode regardless of how episodes are organised inside the folder.
        if is_series_folder(&folder_name) {
            debug!(folder = %folder_name, "detected series folder — looking for first video");
            if let Some(first_ep) = find_first_video(dir) {
                debug!(episode = %first_ep.display(), "running mediainfo on first episode");
                match run_mediainfo(&first_ep) {
                    Ok(out) => return Some(out),
                    Err(e) => {
                        warn!(episode = %first_ep.display(), error = %e, "mediainfo failed; falling back to folder NFO")
                    }
                }
            } else {
                debug!("no video file found in series folder; using folder NFO");
            }
            return Some(build_folder_nfo(dir));
        }

        // If any subdirectory contains video files the folder is a course,
        // collection, or multi-episode set — the tree structure is the useful
        // output, not a mediainfo report for one arbitrary file.
        if has_video_in_subdirs(dir) {
            debug!("video files found in subdirectories — using folder NFO");
            return Some(build_folder_nfo(dir));
        }

        // Flat folder (no videos inside subdirs): if there is a video at the
        // root it is a movie or single-episode folder → run mediainfo on it.
        if let Some(first_video) = find_root_video(dir) {
            debug!(video = %first_video.display(), "running mediainfo on root video file");
            match run_mediainfo(&first_video) {
                Ok(out) => return Some(out),
                Err(e) => {
                    warn!(video = %first_video.display(), error = %e, "mediainfo failed; falling back to folder NFO")
                }
            }
        } else {
            debug!("no video file found; using folder NFO");
        }

        return Some(build_folder_nfo(dir));
    }

    // Multiple paths: fall back to plain listing.
    debug!("multiple paths — using plain listing");
    Some(build_listing(paths))
}

/// Generate NFO content for a consolidated season (multiple source directories).
///
/// Finds the alphabetically first video file across all `dirs`, runs `mediainfo`
/// on it, and returns the output. Falls back to `generate(dirs)` when no video
/// is found or `mediainfo` fails.
pub fn generate_season(dirs: &[PathBuf]) -> Option<String> {
    if dirs.is_empty() {
        debug!("nfo::generate_season called with no dirs — skipping");
        return None;
    }

    debug!(dirs = dirs.len(), "generating season NFO");

    // Collect all directories, sorted, so episode order is stable.
    let mut sorted_dirs: Vec<&PathBuf> = dirs.iter().collect();
    sorted_dirs.sort();
    for dir in sorted_dirs {
        let first = if dir.is_dir() {
            find_first_video(dir)
        } else if is_video(dir) {
            Some(dir.clone())
        } else {
            None
        };
        if let Some(video) = first {
            debug!(video = %video.display(), "running mediainfo for season NFO");
            match run_mediainfo(&video) {
                Ok(out) => return Some(out),
                Err(e) => {
                    warn!(video = %video.display(), error = %e, "mediainfo failed for season entry")
                }
            }
        }
    }
    // Fallback: plain listing.
    debug!("mediainfo unavailable for all season entries; falling back to listing");
    generate(dirs)
}

/// Write the NFO content to `path`, creating or overwriting it.
pub fn write(path: &Path, content: &str) -> std::io::Result<()> {
    debug!(path = %path.display(), bytes = content.len(), "writing NFO");
    std::fs::write(path, content.as_bytes())
}

// ── helpers ──────────────────────────────────────────────────────────────────
mod detect;
mod dvd;
mod mediainfo;
mod mpls;
mod render;
pub use detect::looks_like_bluray;
use detect::*;
use dvd::*;
use mediainfo::*;
use mpls::*;
use render::*;

#[cfg(test)]
mod tests;
