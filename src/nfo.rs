//! `.nfo` article content generation.
//!
//! An `.nfo` is a plain-text informational file posted as the first article
//! in a set. It lists the upload name, date, file sizes, and SHA-256 hashes
//! so a downloader can verify the original files without fetching every
//! article.

use std::fmt::Write as FmtWrite;
use std::io;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// A file entry included in the `.nfo`.
#[derive(Debug, Clone)]
pub struct NfoEntry {
    /// Published name (relative path with `/` separators).
    pub name: String,
    /// File size in bytes.
    pub size: u64,
    /// Hex-encoded SHA-256 digest of the original file content.
    pub sha256: String,
}

/// Compute the SHA-256 digest of a file, reading it sequentially.
pub fn sha256_file(path: &Path) -> io::Result<String> {
    use std::io::Read;

    // SHA-256 via the `sha2` crate (added to Cargo.toml).
    use sha2::Digest;
    let mut hasher = sha2::Sha256::new();
    let mut file = std::fs::File::open(path)?;
    let mut buf = vec![0u8; 1 << 16]; // 64 KiB
    loop {
        let n = file.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex_encode(&hasher.finalize()))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Build the text content of the `.nfo` article.
pub fn build(upload_name: &str, entries: &[NfoEntry]) -> String {
    let total: u64 = entries.iter().map(|e| e.size).sum();

    // Rough human-readable size.
    let total_str = format_size(total);

    // ISO-8601 date (UTC).
    let date = {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Simple YYYY-MM-DD approximation (no chrono dependency).
        let days = secs / 86400;
        // Days since 1970-01-01 → date (Gregorian proleptic, good until 2100).
        let (y, m, d) = days_to_ymd(days);
        format!("{y:04}-{m:02}-{d:02}")
    };

    let width = 72usize;
    let rule = "═".repeat(width);
    let thin = "─".repeat(width);

    let mut out = String::new();
    let _ = writeln!(out, " {rule}");
    let _ = writeln!(out, "  {upload_name}");
    let _ = writeln!(out, " {rule}");
    let _ = writeln!(out);
    let _ = writeln!(out, "  Date     : {date}");
    let _ = writeln!(out, "  Files    : {}", entries.len());
    let _ = writeln!(out, "  Size     : {total_str}");
    let _ = writeln!(out);
    let _ = writeln!(out, " {thin}");

    // Column widths: name left-aligned, size right-aligned in 9 chars.
    let name_w = entries.iter().map(|e| e.name.len()).max().unwrap_or(20).max(20);
    for e in entries {
        let size_str = format_size(e.size);
        let _ = writeln!(
            out,
            "  {:<name_w$}  {:>9}  {}",
            e.name, size_str, e.sha256,
            name_w = name_w
        );
    }

    let _ = writeln!(out, " {thin}");
    let _ = writeln!(out);
    let _ = writeln!(out, "  Created by pesto");
    out
}

fn format_size(bytes: u64) -> String {
    const GIB: u64 = 1 << 30;
    const MIB: u64 = 1 << 20;
    const KIB: u64 = 1 << 10;
    if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// Convert days since 1970-01-01 to (year, month, day). Valid until 2100.
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_contains_expected_fields() {
        let entries = vec![
            NfoEntry { name: "movie.mkv".into(), size: 1 << 30, sha256: "abc".into() },
            NfoEntry { name: "sub.srt".into(), size: 1024, sha256: "def".into() },
        ];
        let text = build("My Upload", &entries);
        assert!(text.contains("My Upload"));
        assert!(text.contains("Files    : 2"));
        assert!(text.contains("movie.mkv"));
        assert!(text.contains("abc"));
        assert!(text.contains("sub.srt"));
    }
}
