//! Upload history log, compatible with the upapasta catalog.
//!
//! Records are appended to `~/.config/upapasta/history.jsonl` in the same
//! JSON format used by upapasta's `catalog.py`, so the two tools share a
//! single upload history visible from the upapasta TUI.
//!
//! After a successful upload the NZB file is also hard-linked (or copied) into
//! `~/.config/upapasta/nzb/<stamp>_<name>.nzb`, matching upapasta behaviour.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// All data needed to write one history record.
pub struct UploadRecord<'a> {
    /// Original name of the uploaded content (file or folder).
    pub name: &'a str,
    /// Obfuscated name, if `--obfuscate` was active.
    pub obfuscated_name: Option<&'a str>,
    /// Archive password, if `--password` was set.
    pub password: Option<&'a str>,
    /// Total bytes uploaded (including PAR2 and metadata files).
    pub total_bytes: u64,
    /// Newsgroups used (first group only, matching upapasta convention).
    pub group: Option<&'a str>,
    /// Primary server hostname.
    pub server: Option<&'a str>,
    /// PAR2 redundancy percentage (e.g. `"10%"`).
    pub par2_redundancy: Option<&'a str>,
    /// Wall-clock seconds the upload took.
    pub duration_secs: f64,
    /// Path to the written `.nzb` file.
    pub nzb_path: Option<&'a str>,
    /// Subject / NZB name used for the upload.
    pub subject: Option<&'a str>,
}

/// Returns `~/.config/upapasta`, creating it (and `nzb/`) if necessary.
fn catalog_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    let dir = PathBuf::from(home).join(".config").join("upapasta");
    fs::create_dir_all(dir.join("nzb")).ok()?;
    Some(dir)
}

/// Detect upload category from the name, mirroring upapasta's `detect_category`.
fn detect_category(name: &str) -> &'static str {
    let stem = Path::new(name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(name);

    // Anime: [SubGroup] ... - 01  or  EP01
    let anime = stem.contains("EP0")
        || stem.contains("EP1")
        || stem.contains("EP2")
        || (stem.contains('[') && (stem.contains(" - ") || stem.contains("- ")));

    if anime {
        return "Anime";
    }

    // TV: S01E01, 1x01, Season N, Complete Series, MINISERIES
    let tv = contains_pattern(stem, &["x0", "x1", "x2"])
        || contains_ci(
            stem,
            &["s01e", "s02e", "s03e", "season", "miniseries", "complete"],
        )
        || (stem.len() > 3 && {
            let b = stem.as_bytes();
            b.iter().any(|&c| c == b'S' || c == b's')
                && b.windows(4).any(|w| {
                    (w[0] == b'S' || w[0] == b's')
                        && w[1].is_ascii_digit()
                        && w[2].is_ascii_digit()
                        && (w[3] == b'E' || w[3] == b'e')
                })
        });

    if tv {
        return "TV";
    }

    // Movie: four-digit year 1900-2099 not followed by -MM-
    if has_year(stem) {
        return "Movie";
    }

    "Generic"
}

fn contains_pattern(s: &str, pats: &[&str]) -> bool {
    pats.iter().any(|p| s.contains(p))
}

fn contains_ci(s: &str, pats: &[&str]) -> bool {
    let lower = s.to_lowercase();
    pats.iter().any(|p| lower.contains(p))
}

fn has_year(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() < 4 {
        return false;
    }
    for i in 0..b.len() - 3 {
        let a = b[i];
        // year starts with '1' (1900-1999) or '2' (2000-2099)
        if a != b'1' && a != b'2' {
            continue;
        }
        if b[i + 1].is_ascii_digit() && b[i + 2].is_ascii_digit() && b[i + 3].is_ascii_digit() {
            // must be preceded by word boundary
            let before_ok = i == 0 || !b[i - 1].is_ascii_alphanumeric();
            // must not be followed by -MM- (ISO date)
            let after_ok = i + 4 >= b.len()
                || !(b[i + 4] == b'-'
                    && i + 7 < b.len()
                    && b[i + 5].is_ascii_digit()
                    && b[i + 6].is_ascii_digit()
                    && b[i + 7] == b'-');
            if before_ok && after_ok {
                return true;
            }
        }
    }
    false
}

/// Format a Unix timestamp as ISO 8601 UTC (e.g. `2024-01-15T12:30:45+00:00`).
fn iso8601_now() -> String {
    use crate::article::format_rfc2822;
    // Reuse the RFC 2822 formatter but convert to ISO 8601 manually.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Decompose manually (same logic as article::days_to_ymd).
    let (year, doy) = days_to_ymd(secs / 86400);
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let month_days: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let mut month = 1u64;
    let mut remaining = doy;
    for (i, &md) in month_days.iter().enumerate() {
        let days = if i == 1 && is_leap { md + 1 } else { md };
        if remaining < days {
            month = i as u64 + 1;
            break;
        }
        remaining -= days;
    }
    let day = remaining + 1;
    let _ = format_rfc2822; // suppress unused-import if inlined
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}+00:00")
}

fn days_to_ymd(mut days: u64) -> (u64, u64) {
    let mut y = 1970u64;
    loop {
        let leap = (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            return (y, days);
        }
        days -= dy;
        y += 1;
    }
}

/// Current UTC time as `YYYYMMDDTHHMMSSz` (stamp for filenames).
fn stamp_now() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (year, doy) = days_to_ymd(secs / 86400);
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;
    let month_days: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400);
    let mut month = 1u64;
    let mut remaining = doy;
    for (i, &md) in month_days.iter().enumerate() {
        let days = if i == 1 && is_leap { md + 1 } else { md };
        if remaining < days {
            month = i as u64 + 1;
            break;
        }
        remaining -= days;
    }
    let day = remaining + 1;
    format!("{year:04}{month:02}{day:02}T{h:02}{m:02}{s:02}Z")
}

/// Archive the NZB file to `~/.config/upapasta/nzb/<stamp>_<name>.nzb`.
/// Returns the archived path on success.
fn archive_nzb(src: &Path, stamp: &str, name: &str) -> Option<PathBuf> {
    let dir = catalog_dir()?.join("nzb");
    let safe: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect();
    let dest = dir.join(format!("{stamp}_{safe}.nzb"));
    fs::hard_link(src, &dest)
        .or_else(|_| fs::copy(src, &dest).map(|_| ()))
        .ok()?;
    Some(dest)
}

/// Escape a string for embedding inside a JSON string literal.
fn json_str(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
}

/// Render an optional string value as a JSON token (`null` or `"value"`).
fn json_opt(v: Option<&str>) -> String {
    match v {
        None => "null".to_string(),
        Some(s) => format!("\"{}\"", json_str(s)),
    }
}

/// Append one record to `~/.config/upapasta/history.jsonl`.
///
/// Errors are silently ignored so a catalog write failure never aborts an
/// otherwise-successful upload.
pub fn record_upload(rec: &UploadRecord<'_>) {
    let Some(dir) = catalog_dir() else { return };
    let history = dir.join("history.jsonl");

    let stamp = stamp_now();
    let iso = iso8601_now();
    let category = detect_category(rec.name);

    // Archive the NZB if present.
    let archived_nzb = rec
        .nzb_path
        .and_then(|p| archive_nzb(Path::new(p), &stamp, rec.name));
    let nzb_val = json_opt(
        archived_nzb
            .as_deref()
            .and_then(|p| p.to_str())
            .or(rec.nzb_path),
    );

    let line = format!(
        r#"{{"data_upload":"{iso}","nome_original":{nome},"categoria":"{category}","nome_ofuscado":{obf},"senha_rar":{pw},"tamanho_bytes":{bytes},"tmdb_id":null,"grupo_usenet":{group},"servidor_nntp":{srv},"redundancia_par2":{par2},"duracao_upload_s":{dur:.3},"num_arquivos_rar":null,"caminho_nzb":{nzb},"subject":{subject}}}"#,
        nome = json_opt(Some(rec.name)),
        obf = json_opt(rec.obfuscated_name),
        pw = json_opt(rec.password),
        bytes = rec.total_bytes,
        group = json_opt(rec.group),
        srv = json_opt(rec.server),
        par2 = json_opt(rec.par2_redundancy),
        dur = rec.duration_secs,
        nzb = nzb_val,
        subject = json_opt(rec.subject),
    );

    let Ok(mut f) = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&history)
    else {
        return;
    };
    let _ = writeln!(f, "{line}");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_category_tv() {
        assert_eq!(detect_category("Show.S01E03.720p"), "TV");
        assert_eq!(detect_category("Show Season 2"), "TV");
    }

    #[test]
    fn detect_category_movie() {
        assert_eq!(detect_category("Interstellar.2014.BluRay"), "Movie");
        assert_eq!(detect_category("The.Matrix.1999.x264"), "Movie");
    }

    #[test]
    fn detect_category_generic() {
        assert_eq!(detect_category("some_file.rar"), "Generic");
    }

    #[test]
    fn iso8601_format() {
        let s = iso8601_now();
        // YYYY-MM-DDTHH:MM:SS+00:00
        assert_eq!(s.len(), 25);
        assert!(s.ends_with("+00:00"));
        assert_eq!(&s[4..5], "-");
    }

    #[test]
    fn json_opt_none_is_null() {
        assert_eq!(json_opt(None), "null");
    }

    #[test]
    fn json_opt_escapes_quotes() {
        assert_eq!(json_opt(Some(r#"say "hi""#)), r#""say \"hi\"""#);
    }
}
