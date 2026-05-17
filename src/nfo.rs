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
///
/// When `strip_path` is `true` (i.e. `--obfuscate=full`), the `Complete name`
/// line emitted by mediainfo is removed so the NFO does not reveal the original
/// file path to anyone who receives it alongside the `.nzb`.
pub fn generate(paths: &[PathBuf], strip_path: bool) -> Option<String> {
    if paths.is_empty() {
        return None;
    }

    // Find a representative media file.
    let media_target = find_media_file(paths);
    if let Some(ref target) = media_target {
        if let Ok(output) = run_mediainfo(target) {
            let content = if strip_path {
                strip_complete_name(&output)
            } else {
                output
            };
            return Some(content);
        }
    }

    // Fall back to a listing — directory listings always show real names, so
    // omit them entirely under full obfuscation.
    if strip_path {
        return None;
    }
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

/// Remove the `Complete name` line from mediainfo output so the file path is
/// not exposed when obfuscation is active.
fn strip_complete_name(output: &str) -> String {
    output
        .lines()
        .filter(|l| !l.trim_start().starts_with("Complete name"))
        .flat_map(|l| [l, "\n"])
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── is_video ─────────────────────────────────────────────────────────────

    #[test]
    fn is_video_known_extensions() {
        for ext in &["mkv", "mp4", "avi", "ts", "m2ts", "mov"] {
            let p = PathBuf::from(format!("file.{ext}"));
            assert!(is_video(&p), "{ext} should be recognised as video");
        }
    }

    #[test]
    fn is_video_unknown_extension() {
        assert!(!is_video(&PathBuf::from("file.txt")));
        assert!(!is_video(&PathBuf::from("file.nfo")));
        assert!(!is_video(&PathBuf::from("file.nzb")));
    }

    #[test]
    fn is_video_no_extension() {
        assert!(!is_video(&PathBuf::from("README")));
    }

    #[test]
    fn is_video_mixed_case() {
        assert!(is_video(&PathBuf::from("movie.MKV")));
        assert!(is_video(&PathBuf::from("clip.Mp4")));
    }

    // ── build_listing ─────────────────────────────────────────────────────────

    #[test]
    fn build_listing_single_file() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("sample.txt");
        fs::write(&f, b"hello").unwrap();

        let listing = build_listing(&[f]);
        assert!(listing.contains("sample.txt"));
        assert!(listing.contains("5 bytes"));
    }

    #[test]
    fn build_listing_empty_input() {
        let listing = build_listing(&[]);
        assert!(listing.is_empty());
    }

    #[test]
    fn build_listing_directory_with_nested_files() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("sub");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("a.txt"), b"aa").unwrap();
        fs::write(dir.path().join("b.txt"), b"bbb").unwrap();

        let listing = build_listing(&[dir.path().to_path_buf()]);
        assert!(listing.contains("b.txt"));
        assert!(listing.contains("sub/"));
        assert!(listing.contains("a.txt"));
    }

    // ── find_media_file ───────────────────────────────────────────────────────

    #[test]
    fn find_media_file_single_video() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("movie.mkv");
        fs::write(&f, b"").unwrap();

        let result = find_media_file(&[f.clone()]);
        assert_eq!(result, Some(f));
    }

    #[test]
    fn find_media_file_no_video() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("readme.txt");
        fs::write(&f, b"").unwrap();

        assert_eq!(find_media_file(&[f]), None);
    }

    #[test]
    fn find_media_file_returns_alphabetically_first() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("ep02.mkv");
        let b = dir.path().join("ep01.mkv");
        fs::write(&a, b"").unwrap();
        fs::write(&b, b"").unwrap();

        let result = find_media_file(&[dir.path().to_path_buf()]);
        assert_eq!(result.unwrap().file_name().unwrap(), "ep01.mkv");
    }

    #[test]
    fn find_media_file_recurses_into_subdirectory() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("season1");
        fs::create_dir(&sub).unwrap();
        let f = sub.join("ep01.mp4");
        fs::write(&f, b"").unwrap();

        let result = find_media_file(&[dir.path().to_path_buf()]);
        assert_eq!(result, Some(f));
    }

    // ── generate ─────────────────────────────────────────────────────────────

    #[test]
    fn generate_returns_none_for_empty_paths() {
        assert!(generate(&[], false).is_none());
    }

    #[test]
    fn generate_falls_back_to_listing_for_non_video() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("data.nzb");
        fs::write(&f, b"content").unwrap();

        let result = generate(&[f], false);
        assert!(result.is_some());
        let listing = result.unwrap();
        assert!(listing.contains("data.nzb"));
    }

    #[test]
    fn generate_returns_none_for_non_video_when_strip_path() {
        // With obfuscate=full, directory listings reveal real names, so the
        // NFO must be suppressed entirely when no media file is found.
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("data.nzb");
        fs::write(&f, b"content").unwrap();

        assert!(generate(&[f], true).is_none());
    }

    #[test]
    fn strip_complete_name_removes_only_that_line() {
        let input = "General\nComplete name                            : /home/user/movie.mkv\nFormat                                   : Matroska\n";
        let output = strip_complete_name(input);
        assert!(!output.contains("Complete name"));
        assert!(!output.contains("movie.mkv"));
        assert!(output.contains("General"));
        assert!(output.contains("Format"));
    }
}
