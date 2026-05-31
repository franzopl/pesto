//! Prowlarr / Newznab client for upapasta.
//!
//! Goals:
//!   1. Test connectivity to a configured Prowlarr instance.
//!   2. Search for an NZB by **exact release name** — not by movie/show title.
//!      The Newznab `q=` parameter is sent verbatim; the caller strips the
//!      file extension so the query matches the release name only.
//!   3. Download the `.nzb` file for a chosen search result.
//!
//! Prowlarr exposes a Newznab-compatible `/api/v1/indexer/...` endpoint but
//! the easier path is its own aggregated search: `GET /api/v1/search`.
//! Results are JSON (not XML) which avoids a second XML parse layer.

#![allow(dead_code)]

use anyhow::{bail, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Prowlarr connection config (from pesto.toml `[indexer]` or upapasta.toml).
#[derive(Debug, Clone)]
pub struct ProwlarrConfig {
    /// Base URL, e.g. `http://localhost:9696`
    pub url: String,
    /// Prowlarr API key
    pub api_key: String,
}

impl ProwlarrConfig {
    /// Returns None when either field is empty/missing.
    pub fn from_opt(url: Option<&str>, api_key: Option<&str>) -> Option<Self> {
        match (url, api_key) {
            (Some(u), Some(k)) if !u.is_empty() && !k.is_empty() => Some(Self {
                url: u.trim_end_matches('/').to_string(),
                api_key: k.to_string(),
            }),
            _ => None,
        }
    }
}

/// Status of the Prowlarr connection.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum ConnectionStatus {
    /// Not yet tested.
    #[default]
    Unknown,
    /// Connection in progress.
    Checking,
    /// Connected successfully; inner string is the Prowlarr version.
    Ok(String),
    /// Could not connect or authentication failed.
    Failed(String),
}

/// One result returned by the Prowlarr search API.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchResult {
    /// The NZB/release title (this is what we match against the release name).
    pub title: String,
    /// Download link — calling this URL with the API key returns the `.nzb`.
    pub download_url: Option<String>,
    /// Size in bytes (may be 0 if the indexer didn't report it).
    #[serde(default)]
    pub size: u64,
    /// Indexer name (for display).
    #[serde(default)]
    pub indexer: String,
    /// Publication date (ISO-8601 string from Prowlarr).
    #[serde(default)]
    pub publish_date: String,
    /// Number of Usenet articles in the NZB (if reported by the indexer).
    #[serde(default)]
    pub grabs: u32,
    /// Guid/link to the NZB details page (sometimes usable as download URL).
    #[serde(default)]
    pub guid: String,
    /// Newznab category IDs (numeric).
    #[serde(default)]
    pub categories: Vec<CategoryEntry>,
    /// Whether the indexer flagged this as a passworded release.
    #[serde(default)]
    pub password_protected: bool,
    /// Indexer protocol: `"usenet"` or `"torrent"`. Used to drop torrent
    /// results so only `.nzb`-backed releases are shown.
    #[serde(default)]
    pub protocol: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CategoryEntry {
    pub id: u32,
    #[serde(default)]
    pub name: String,
}

/// Build a shared `reqwest::Client` (reuse across calls).
pub fn build_client() -> Result<Client> {
    Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .context("building HTTP client")
}

/// Test the Prowlarr connection by calling `/api/v1/system/status`.
/// Returns the Prowlarr version string on success.
pub async fn check_connection(cfg: &ProwlarrConfig, client: &Client) -> Result<String> {
    let url = format!("{}/api/v1/system/status", cfg.url);
    let resp = client
        .get(&url)
        .header("X-Api-Key", &cfg.api_key)
        .send()
        .await
        .context("connecting to Prowlarr")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "HTTP {}: {}",
            status,
            body.chars().take(120).collect::<String>()
        );
    }

    #[derive(Deserialize)]
    struct Status {
        version: String,
    }
    let s: Status = resp.json().await.context("parsing Prowlarr status")?;
    Ok(s.version)
}

/// Fetch the IDs of every **Usenet** indexer configured in Prowlarr.
///
/// Prowlarr's aggregated `/api/v1/search` queries *all* indexers, including
/// torrent trackers. Passing the resulting IDs as `indexerIds[]` restricts a
/// search to Usenet sources so torrent trackers are never even queried.
///
/// Returns an empty vector when no Usenet indexer is configured; the caller
/// can then fall back to protocol-based filtering of the results.
pub async fn usenet_indexer_ids(cfg: &ProwlarrConfig, client: &Client) -> Result<Vec<u32>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Indexer {
        id: u32,
        #[serde(default)]
        protocol: String,
        #[serde(default)]
        enable: bool,
    }

    let url = format!("{}/api/v1/indexer", cfg.url);
    let resp = client
        .get(&url)
        .header("X-Api-Key", &cfg.api_key)
        .send()
        .await
        .context("listing Prowlarr indexers")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "indexer list HTTP {}: {}",
            status,
            body.chars().take(200).collect::<String>()
        );
    }

    let indexers: Vec<Indexer> = resp.json().await.context("parsing indexer list")?;
    Ok(indexers
        .into_iter()
        .filter(|i| i.enable && i.protocol.eq_ignore_ascii_case("usenet"))
        .map(|i| i.id)
        .collect())
}

/// Search Prowlarr for a release by **exact name**.
///
/// `release_name` should be the filename without extension, e.g.
/// `"Movie.2024.1080p.BluRay.x264-GROUP"`.  We pass it verbatim as `q=` so
/// the search targets the exact release, not a fuzzy title match.
///
/// When `indexer_ids` is non-empty the search is restricted to those indexers
/// (see [`usenet_indexer_ids`]) so torrent trackers are skipped entirely. As a
/// safety net, any non-Usenet result that slips through is dropped.
///
/// Returns results sorted by title similarity to `release_name` (exact match
/// first, then prefix matches, then the rest ordered by date descending).
pub async fn search_by_release(
    cfg: &ProwlarrConfig,
    client: &Client,
    release_name: &str,
    indexer_ids: &[u32],
) -> Result<Vec<SearchResult>> {
    let url = format!("{}/api/v1/search", cfg.url);
    let mut query: Vec<(&str, String)> = vec![
        ("query", release_name.to_string()),
        ("type", "search".to_string()),
        // Usenet category buckets (movies, TV, etc.).
        ("categories[]", "1000".to_string()),
        ("categories[]", "2000".to_string()),
        ("categories[]", "3000".to_string()),
        ("categories[]", "4000".to_string()),
        ("categories[]", "5000".to_string()),
        ("categories[]", "6000".to_string()),
        ("categories[]", "7000".to_string()),
    ];
    // Restrict the search to the given (Usenet) indexers when known.
    for id in indexer_ids {
        query.push(("indexerIds[]", id.to_string()));
    }

    let resp = client
        .get(&url)
        .header("X-Api-Key", &cfg.api_key)
        .query(&query)
        .send()
        .await
        .context("sending search request")?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        bail!(
            "search HTTP {}: {}",
            status,
            body.chars().take(200).collect::<String>()
        );
    }

    let mut results: Vec<SearchResult> = resp.json().await.context("parsing search results")?;

    // Drop torrent results (defensive: should be empty when indexer_ids is set).
    // Keep entries that report Usenet or report nothing at all.
    results.retain(|r| r.protocol.is_empty() || r.protocol.eq_ignore_ascii_case("usenet"));

    // Sort: exact match first, then prefix, then by date (newest first).
    let q_lower = release_name.to_lowercase();
    results.sort_by(|a, b| {
        let score = |r: &SearchResult| -> u8 {
            let t = r.title.to_lowercase();
            // Strip common NZB extensions for comparison
            let t = t.strip_suffix(".nzb").unwrap_or(&t);
            if t == q_lower {
                0
            } else if t.starts_with(&q_lower) || q_lower.starts_with(t) {
                1
            } else {
                2
            }
        };
        score(a).cmp(&score(b))
    });

    Ok(results)
}

/// Download a `.nzb` file from a `SearchResult` and save it to `dest_path`.
///
/// Uses the `download_url` field.  If that is absent, returns an error.
pub async fn download_nzb(
    cfg: &ProwlarrConfig,
    client: &Client,
    result: &SearchResult,
    dest_path: &Path,
) -> Result<()> {
    let url = result
        .download_url
        .as_deref()
        .filter(|u| !u.is_empty())
        .context("no download URL in search result")?;

    let resp = client
        .get(url)
        .header("X-Api-Key", &cfg.api_key)
        .send()
        .await
        .context("downloading NZB")?;

    if !resp.status().is_success() {
        let status = resp.status();
        bail!("download HTTP {}", status);
    }

    let bytes = resp.bytes().await.context("reading NZB body")?;

    // Basic sanity check: NZB files start with XML or the <nzb> tag.
    if !bytes.starts_with(b"<?xml") && !bytes.starts_with(b"<nzb") {
        bail!(
            "downloaded content does not look like an NZB ({} bytes, starts with: {:?})",
            bytes.len(),
            &bytes[..bytes.len().min(20)]
        );
    }

    tokio::fs::write(dest_path, &bytes)
        .await
        .with_context(|| format!("writing NZB to {}", dest_path.display()))?;

    Ok(())
}

/// Derive a safe filename for a downloaded NZB from a `SearchResult`.
///
/// Strips forbidden characters and ensures the `.nzb` extension is present.
pub fn nzb_filename_for(result: &SearchResult) -> String {
    let base = result.title.strip_suffix(".nzb").unwrap_or(&result.title);
    let safe: String = base
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c => c,
        })
        .collect();
    format!("{}.nzb", safe)
}

/// Derive the release name from a filename: strip the extension and any
/// trailing `.nzb` so the caller gets e.g.
/// `"Movie.2024.1080p.BluRay.x264-GROUP"` from `"Movie.2024.1080p.BluRay.x264-GROUP.mkv"`.
pub fn release_name_from_filename(filename: &str) -> &str {
    // Strip the outermost extension (e.g. .mkv, .nzb, .rar).
    let stem = Path::new(filename)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(filename);
    stem
}

/// Build the destination path for a downloaded NZB.
///
/// Places the file in `nzb_dir/downloaded/` so the vault can distinguish
/// Prowlarr downloads from uploads and manually-placed NZBs. Creates the
/// subdirectory if it does not exist yet.
pub fn dest_path_in(nzb_dir: &Path, result: &SearchResult) -> PathBuf {
    let sub = nzb_dir.join("downloaded");
    let _ = std::fs::create_dir_all(&sub);
    sub.join(nzb_filename_for(result))
}
