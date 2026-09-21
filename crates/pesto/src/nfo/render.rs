//! NFO rendering: banners, trees, folder listings.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub(super) fn format_size(bytes: u64) -> String {
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

pub(super) fn center(text: &str, width: usize) -> String {
    if text.len() >= width {
        return text.to_string();
    }
    let pad = (width - text.len()) / 2;
    format!("{:pad$}{}{:pad$}", "", text, "")
}

pub(super) fn default_banner() -> &'static str {
    ".------------------------------------------------------------------------------.\n\
     |                                                                              |\n\
     |    ____  _____ ____ _____ ___                                                |\n\
     |   |  _ \\| ____/ ___|_   _/ _ \\                                               |\n\
     |   | |_) |  _| \\___ \\ | || | | |                                              |\n\
     |   |  __/| |___ ___) || || |_| |                                              |\n\
     |   |_|   |_____|____/ |_| \\___/                                               |\n\
     |                                                                              |\n\
     |                     usenet poster                                            |\n\
     |                                                                              |\n\
     '------------------------------------------------------------------------------'"
}

/// Collect all files under `dir` (skipping `.nfo` with the same base name).
pub(super) fn collect_all_files(dir: &Path, nfo_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_files_recursive(dir, nfo_name, &mut out);
    out
}

pub(super) fn collect_files_recursive(dir: &Path, nfo_name: &str, out: &mut Vec<PathBuf>) {
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

pub(super) struct TreeState {
    lines: Vec<String>,
    file_count: usize,
    dir_count: usize,
}

pub(super) fn build_tree(
    dir: &Path,
    nfo_name: &str,
    file_sizes: &HashMap<PathBuf, u64>,
) -> TreeState {
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

pub(super) fn walk_tree(
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
            let display_name = super::truncate_filename(&item_name, super::MAX_FILENAME_LEN);
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
pub(super) fn build_folder_nfo(dir: &Path) -> String {
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
pub(super) fn build_listing(paths: &[PathBuf]) -> String {
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

pub(super) fn append_dir_listing(dir: &Path, buf: &mut String, depth: usize) {
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
