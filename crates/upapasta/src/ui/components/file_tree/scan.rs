use std::collections::HashMap;
use std::path::Path;

use crate::app::DiskNzbInfo;
use crate::catalog::NzbStatusEntry;

/// Whether a path is already backed up (has an NZB in the catalog).
///
/// A file is backed when it is in the catalog by full path or base name. A
/// directory is backed when it was uploaded as a release (its folder name is in
/// the catalog) *or* every file under it is individually backed — i.e. it is
/// unbacked if any child still needs uploading. The directory case walks the
/// subtree, so this runs on a blocking worker (see [`super::DirScanJob::run`]),
/// never on the UI thread.
pub(super) fn path_is_backed(
    path: &Path,
    nzb_status: &HashMap<String, NzbStatusEntry>,
    nzb_disk_index: &HashMap<String, DiskNzbInfo>,
) -> bool {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let full = path.to_string_lossy();
    if nzb_status.contains_key(full.as_ref()) || nzb_status.contains_key(name) {
        return true;
    }
    if path.is_dir() {
        // A release NZB named after the folder (e.g. a downloaded season pack)
        // backs the whole directory; otherwise fall back to checking that every
        // inner file is individually backed.
        if nzb_disk_index.contains_key(&release_key(name)) {
            return true;
        }
        return !dir_has_unbacked(path, nzb_status, nzb_disk_index);
    }
    // A matching .nzb already on disk counts as backed.
    nzb_disk_index.contains_key(&release_key(name))
}

/// Whether `dir` contains at least one file (recursively) that is not in the
/// catalog by its base name. Walks with a cap and stops at the first hit, so a
/// huge tree cannot stall the UI. Symlinks are skipped (as `pesto::walk` does).
fn dir_has_unbacked(
    dir: &Path,
    nzb_status: &HashMap<String, NzbStatusEntry>,
    nzb_disk_index: &HashMap<String, DiskNzbInfo>,
) -> bool {
    const CAP: usize = 50_000;
    let mut stack = vec![dir.to_path_buf()];
    let mut visited = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            } else if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                visited += 1;
                let name = entry.file_name();
                let backed = name
                    .to_str()
                    .map(|n| {
                        nzb_status.contains_key(n) || nzb_disk_index.contains_key(&release_key(n))
                    })
                    .unwrap_or(false);
                if !backed {
                    return true;
                }
                if visited >= CAP {
                    return false;
                }
            }
        }
    }
    // No files at all (empty dir) counts as nothing to upload.
    false
}

/// Known media/archive extensions stripped when deriving a release key. Only
/// these are removed (not arbitrary trailing segments), so codec/group tags
/// like `x264` or `-cza` survive and stay part of the match.
const RELEASE_KEY_EXTS: &[&str] = &[
    "mkv", "mp4", "avi", "m2ts", "ts", "mov", "wmv", "flv", "iso", "img", "rar", "zip", "7z",
    "mka", "webm", "m4v", "mpg", "mpeg", "vob",
];

/// Derive a comparison key that matches a media file against an existing `.nzb`.
///
/// The same transformation is applied to both sides so different naming
/// conventions converge:
///   * `Zootopia.2016...DUAL-cza.mkv`            (media file)
///   * `Zootopia.2016...DUAL-cza.nzb`            (NZB named after the release)
///   * `20260427T151003Z_Zootopia...BiOMA.mkv.nzb` (upapasta-generated NZB)
///
/// Steps: strip a trailing `.nzb`, then any trailing known media/archive
/// extensions, then a leading `YYYYMMDDThhmmssZ_` timestamp prefix, and finally
/// keep only lowercase alphanumerics so separators do not affect the match.
pub(crate) fn release_key(name: &str) -> String {
    let mut base = name.to_string();

    // 1. Drop a trailing `.nzb` (case-insensitive).
    if base.len() >= 4 && base[base.len() - 4..].eq_ignore_ascii_case(".nzb") {
        base.truncate(base.len() - 4);
    }

    // 2. Drop trailing known media/archive extensions (e.g. `.mkv.nzb` →
    //    `.mkv` → ``). Loops so doubled extensions are all removed.
    loop {
        let stripped = base
            .rfind('.')
            .map(|dot| {
                let ext = base[dot + 1..].to_ascii_lowercase();
                if RELEASE_KEY_EXTS.contains(&ext.as_str()) {
                    base.truncate(dot);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if !stripped {
            break;
        }
    }

    // 3. Drop a leading timestamp prefix: 8 digits, 'T', 6 digits, 'Z', '_'.
    let b = base.as_bytes();
    if b.len() > 17
        && b[..8].iter().all(u8::is_ascii_digit)
        && b[8].eq_ignore_ascii_case(&b'T')
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15].eq_ignore_ascii_case(&b'Z')
        && b[16] == b'_'
    {
        base.drain(..17);
    }

    // 4. Normalize: lowercase alphanumerics only.
    base.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// Best-effort byte size of an item: file length, or the recursive sum of a
/// directory's files (capped for very large trees).
pub(super) fn item_size(path: &Path) -> u64 {
    if path.is_file() {
        return std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    const CAP: usize = 50_000;
    let mut stack = vec![path.to_path_buf()];
    let mut total = 0u64;
    let mut visited = 0usize;
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in rd.flatten() {
            let Ok(ft) = entry.file_type() else { continue };
            if ft.is_symlink() {
                continue;
            } else if ft.is_dir() {
                stack.push(entry.path());
            } else if ft.is_file() {
                total += entry.metadata().map(|m| m.len()).unwrap_or(0);
                visited += 1;
                if visited >= CAP {
                    return total;
                }
            }
        }
    }
    total
}
