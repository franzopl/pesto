//! `mediainfo`/`bdinfo` process invocation and output extraction.

use std::path::Path;

/// Run `mediainfo` with a minimal General template to obtain the duration in ms.
/// Returns `None` if mediainfo is not available or the output cannot be parsed.
pub(super) fn mediainfo_duration_ms(path: &Path) -> Option<u64> {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let output = std::process::Command::new("mediainfo")
        .arg("--Output=General;%Duration%")
        .arg(&abs)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

pub(super) fn run_mediainfo(path: &Path) -> std::io::Result<String> {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let output = std::process::Command::new("mediainfo")
        .arg(&abs)
        .output()
        .map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("could not launch mediainfo (is it installed and in PATH?): {e}"),
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = if stderr.trim().is_empty() {
            format!("mediainfo exited with status {}", output.status)
        } else {
            format!(
                "mediainfo exited with status {}: {}",
                output.status,
                stderr.trim()
            )
        };
        return Err(std::io::Error::other(msg));
    }
    if output.stdout.is_empty() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = if stderr.trim().is_empty() {
            "mediainfo exited successfully but produced no output \
             (verify that `mediainfo` on PATH is the real CLI binary and that \
             the input file exists)"
                .to_owned()
        } else {
            format!("mediainfo produced no output; stderr: {}", stderr.trim())
        };
        return Err(std::io::Error::other(msg));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    // Replace any occurrence of the full filesystem path with just the
    // basename, hiding the local directory. On Windows, canonicalize() adds a
    // \\?\ prefix that mediainfo echoes back, so we replace both forms.
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let canonical_str = abs.to_string_lossy();
    let original_str = path.to_string_lossy();
    let replaced = raw.replace(canonical_str.as_ref(), &filename);
    let replaced = replaced.replace(original_str.as_ref(), &filename);
    Ok(replaced)
}

/// Scan the Blu-ray disc at `disc_root` using `bdinfo-rs-core` and return
/// the QUICK SUMMARY block from the generated report.
pub(super) fn run_bdinfo(disc_root: &Path) -> Option<String> {
    use bdinfo_rs_core::bdrom::disc::BdRom;
    use bdinfo_rs_core::bdrom::order::PlaylistFilter;
    use bdinfo_rs_core::report::text;
    use bdinfo_rs_core::vfs::fs::FsDir;

    let vfs = FsDir::new(disc_root);
    let scan = BdRom::open_resilient(&vfs, true).ok()?;
    let order = scan.bdrom.presentation_order(&PlaylistFilter::default());
    let report = text::render_with(&scan.bdrom, &order, &scan.errors);
    extract_quick_summary_from_str(&report)
}

/// Extract the QUICK SUMMARY block from a BDInfo report string.
pub(super) fn extract_quick_summary_from_str(raw: &str) -> Option<String> {
    let after = raw
        .lines()
        .skip_while(|l| l.trim() != "QUICK SUMMARY:")
        .skip(1)
        .skip_while(|l| l.trim().is_empty())
        .take_while(|l| !l.starts_with('<') && l.trim() != "[/code]")
        .collect::<Vec<_>>()
        .join("\n");

    let trimmed = after.trim_end().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}
