//! NFO file generation.
//!
//! Generates a plain-text `.nfo` summary describing the upload:
//! - Single media file → `mediainfo` output for that file.
//! - Series directory (name contains SXX pattern) → `mediainfo` of first episode.
//! - Generic directory (courses, documents, etc.) → banner + stats + directory tree.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "m4v", "mov", "wmv", "flv", "ts", "m2ts", "vob", "divx", "xvid",
];

const MAX_FILENAME_LEN: usize = 42;

/// Generate NFO content for `paths` (the original input paths before any compression).
///
/// Runs `mediainfo` when a media file can be identified; for generic directories
/// produces a banner + statistics + directory tree. Returns `None` when there are
/// no paths.
pub fn generate(paths: &[PathBuf]) -> Option<String> {
    if paths.is_empty() {
        return None;
    }

    // Single file: mediainfo if video, plain listing otherwise.
    if paths.len() == 1 && paths[0].is_file() {
        if is_video(&paths[0]) {
            if let Ok(out) = run_mediainfo(&paths[0]) {
                return Some(out);
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

        if is_series_folder(&folder_name) {
            if let Some(first_ep) = find_first_video(dir) {
                if let Ok(out) = run_mediainfo(&first_ep) {
                    return Some(out);
                }
            }
        }

        return Some(build_folder_nfo(dir));
    }

    // Multiple paths: fall back to plain listing.
    Some(build_listing(paths))
}

/// Generate NFO content for a consolidated season (multiple source directories).
///
/// Finds the alphabetically first video file across all `dirs`, runs `mediainfo`
/// on it, and returns the output. Falls back to `generate(dirs)` when no video
/// is found or `mediainfo` fails.
pub fn generate_season(dirs: &[PathBuf]) -> Option<String> {
    if dirs.is_empty() {
        return None;
    }
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
            if let Ok(out) = run_mediainfo(&video) {
                return Some(out);
            }
        }
    }
    // Fallback: plain listing.
    generate(dirs)
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

/// Detect series directories by the SXX or SXXEXX pattern in the folder name.
fn is_series_folder(name: &str) -> bool {
    // Matches S01, S01E01, s02, etc. not preceded by a letter.
    let upper = name.to_uppercase();
    let bytes = upper.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'S' {
            let prev_is_letter = i > 0 && bytes[i - 1].is_ascii_alphabetic();
            if prev_is_letter {
                continue;
            }
            // expect at least two digits after S
            let rest = &upper[i + 1..];
            let digits: usize = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits >= 2 {
                return true;
            }
        }
    }
    false
}

/// Return the alphabetically first video file inside `dir`, recursing into sub-dirs.
fn find_first_video(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    collect_videos(dir, &mut candidates);
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

fn format_size(bytes: u64) -> String {
    let mut val = bytes as f64;
    for unit in &["B", "KB", "MB", "GB"] {
        if val < 1024.0 {
            if *unit == "B" {
                return format!("{} B", bytes);
            }
            return format!("{val:.2} {unit}");
        }
        val /= 1024.0;
    }
    format!("{val:.2} TB")
}

fn center(text: &str, width: usize) -> String {
    if text.len() >= width {
        return text.to_string();
    }
    let pad = (width - text.len()) / 2;
    format!("{:pad$}{}{:pad$}", "", text, "")
}

fn default_banner() -> &'static str {
    ".------------------------------------------------------------------------------.\n\
     |                                                                              |\n\
     |    ____  _____ ____ _____ ___                                               |\n\
     |   |  _ \\| ____/ ___|_   _/ _ \\                                             |\n\
     |   | |_) |  _| \\___ \\ | || | | |                                            |\n\
     |   |  __/| |___ ___) || || |_| |                                            |\n\
     |   |_|   |_____|____/ |_| \\___/                                             |\n\
     |                                                                              |\n\
     |                     usenet poster                                            |\n\
     |                                                                              |\n\
     '------------------------------------------------------------------------------'"
}

/// Collect all files under `dir` (skipping `.nfo` with the same base name).
fn collect_all_files(dir: &Path, nfo_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_files_recursive(dir, nfo_name, &mut out);
    out
}

fn collect_files_recursive(dir: &Path, nfo_name: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_files_recursive(&child, nfo_name, out);
        } else {
            let fname = child
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if fname != nfo_name {
                out.push(child);
            }
        }
    }
}

struct TreeState {
    lines: Vec<String>,
    file_count: usize,
    dir_count: usize,
}

fn build_tree(dir: &Path, nfo_name: &str, file_sizes: &HashMap<PathBuf, u64>) -> TreeState {
    let mut state = TreeState {
        lines: Vec::new(),
        file_count: 0,
        dir_count: 0,
    };
    let root_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string_lossy().into_owned());
    state.lines.push(root_name);
    walk_tree(dir, "", nfo_name, file_sizes, &mut state);
    state
}

fn walk_tree(
    current_dir: &Path,
    prefix: &str,
    nfo_name: &str,
    file_sizes: &HashMap<PathBuf, u64>,
    state: &mut TreeState,
) {
    let Ok(entries) = std::fs::read_dir(current_dir) else {
        return;
    };
    let mut contents: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy() != nfo_name)
                .unwrap_or(true)
        })
        .collect();
    contents.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .cmp(
                &b.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase(),
            )
    });

    let total = contents.len();
    for (i, path) in contents.iter().enumerate() {
        let is_last = i == total - 1;
        let pointer = if is_last { "`-- " } else { "|-- " };
        let new_prefix = format!("{}{}", prefix, if is_last { "    " } else { "|   " });
        let item_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if path.is_dir() {
            state
                .lines
                .push(format!("{}{}{}", prefix, pointer, item_name));
            state.dir_count += 1;
            walk_tree(path, &new_prefix, nfo_name, file_sizes, state);
        } else {
            state.file_count += 1;
            let display_name = if item_name.len() > MAX_FILENAME_LEN {
                format!("{}...", &item_name[..MAX_FILENAME_LEN])
            } else {
                item_name.clone()
            };
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            let size = file_sizes.get(&canonical).copied().unwrap_or(0);
            let size_str = format_size(size);
            state.lines.push(format!(
                "{}{}{} [{}]",
                prefix, pointer, display_name, size_str
            ));
        }
    }
}

/// Build a rich NFO for a generic directory (banner + stats + tree).
fn build_folder_nfo(dir: &Path) -> String {
    let folder_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string_lossy().into_owned());
    let nfo_name = format!("{folder_name}.nfo");

    let all_files = collect_all_files(dir, &nfo_name);

    let mut file_sizes: HashMap<PathBuf, u64> = HashMap::new();
    let mut total_size: u64 = 0;
    for f in &all_files {
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        let canonical = f.canonicalize().unwrap_or_else(|_| f.clone());
        file_sizes.insert(canonical, size);
        total_size += size;
    }

    let tree = build_tree(dir, &nfo_name, &file_sizes);

    let mut ext_counts: HashMap<String, usize> = HashMap::new();
    for f in &all_files {
        let ext = f
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
            .unwrap_or_else(|| ".".to_string());
        *ext_counts.entry(ext).or_insert(0) += 1;
    }

    let mut lines: Vec<String> = Vec::new();

    for l in default_banner().lines() {
        lines.push(l.to_string());
    }
    lines.push(String::new());

    let title = folder_name.to_uppercase();
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(format!("|{}|", center(&title, 78)));
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(String::new());
    lines.push("-".repeat(80));
    lines.push(String::new());

    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(format!("|{}|", center("*** GENERAL STATISTICS ***", 78)));
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(String::new());
    lines.push(format!(
        "  > Total Size:         {}",
        format_size(total_size)
    ));
    lines.push(format!("  > Directories:        {}", tree.dir_count));
    lines.push(format!("  > Total Files:        {}", tree.file_count));
    lines.push("  > Files by Type:".to_string());

    let mut ext_vec: Vec<(String, usize)> = ext_counts.into_iter().collect();
    ext_vec.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (ext, count) in &ext_vec {
        let label = ext.trim_start_matches('.').to_uppercase();
        let label = if label.is_empty() { "NO EXT" } else { &label };
        lines.push(format!("    - {label}: {count} file(s)"));
    }

    lines.push(String::new());
    lines.push(String::new());
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(format!(
        "|{}|",
        center("*** FILE AND DIRECTORY STRUCTURE ***", 78)
    ));
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(String::new());
    lines.extend(tree.lines);
    lines.push(String::new());
    lines.push(format!(
        "{} directories, {} files, {}",
        tree.dir_count,
        tree.file_count,
        format_size(total_size)
    ));

    lines.join("\n")
}

/// Build a human-readable recursive listing of all paths (fallback for multiple paths).
fn build_listing(paths: &[PathBuf]) -> String {
    let mut buf = String::new();
    for root in paths {
        let name = root.file_name().unwrap_or(root.as_os_str());
        if root.is_file() {
            let size = root.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = writeln!(buf, "{} ({})", name.to_string_lossy(), format_size(size));
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
                "{}{}  ({})",
                indent,
                name.to_string_lossy(),
                format_size(size)
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

    // ── is_series_folder ─────────────────────────────────────────────────────

    #[test]
    fn series_folder_detection() {
        assert!(is_series_folder("Breaking.Bad.S01E01.mkv"));
        assert!(is_series_folder("Show.S02"));
        assert!(is_series_folder("My Series S03E05 720p"));
        assert!(!is_series_folder("Curso Python Avancado"));
        assert!(!is_series_folder("Documentary.2024"));
        // "AS01" should not match — 'A' is an alpha prefix
        assert!(!is_series_folder("AS01.mkv"));
    }

    // ── build_folder_nfo ─────────────────────────────────────────────────────

    #[test]
    fn folder_nfo_contains_stats_and_tree() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("module1");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("lesson.pdf"), b"pdf content").unwrap();
        fs::write(dir.path().join("readme.txt"), b"hello").unwrap();

        let nfo = build_folder_nfo(dir.path());
        assert!(nfo.contains("GENERAL STATISTICS"));
        assert!(nfo.contains("FILE AND DIRECTORY STRUCTURE"));
        assert!(nfo.contains("lesson.pdf"));
        assert!(nfo.contains("readme.txt"));
        assert!(nfo.contains("|--") || nfo.contains("`--"));
    }

    #[test]
    fn folder_nfo_shows_formatted_sizes() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file.txt"), vec![0u8; 2048]).unwrap();

        let nfo = build_folder_nfo(dir.path());
        assert!(nfo.contains("KB"));
    }

    // ── generate ─────────────────────────────────────────────────────────────

    #[test]
    fn generate_returns_none_for_empty_paths() {
        assert!(generate(&[]).is_none());
    }

    #[test]
    fn generate_falls_back_to_listing_for_non_video() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("data.nzb");
        fs::write(&f, b"content").unwrap();

        let result = generate(&[f]);
        assert!(result.is_some());
        let listing = result.unwrap();
        assert!(listing.contains("data.nzb"));
    }

    #[test]
    fn generate_generic_dir_produces_rich_nfo() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("notes.txt"), b"study notes").unwrap();
        fs::write(dir.path().join("slides.pdf"), b"slides").unwrap();

        let result = generate(&[dir.path().to_path_buf()]);
        assert!(result.is_some());
        let nfo = result.unwrap();
        assert!(nfo.contains("GENERAL STATISTICS"));
        assert!(nfo.contains("notes.txt"));
    }

    #[test]
    fn find_media_file_returns_alphabetically_first() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("ep02.mkv");
        let b = dir.path().join("ep01.mkv");
        fs::write(&a, b"").unwrap();
        fs::write(&b, b"").unwrap();

        let result = find_first_video(dir.path());
        assert_eq!(result.unwrap().file_name().unwrap(), "ep01.mkv");
    }
}
