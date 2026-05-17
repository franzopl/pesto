//! NFO file generation.
//!
//! Generates a plain-text `.nfo` summary describing the upload:
//! - Single media file → `mediainfo` output for that file.
//! - Directory with video files → `mediainfo` output for the first episode
//!   (lowest-sorted file whose extension matches a common video format).
//! - Everything else → a recursive directory/file listing.

use std::fmt::Write as _;
use std::path::{Path, PathBuf};

/// Video extensions that trigger the `mediainfo` path.
const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "m4v", "mov", "wmv", "flv", "ts", "m2ts", "vob", "divx", "xvid",
];

/// Generate NFO content for `paths` (the original input paths before any compression).
///
/// Runs `mediainfo` when a media file can be identified; falls back to a plain
/// directory listing otherwise. Returns `None` when there are no paths.
pub fn generate(paths: &[PathBuf]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }

    // Find a representative media file.
    let media_target = find_media_file(paths);
    if let Some(ref target) = media_target {
        if let Ok(output) = run_mediainfo(target) {
            return Some(output);
        }
    }

    // Fall back to a listing.
    Some(build_listing(paths))
}

/// Write the NFO content to `path`, creating or overwriting it.
pub fn write(path: &Path, content: &str) -> std::io::Result<()> {
    std::fs::write(path, content.as_bytes())
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Return the first (alphabetically sorted) video file found in `paths`,
/// recursing into directories.
fn find_media_file(paths: &[PathBuf]) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();

    for p in paths {
        if p.is_file() {
            if is_video(p) {
                candidates.push(p.clone());
            }
        } else if p.is_dir() {
            collect_videos(p, &mut candidates);
        }
    }

    candidates.sort();
    candidates.into_iter().next()
}

fn collect_videos(dir: &Path, out: &mut Vec<PathBuf>) {
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

fn run_mediainfo(path: &Path) -> std::io::Result<String> {
    let output = std::process::Command::new("mediainfo").arg(path).output()?;
    if !output.status.success() {
        return Err(std::io::Error::other("mediainfo exited non-zero"));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Build a human-readable recursive listing of all paths.
fn build_listing(paths: &[PathBuf]) -> String {
    let mut buf = String::new();
    for root in paths {
        let name = root.file_name().unwrap_or(root.as_os_str());
        if root.is_file() {
            let size = root.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = writeln!(buf, "{} ({} bytes)", name.to_string_lossy(), size);
        } else if root.is_dir() {
            let _ = writeln!(buf, "{}/", name.to_string_lossy());
            append_dir_listing(root, &mut buf, 1);
        }
    }
    buf
}

fn append_dir_listing(dir: &Path, buf: &mut String, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let indent = "  ".repeat(depth);
    let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    children.sort();
    for child in children {
        let name = child.file_name().unwrap_or(child.as_os_str());
        if child.is_dir() {
            let _ = writeln!(buf, "{}{}/", indent, name.to_string_lossy());
            append_dir_listing(&child, buf, depth + 1);
        } else {
            let size = child.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = writeln!(
                buf,
                "{}{}  ({} bytes)",
                indent,
                name.to_string_lossy(),
                size
            );
        }
    }
}
