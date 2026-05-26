use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// One upload record stored in the catalog.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct UploadRecord {
    pub id: i64,
    pub uploaded_at: DateTime<Utc>,
    pub original_name: String,
    pub category: String,
    pub obfuscated_name: Option<String>,
    pub rar_password: Option<String>,
    pub size_bytes: Option<i64>,
    pub tmdb_id: Option<String>,
    pub usenet_group: Option<String>,
    pub nntp_server: Option<String>,
    pub par2_redundancy: Option<String>,
    pub upload_duration_s: Option<f64>,
    pub rar_file_count: Option<i64>,
    pub nzb_path: Option<String>,
    pub subject: Option<String>,
}

/// Lightweight row used in list views.
#[derive(Debug, Clone)]
pub struct UploadSummary {
    #[allow(dead_code)]
    pub id: i64,
    pub uploaded_at: DateTime<Utc>,
    pub original_name: String,
    pub category: String,
    pub size_bytes: Option<i64>,
    pub upload_duration_s: Option<f64>,
    pub usenet_group: Option<String>,
    pub nzb_path: Option<String>,
}

/// Minimal NZB status info used by the file browser for per-file indicators.
#[derive(Debug, Clone)]
pub struct NzbStatusEntry {
    pub uploaded_at: DateTime<Utc>,
    /// True when the upload used any obfuscation (obfuscated_name IS NOT NULL).
    pub obfuscated: bool,
    /// True when the upload included a RAR password.
    pub has_password: bool,
    /// Path to the generated NZB file on disk (may or may not still exist).
    pub nzb_path: Option<String>,
    pub category: String,
    pub size_bytes: Option<i64>,
    pub usenet_group: Option<String>,
}

/// Aggregate stats over the whole catalog.
#[derive(Debug, Default, Clone)]
pub struct CatalogStats {
    pub total_uploads: u64,
    pub total_bytes: u64,
    /// (category, count) sorted descending
    pub by_category: Vec<(String, u64)>,
    /// (month "YYYY-MM", bytes) last 6 months
    pub bytes_by_month: Vec<(String, u64)>,
}

pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    /// Open (or create) the catalog at the given path.
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("opening catalog at {}", path.display()))?;
        let catalog = Self { conn };
        catalog.migrate()?;
        Ok(catalog)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            "PRAGMA journal_mode=WAL;
             PRAGMA foreign_keys=ON;

             CREATE TABLE IF NOT EXISTS uploads (
                 id               INTEGER PRIMARY KEY AUTOINCREMENT,
                 uploaded_at      TEXT    NOT NULL,
                 original_name    TEXT    NOT NULL,
                 category         TEXT    NOT NULL DEFAULT 'Generic',
                 obfuscated_name  TEXT,
                 rar_password     TEXT,
                 size_bytes       INTEGER,
                 tmdb_id          TEXT,
                 usenet_group     TEXT,
                 nntp_server      TEXT,
                 par2_redundancy  TEXT,
                 upload_duration_s REAL,
                 rar_file_count   INTEGER,
                 nzb_path         TEXT,
                 subject          TEXT
             );

             CREATE INDEX IF NOT EXISTS idx_uploads_uploaded_at
                 ON uploads(uploaded_at DESC);
             CREATE INDEX IF NOT EXISTS idx_uploads_original_name
                 ON uploads(original_name);
             CREATE INDEX IF NOT EXISTS idx_uploads_category
                 ON uploads(category);
            ",
        )?;
        Ok(())
    }

    /// Insert a new upload record. Returns the row id.
    pub fn record(&self, r: &NewUpload) -> Result<i64> {
        self.conn.execute(
            "INSERT INTO uploads
             (uploaded_at, original_name, category, obfuscated_name, rar_password,
              size_bytes, tmdb_id, usenet_group, nntp_server, par2_redundancy,
              upload_duration_s, rar_file_count, nzb_path, subject)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            params![
                r.uploaded_at.to_rfc3339(),
                r.original_name,
                r.category,
                r.obfuscated_name,
                r.rar_password,
                r.size_bytes,
                r.tmdb_id,
                r.usenet_group,
                r.nntp_server,
                r.par2_redundancy,
                r.upload_duration_s,
                r.rar_file_count,
                r.nzb_path,
                r.subject,
            ],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// List uploads, optionally filtering by a name substring (case-insensitive).
    /// Returns newest-first, limited to `limit` rows.
    pub fn list(&self, filter: Option<&str>, limit: usize) -> Result<Vec<UploadSummary>> {
        let sql = if filter.is_some() {
            "SELECT id, uploaded_at, original_name, category, size_bytes,
                    upload_duration_s, usenet_group, nzb_path
             FROM uploads
             WHERE lower(original_name) LIKE lower(?1)
             ORDER BY uploaded_at DESC LIMIT ?2"
        } else {
            "SELECT id, uploaded_at, original_name, category, size_bytes,
                    upload_duration_s, usenet_group, nzb_path
             FROM uploads
             ORDER BY uploaded_at DESC LIMIT ?2"
        };

        let pattern = filter.map(|f| format!("%{}%", f));
        let mut stmt = self.conn.prepare(sql)?;

        let rows = if let Some(ref p) = pattern {
            stmt.query_map(params![p, limit as i64], row_to_summary)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        } else {
            stmt.query_map(params![rusqlite::types::Null, limit as i64], row_to_summary)?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };

        Ok(rows)
    }

    /// Fetch a single full record by id.
    #[allow(dead_code)]
    pub fn get(&self, id: i64) -> Result<Option<UploadRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, uploaded_at, original_name, category, obfuscated_name, rar_password,
                    size_bytes, tmdb_id, usenet_group, nntp_server, par2_redundancy,
                    upload_duration_s, rar_file_count, nzb_path, subject
             FROM uploads WHERE id = ?1",
        )?;
        let mut rows = stmt.query_map(params![id], row_to_record)?;
        match rows.next() {
            Some(r) => Ok(Some(r?)),
            None => Ok(None),
        }
    }

    pub fn stats(&self) -> Result<CatalogStats> {
        let total_uploads: u64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM uploads", [], |r| r.get(0))?;

        let total_bytes: u64 =
            self.conn
                .query_row("SELECT COALESCE(SUM(size_bytes),0) FROM uploads", [], |r| {
                    r.get(0)
                })?;

        let mut stmt = self.conn.prepare(
            "SELECT category, COUNT(*) as cnt FROM uploads GROUP BY category ORDER BY cnt DESC",
        )?;
        let by_category: Vec<(String, u64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;

        let mut stmt = self.conn.prepare(
            "SELECT strftime('%Y-%m', uploaded_at) as month,
                    COALESCE(SUM(size_bytes),0)
             FROM uploads
             GROUP BY month
             ORDER BY month DESC
             LIMIT 6",
        )?;
        let bytes_by_month: Vec<(String, u64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        // Return oldest-first for display
        let bytes_by_month: Vec<_> = bytes_by_month.into_iter().rev().collect();

        Ok(CatalogStats {
            total_uploads,
            total_bytes,
            by_category,
            bytes_by_month,
        })
    }

    /// Build a map from `original_name` → `NzbStatusEntry` for the file browser.
    /// Only the most recent record per filename is kept (ORDER BY uploaded_at DESC).
    pub fn status_map(&self) -> Result<HashMap<String, NzbStatusEntry>> {
        let mut stmt = self.conn.prepare(
            "SELECT original_name, uploaded_at, obfuscated_name, rar_password,
                    nzb_path, category, size_bytes, usenet_group
             FROM uploads
             ORDER BY uploaded_at DESC",
        )?;
        let mut map: HashMap<String, NzbStatusEntry> = HashMap::new();
        let rows = stmt.query_map([], |r| {
            let name: String = r.get(0)?;
            let dt_str: String = r.get(1)?;
            let obfuscated_name: Option<String> = r.get(2)?;
            let rar_password: Option<String> = r.get(3)?;
            Ok((
                name,
                NzbStatusEntry {
                    uploaded_at: parse_dt(&dt_str),
                    obfuscated: obfuscated_name.is_some(),
                    has_password: rar_password.is_some(),
                    nzb_path: r.get(4)?,
                    category: r.get(5)?,
                    size_bytes: r.get(6)?,
                    usenet_group: r.get(7)?,
                },
            ))
        })?;
        for row in rows {
            let (name, entry) = row?;
            map.entry(name).or_insert(entry);
        }
        Ok(map)
    }

    /// Returns all non-null nzb_path values from the catalog.
    pub fn all_nzb_paths(&self) -> Result<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT nzb_path FROM uploads WHERE nzb_path IS NOT NULL")?;
        let paths = stmt
            .query_map([], |r| r.get::<_, String>(0))?
            .filter_map(|r| r.ok())
            .collect();
        Ok(paths)
    }

    /// Returns true if at least one record exists (to skip re-import).
    pub fn is_populated(&self) -> bool {
        self.conn
            .query_row("SELECT COUNT(*) FROM uploads", [], |r| r.get::<_, i64>(0))
            .map(|n| n > 0)
            .unwrap_or(false)
    }

    /// Import from the legacy Python JSONL file. Skips malformed lines.
    /// Returns (imported, skipped) counts.
    pub fn import_jsonl(&self, path: &Path) -> Result<(usize, usize)> {
        let content =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

        let mut imported = 0usize;
        let mut skipped = 0usize;

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            match serde_json::from_str::<LegacyRecord>(line) {
                Ok(rec) => {
                    let new = rec.into_new_upload();
                    if self.record(&new).is_ok() {
                        imported += 1;
                    } else {
                        skipped += 1;
                    }
                }
                Err(_) => skipped += 1,
            }
        }

        Ok((imported, skipped))
    }
}

// ── helpers ────────────────────────────────────────────────────────────────

fn parse_dt(s: &str) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

fn row_to_summary(r: &rusqlite::Row<'_>) -> rusqlite::Result<UploadSummary> {
    let dt_str: String = r.get(1)?;
    Ok(UploadSummary {
        id: r.get(0)?,
        uploaded_at: parse_dt(&dt_str),
        original_name: r.get(2)?,
        category: r.get(3)?,
        size_bytes: r.get(4)?,
        upload_duration_s: r.get(5)?,
        usenet_group: r.get(6)?,
        nzb_path: r.get(7)?,
    })
}

#[allow(dead_code)]
fn row_to_record(r: &rusqlite::Row<'_>) -> rusqlite::Result<UploadRecord> {
    let dt_str: String = r.get(1)?;
    Ok(UploadRecord {
        id: r.get(0)?,
        uploaded_at: parse_dt(&dt_str),
        original_name: r.get(2)?,
        category: r.get(3)?,
        obfuscated_name: r.get(4)?,
        rar_password: r.get(5)?,
        size_bytes: r.get(6)?,
        tmdb_id: r.get(7)?,
        usenet_group: r.get(8)?,
        nntp_server: r.get(9)?,
        par2_redundancy: r.get(10)?,
        upload_duration_s: r.get(11)?,
        rar_file_count: r.get(12)?,
        nzb_path: r.get(13)?,
        subject: r.get(14)?,
    })
}

// ── New upload builder ────────────────────────────────────────────────────

#[derive(Debug, Default)]
pub struct NewUpload {
    pub uploaded_at: DateTime<Utc>,
    pub original_name: String,
    pub category: String,
    pub obfuscated_name: Option<String>,
    pub rar_password: Option<String>,
    pub size_bytes: Option<i64>,
    pub tmdb_id: Option<String>,
    pub usenet_group: Option<String>,
    pub nntp_server: Option<String>,
    pub par2_redundancy: Option<String>,
    pub upload_duration_s: Option<f64>,
    pub rar_file_count: Option<i64>,
    pub nzb_path: Option<String>,
    pub subject: Option<String>,
}

impl NewUpload {
    pub fn from_name(name: impl Into<String>) -> Self {
        let name = name.into();
        let category = detect_category(&name);
        Self {
            uploaded_at: Utc::now(),
            original_name: name,
            category,
            ..Default::default()
        }
    }
}

// ── Category detection (mirrors the Python version) ───────────────────────

pub fn detect_category(name: &str) -> String {
    let stem = Path::new(name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| name.to_string());

    if is_anime(&stem) {
        return "Anime".into();
    }
    if is_tv(&stem) {
        return "TV".into();
    }
    if is_movie(&stem) {
        return "Movie".into();
    }
    "Generic".into()
}

fn is_anime(s: &str) -> bool {
    // [SubGroup] ... - 01  or EP01
    let has_bracket_group = s.starts_with('[')
        || (s.contains('[') && s.find('[').unwrap_or(usize::MAX) < s.find(']').unwrap_or(0));
    let has_ep_marker = {
        let l = s.to_lowercase();
        l.contains(" - ")
            && l[l.find(" - ").unwrap_or(0)..]
                .chars()
                .any(|c| c.is_ascii_digit())
            || l.contains("ep")
                && l[l.find("ep").unwrap_or(0) + 2..]
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
    };
    has_bracket_group && has_ep_marker
}

fn is_tv(s: &str) -> bool {
    let l = s.to_lowercase();
    // S01E01, s1e2, 1x01
    let has_sxey = (|| {
        let mut chars = l.chars().peekable();
        while let Some(c) = chars.next() {
            if c == 's' && chars.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                // consume digits
                while chars.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                    chars.next();
                }
                if chars.next() == Some('e')
                    && chars.peek().map(|c| c.is_ascii_digit()).unwrap_or(false)
                {
                    return true;
                }
            }
        }
        false
    })();
    let has_nxnn = l.contains('x') && {
        let idx = l.find('x').unwrap();
        idx > 0
            && l[..idx]
                .chars()
                .last()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
            && l[idx + 1..]
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
    };
    let has_season = l.contains("season");
    let has_series = l.contains("complete") && l.contains("series");
    let has_miniseries = l.contains("miniseries");
    has_sxey || has_nxnn || has_season || has_series || has_miniseries
}

fn is_movie(s: &str) -> bool {
    // Look for a standalone year 1900-2099 not part of a date like 2024-01-01
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i + 3 < len {
        if (bytes[i] == b'1' || bytes[i] == b'2')
            && bytes[i + 1].is_ascii_digit()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            let year = std::str::from_utf8(&bytes[i..i + 4])
                .ok()
                .and_then(|y| y.parse::<u16>().ok())
                .unwrap_or(0);
            if (1900..=2099).contains(&year) {
                // Not part of ISO date pattern YYYY-MM-DD
                let after = bytes.get(i + 4);
                if after != Some(&b'-') {
                    return true;
                }
            }
        }
        i += 1;
    }
    false
}

// ── Legacy JSONL deserialization ──────────────────────────────────────────

#[derive(Deserialize)]
struct LegacyRecord {
    #[serde(default)]
    data_upload: Option<String>,
    nome_original: String,
    #[serde(default)]
    categoria: Option<String>,
    #[serde(default)]
    nome_ofuscado: Option<String>,
    #[serde(default)]
    senha_rar: Option<String>,
    #[serde(default)]
    tamanho_bytes: Option<i64>,
    #[serde(default)]
    tmdb_id: Option<String>,
    #[serde(default)]
    grupo_usenet: Option<String>,
    #[serde(default)]
    servidor_nntp: Option<String>,
    #[serde(default)]
    redundancia_par2: Option<String>,
    #[serde(default)]
    duracao_upload_s: Option<f64>,
    #[serde(default)]
    num_arquivos_rar: Option<i64>,
    #[serde(default)]
    caminho_nzb: Option<String>,
    #[serde(default)]
    subject: Option<String>,
}

impl LegacyRecord {
    fn into_new_upload(self) -> NewUpload {
        let uploaded_at = self
            .data_upload
            .as_deref()
            .and_then(|s| DateTime::parse_from_rfc3339(s).ok())
            .map(|d| d.with_timezone(&Utc))
            .unwrap_or_else(Utc::now);

        let category = self
            .categoria
            .unwrap_or_else(|| detect_category(&self.nome_original));

        NewUpload {
            uploaded_at,
            original_name: self.nome_original,
            category,
            obfuscated_name: self.nome_ofuscado,
            rar_password: self.senha_rar,
            size_bytes: self.tamanho_bytes,
            tmdb_id: self.tmdb_id,
            usenet_group: self.grupo_usenet,
            nntp_server: self.servidor_nntp,
            par2_redundancy: self.redundancia_par2,
            upload_duration_s: self.duracao_upload_s,
            rar_file_count: self.num_arquivos_rar,
            nzb_path: self.caminho_nzb,
            subject: self.subject,
        }
    }
}

/// Default catalog path: ~/.local/share/upapasta/catalog.db
pub fn default_catalog_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "upapasta")
        .map(|d| d.data_local_dir().join("catalog.db"))
}

/// Default legacy JSONL path: ~/.config/upapasta/history.jsonl
pub fn legacy_jsonl_path() -> Option<PathBuf> {
    directories::ProjectDirs::from("", "", "upapasta").map(|d| d.config_dir().join("history.jsonl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::NamedTempFile;

    fn temp_catalog() -> Catalog {
        let f = NamedTempFile::new().unwrap();
        Catalog::open(f.path()).unwrap()
    }

    #[test]
    fn record_and_list() {
        let c = temp_catalog();
        let r = NewUpload::from_name("My.Movie.2023.1080p.mkv");
        c.record(&r).unwrap();
        let rows = c.list(None, 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].original_name, "My.Movie.2023.1080p.mkv");
        assert_eq!(rows[0].category, "Movie");
    }

    #[test]
    fn filter_by_name() {
        let c = temp_catalog();
        c.record(&NewUpload::from_name("Breaking.Bad.S01E01.mkv"))
            .unwrap();
        c.record(&NewUpload::from_name("some_random_file.bin"))
            .unwrap();
        let rows = c.list(Some("breaking"), 10).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].category, "TV");
    }

    #[test]
    fn stats_counts() {
        let c = temp_catalog();
        for name in &["A.Movie.2020.mkv", "B.Movie.2021.mkv", "Show.S01E01.mkv"] {
            c.record(&NewUpload::from_name(*name)).unwrap();
        }
        let s = c.stats().unwrap();
        assert_eq!(s.total_uploads, 3);
    }

    #[test]
    fn detect_category_cases() {
        assert_eq!(detect_category("The.Movie.2023.1080p.mkv"), "Movie");
        assert_eq!(detect_category("Show.S02E05.HDTV.mkv"), "TV");
        assert_eq!(detect_category("random_file.bin"), "Generic");
    }
}
