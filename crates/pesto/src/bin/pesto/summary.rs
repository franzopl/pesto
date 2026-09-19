//! Final structured reporting for an upload session.

use std::path::Path;

/// Append a one-line structured summary to the session log file.
///
/// Written after the upload completes so it is always the last line, making
/// `tail -1` a reliable way to check the outcome of any upload.
pub(super) fn write_session_summary(
    path: &Path,
    label: &str,
    cancelled: bool,
    had_failures: bool,
    total_bytes: u64,
    nzb_path: Option<&Path>,
) {
    use std::io::Write;

    let status = if cancelled {
        "cancelled"
    } else if had_failures {
        "failed"
    } else {
        "ok"
    };

    let total_mb = total_bytes as f64 / 1_048_576.0;
    let nzb = nzb_path
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("-");

    let now = chrono::Local::now().format("%Y-%m-%dT%H:%M:%SZ");
    let line = format!(
        "{now}  summary  status={status}  label=\"{label}\"  bytes={total_mb:.1}MiB  nzb={nzb}\n"
    );

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}
