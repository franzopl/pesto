//! Path detection: video files, series folders, Blu-ray/DVD disc roots.

use std::path::{Path, PathBuf};

pub(super) fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| super::VIDEO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Detect series directories by the SXX or SXXEXX pattern in the folder name.
pub(super) fn is_series_folder(name: &str) -> bool {
    // Matches S01, S01E01, s02, etc. not preceded by a letter.
    let upper = name.to_uppercase();
    let bytes = upper.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'S' {
            let prev_is_letter = i > 0 && bytes[i - 1].is_ascii_alphabetic();
            if prev_is_letter {
                continue;
            }
            let rest = &upper[i + 1..];
            let digits: usize = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits >= 2 {
                return true;
            }
        }
    }
    false
}

/// Return true when at least one direct subdirectory of `dir` contains a video file.
///
/// Used to distinguish movie folders (flat, one MKV at root) from course or
/// collection folders where lesson/episode files are organised into subdirectories.
pub(super) fn has_video_in_subdirs(dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            let mut vids = Vec::new();
            collect_videos(&entry.path(), &mut vids);
            if !vids.is_empty() {
                return true;
            }
        }
    }
    false
}

/// Return the alphabetically first video file directly inside `dir` (non-recursive).
///
/// Used for movie folders: the MKV is at the root alongside subtitle or NFO
/// companions, not nested inside a subdirectory.
pub(super) fn find_root_video(dir: &Path) -> Option<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return None;
    };
    let mut candidates: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        .map(|e| e.path())
        .filter(|p| is_video(p))
        .collect();
    candidates.sort();
    candidates.into_iter().next()
}

/// Return the alphabetically first video file inside `dir`, recursing into sub-dirs.
pub(super) fn find_first_video(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    collect_videos(dir, &mut candidates);
    candidates.sort();
    candidates.into_iter().next()
}

pub(super) fn collect_videos(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_videos(&child, out);
        } else if is_video(&child) {
            out.push(child);
        }
    }
}

/// Returns `true` when `paths` point at a Blu-ray disc structure, i.e. the case
/// where [`generate`] will invoke `bdinfo`. Used to decide whether a caller's
/// progress message should mention `bdinfo` specifically.
pub fn looks_like_bluray(paths: &[PathBuf]) -> bool {
    paths.len() == 1 && paths[0].is_dir() && !find_bluray_disc_roots(&paths[0]).is_empty()
}

/// Find Blu-ray disc roots by locating BDMV/index.bdmv anywhere under `path`.
pub(super) fn find_bluray_disc_roots(path: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    collect_bluray_roots(path, &mut roots);
    roots.sort();
    roots.dedup();
    roots
}

pub(super) fn collect_bluray_roots(dir: &Path, roots: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let child = entry.path();
        if child.is_dir() {
            // Skip BDMV/BACKUP/ — the Blu-ray spec mandates a backup of BDMV
            // metadata there, including a duplicate index.bdmv, but it has no
            // STREAM/ folder so it would produce a phantom disc section.
            let is_backup = child
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.eq_ignore_ascii_case("BACKUP"))
                .unwrap_or(false);
            let parent_is_bdmv = dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.eq_ignore_ascii_case("BDMV"))
                .unwrap_or(false);
            if !(is_backup && parent_is_bdmv) {
                collect_bluray_roots(&child, roots);
            }
        } else if child
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("index.bdmv"))
            .unwrap_or(false)
        {
            // index.bdmv -> BDMV/ -> disc root
            if let Some(bdmv) = child.parent() {
                if let Some(disc_root) = bdmv.parent() {
                    roots.push(disc_root.to_path_buf());
                }
            }
        }
    }
}

/// Return the largest `.m2ts` file inside `disc_root/BDMV/STREAM/`.
///
/// The largest file is the main feature; extras and menus are much smaller.
pub(super) fn find_main_m2ts(disc_root: &Path) -> Option<PathBuf> {
    let stream = disc_root.join("BDMV").join("STREAM");
    std::fs::read_dir(&stream)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("m2ts"))
                .unwrap_or(false)
        })
        .max_by_key(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
}
