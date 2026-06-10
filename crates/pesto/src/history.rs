//! Upload history log.
//!
//! Records are appended to `<history_dir>/history.jsonl` (default:
//! `~/.config/pesto/history.jsonl`). The JSON format is compatible with
//! upapasta's `catalog.py`; set `output.history_dir = "~/.config/upapasta"`
//! in `config.toml` to share the catalog with upapasta.
//!
//! After a successful upload the NZB file is also hard-linked (or copied) into
//! `<history_dir>/nzb/<stamp>_<name>.nzb`.

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

/// Returns the history catalog directory, creating it (and `nzb/`) if necessary.
///
/// Uses `override_dir` when provided; otherwise falls back to
/// `$XDG_CONFIG_HOME/pesto` or `~/.config/pesto`.
fn catalog_dir(override_dir: Option<&Path>) -> Option<PathBuf> {
    let dir = if let Some(d) = override_dir {
        d.to_path_buf()
    } else if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME").filter(|v| !v.is_empty()) {
        PathBuf::from(xdg).join("pesto")
    } else {
        let home = std::env::var_os("HOME")?;
        PathBuf::from(home).join(".config").join("pesto")
    };
    fs::create_dir_all(dir.join("nzb")).ok()?;
    Some(dir)
}

/// Sanitise a content name for use in a catalog file name: keep alphanumerics,
/// `-`, `.` and spaces; replace everything else with `_`; cap at 80 chars.
fn sanitize_name(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '.' || c == ' ' {
                c
            } else {
                '_'
            }
        })
        .take(80)
        .collect()
}

/// Resolve the per-upload verbose log path under `<history_dir>/logs/`.
///
/// The directory is created if needed, and existing **pesto** logs are pruned
/// so that at most `keep` remain once the new one (named here) is written.
/// File names are `<stamp>_<name>.log`; the leading timestamp sorts lexically
/// in chronological order, so the oldest files are removed first. Logs not
/// matching pesto's naming (e.g. legacy upapasta logs that share the same
/// directory) are never touched.
///
/// Returns `None` when no history directory can be resolved (e.g. `$HOME` and
/// `$XDG_CONFIG_HOME` are both unset). The caller treats that as "no log".
pub fn session_log_path(history_dir: Option<&Path>, name: &str, keep: usize) -> Option<PathBuf> {
    let dir = catalog_dir(history_dir)?.join("logs");
    fs::create_dir_all(&dir).ok()?;
    // Keep room for the file we are about to create: prune to `keep - 1`.
    prune_logs(&dir, keep.saturating_sub(1));
    let stamp = stamp_now();
    let safe = sanitize_name(name);
    Some(dir.join(format!("{stamp}_{safe}.log")))
}

/// True when `name` is one of pesto's own session logs, i.e. it starts with a
/// `stamp_now()` prefix (`YYYYMMDDTHHMMSSZ_`) and ends in `.log`. This excludes
/// legacy upapasta logs (`YYYY-MM-DD_HH-MM-SS_…`) so pruning never removes them.
fn is_pesto_log(name: &str) -> bool {
    let b = name.as_bytes();
    b.len() > 16
        && name.ends_with(".log")
        && b[0..8].iter().all(u8::is_ascii_digit)
        && b[8] == b'T'
        && b[9..15].iter().all(u8::is_ascii_digit)
        && b[15] == b'Z'
        && b[16] == b'_'
}

/// Remove the oldest pesto logs in `dir` until at most `keep` remain. Only
/// files matching [`is_pesto_log`] are considered for counting and removal.
fn prune_logs(dir: &Path, keep: usize) {
    let mut logs: Vec<PathBuf> = match fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|s| s.to_str())
                    .is_some_and(is_pesto_log)
            })
            .collect(),
        Err(_) => return,
    };
    if logs.len() <= keep {
        return;
    }
    logs.sort();
    let remove = logs.len() - keep;
    for p in logs.into_iter().take(remove) {
        let _ = fs::remove_file(p);
    }
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

/// Archive the NZB file to `<catalog_dir>/nzb/<stamp>_<name>.nzb`.
/// Returns the archived path on success.
fn archive_nzb(
    src: &Path,
    stamp: &str,
    name: &str,
    override_dir: Option<&Path>,
) -> Option<PathBuf> {
    let dir = catalog_dir(override_dir)?.join("nzb");
    let safe = sanitize_name(name);
    let dest = dir.join(format!("{stamp}_{safe}.nzb"));

    // Guard against archiving a file onto itself. `src` is already a hard link
    // of the canonical archive copy (`~/.config/pesto/nzb/<ts>_<stem>.nzb`),
    // and truncating `name` to 80 chars can strip the extension so that
    // `<stamp>_<safe>.nzb` resolves to that very same canonical path. When that
    // happens `hard_link` fails with EEXIST and the `fs::copy` fallback would
    // open `dest` with `O_TRUNC` *before* reading `src` — and since both names
    // point at one inode, that zeroes the NZB. The file is already archived in
    // this case, so just report the existing path.
    if same_file(src, &dest) {
        return Some(dest);
    }

    fs::hard_link(src, &dest)
        .or_else(|_| fs::copy(src, &dest).map(|_| ()))
        .ok()?;
    Some(dest)
}

/// Whether two paths refer to the same on-disk file (same device + inode).
/// Returns `false` if either path cannot be stat'd.
fn same_file(a: &Path, b: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        match (fs::metadata(a), fs::metadata(b)) {
            (Ok(ma), Ok(mb)) => ma.dev() == mb.dev() && ma.ino() == mb.ino(),
            _ => false,
        }
    }
    #[cfg(not(unix))]
    {
        match (fs::canonicalize(a), fs::canonicalize(b)) {
            (Ok(ca), Ok(cb)) => ca == cb,
            _ => false,
        }
    }
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

/// Append one record to `<history_dir>/history.jsonl`.
///
/// `history_dir` overrides the default (`~/.config/pesto`). Pass `None` to
/// use the default. Errors are silently ignored so a catalog write failure
/// never aborts an otherwise-successful upload.
pub fn record_upload(rec: &UploadRecord<'_>, history_dir: Option<&Path>) {
    let Some(dir) = catalog_dir(history_dir) else {
        return;
    };
    let history = dir.join("history.jsonl");

    let stamp = stamp_now();
    let iso = iso8601_now();
    let category = detect_category(rec.name);

    // Archive the NZB if present.
    let archived_nzb = rec
        .nzb_path
        .and_then(|p| archive_nzb(Path::new(p), &stamp, rec.name, history_dir));
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

    // Regression: archiving an NZB whose computed destination resolves to the
    // same inode as the source must not zero the file. This reproduces the
    // `--season` data-loss bug where a long release name truncated to 80 chars
    // dropped its extension, making `<stamp>_<safe>.nzb` collide with the
    // canonical archive copy that `src` is already hard-linked to.
    #[test]
    fn archive_nzb_does_not_zero_a_self_referential_copy() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let nzb_dir = root.join("nzb");
        fs::create_dir_all(&nzb_dir).unwrap();

        let stamp = "20260101T000000Z";
        let name = "Movie";

        // The canonical archive copy that already lives in <root>/nzb, named
        // exactly as archive_nzb would name its destination.
        let dest = nzb_dir.join(format!("{stamp}_{name}.nzb"));
        fs::write(&dest, b"<nzb>real content</nzb>").unwrap();

        // `src` is a separate path hard-linked to the canonical copy — i.e. the
        // user-destination NZB. Same inode as `dest`.
        let src = root.join("user-dest.nzb");
        fs::hard_link(&dest, &src).unwrap();

        let archived = archive_nzb(&src, stamp, name, Some(root));

        assert_eq!(archived.as_deref(), Some(dest.as_path()));
        // Neither the source nor the destination may have been truncated.
        assert_eq!(fs::read(&src).unwrap(), b"<nzb>real content</nzb>");
        assert_eq!(fs::read(&dest).unwrap(), b"<nzb>real content</nzb>");
    }

    // A normal archive (no collision) still hard-links the NZB into <dir>/nzb.
    #[test]
    fn archive_nzb_links_a_distinct_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let src = root.join("upload.nzb");
        fs::write(&src, b"payload").unwrap();

        let dest = archive_nzb(&src, "20260101T000000Z", "Movie", Some(root)).unwrap();
        assert!(dest.exists());
        assert_eq!(fs::read(&dest).unwrap(), b"payload");
        // Original is untouched.
        assert_eq!(fs::read(&src).unwrap(), b"payload");
    }

    #[test]
    fn session_log_path_creates_logs_dir_and_names_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let p = session_log_path(Some(root), "Movie 2026.mkv", 50).unwrap();
        assert_eq!(p.parent().unwrap(), root.join("logs"));
        assert!(root.join("logs").is_dir());
        let fname = p.file_name().unwrap().to_string_lossy();
        assert!(fname.ends_with("_Movie 2026.mkv.log"), "got {fname}");
    }

    #[test]
    fn session_log_path_prunes_to_keep() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let logs = root.join("logs");
        fs::create_dir_all(&logs).unwrap();
        // Seed 5 existing pesto logs with sortable (chronological) stamps.
        for i in 0..5 {
            fs::write(logs.join(format!("2026010{i}T000000Z_old.log")), b"x").unwrap();
        }
        // A non-log file and a legacy upapasta log must never be pruned.
        fs::write(logs.join("keep.txt"), b"x").unwrap();
        let legacy = "2026-05-17_15-32-37_legacy.log";
        fs::write(logs.join(legacy), b"x").unwrap();

        // keep=3 → prune pesto logs to 2 existing, then the caller writes the 3rd.
        let new = session_log_path(Some(root), "new", 3).unwrap();

        let mut pesto_logs: Vec<String> = fs::read_dir(&logs)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|n| is_pesto_log(n))
            .collect();
        pesto_logs.sort();
        // Two newest pesto logs survive; the three oldest were removed.
        assert_eq!(
            pesto_logs,
            vec![
                "20260103T000000Z_old.log".to_string(),
                "20260104T000000Z_old.log".to_string(),
            ]
        );
        // Unrelated files are left alone, including the legacy upapasta log.
        assert!(logs.join("keep.txt").exists());
        assert!(logs.join(legacy).exists());
        // The returned path is where the caller will write the new log.
        assert!(new
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with("_new.log"));
    }

    #[test]
    fn is_pesto_log_matches_only_pesto_stamps() {
        assert!(is_pesto_log("20260610T141921Z_Movie.mkv.log"));
        // Legacy upapasta format (dashes, no T/Z stamp).
        assert!(!is_pesto_log("2026-05-17_15-32-37_legacy.log"));
        // Right shape but wrong extension.
        assert!(!is_pesto_log("20260610T141921Z_Movie.mkv.txt"));
        // Too short / not a stamp.
        assert!(!is_pesto_log("notes.log"));
    }
}
