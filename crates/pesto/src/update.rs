//! `pesto --update`: fetch the latest `pesto-v*` GitHub release for the
//! platform pesto is currently running on, verify its SHA256 against the
//! `SHA256SUMS` file published alongside it, and replace the running binary
//! in place.
//!
//! This is deliberately independent of `cargo install`/`cargo publish` (see
//! RELEASING.md) — it targets users who installed a prebuilt binary via
//! `scripts/install.sh`/`install.ps1` and have no Rust toolchain to
//! `cargo install` a new version with.

use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};

const REPO: &str = "franzopl/pesto";

/// How long a cached "latest version" result is trusted before `check_notice`
/// hits GitHub again. Keeps the startup notice off the hot path on every run
/// but for one request per day.
const CACHE_TTL: Duration = Duration::from_secs(24 * 60 * 60);

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

#[derive(Debug, Deserialize)]
struct GhAsset {
    name: String,
    browser_download_url: String,
}

/// Name of the release asset matching the platform this binary was built
/// for. Must match the names `.github/workflows/release-pesto.yml` uploads.
fn asset_name() -> &'static str {
    #[cfg(all(target_os = "linux", target_env = "musl"))]
    {
        "pesto-linux-x86_64-musl"
    }
    #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    {
        "pesto-linux-x86_64"
    }
    #[cfg(target_os = "windows")]
    {
        "pesto-windows-x86_64.exe"
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        compile_error!("pesto --update has no published release binary for this platform");
    }
}

/// Find the hex SHA256 for `want` inside a `sha256sum`-formatted checksums
/// file (`<hash>  <filename>` or `<hash> *<filename>`, one entry per line).
fn find_checksum(sums_text: &str, want: &str) -> Option<String> {
    for line in sums_text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.splitn(2, char::is_whitespace);
        let hash = parts.next()?;
        let name = parts.next()?.trim().trim_start_matches('*');
        if name == want {
            return Some(hash.to_lowercase());
        }
    }
    None
}

/// Fetch the GitHub release list and return the latest `pesto-v*` release
/// along with its parsed semver version.
async fn fetch_latest_release(client: &reqwest::Client) -> Result<(GhRelease, semver::Version)> {
    let releases: Vec<GhRelease> = client
        .get(format!("https://api.github.com/repos/{REPO}/releases"))
        .send()
        .await
        .context("fetching release list from GitHub")?
        .error_for_status()
        .context("GitHub API returned an error")?
        .json()
        .await
        .context("parsing GitHub release list")?;

    select_latest_release(releases)
}

/// Pick the highest-semver `pesto-v*` release out of `releases`.
///
/// The GitHub releases list is NOT reliably sorted newest-first (observed in
/// practice: a release can appear before a later one it was published
/// after). pesto is tagged/released independently of parmesan/penne (see
/// RELEASING.md), so this scans every `pesto-v*`, non-draft, non-prerelease
/// tag and keeps the one with the highest semver version rather than
/// trusting list order.
fn select_latest_release(releases: Vec<GhRelease>) -> Result<(GhRelease, semver::Version)> {
    let candidates = releases
        .into_iter()
        .filter(|r| !r.draft && !r.prerelease && r.tag_name.starts_with("pesto-v"))
        .filter_map(|r| {
            let version: semver::Version = r.tag_name.trim_start_matches("pesto-v").parse().ok()?;
            Some((r, version))
        });

    candidates
        .max_by(|(_, a), (_, b)| a.cmp(b))
        .context("no pesto-v* release with a valid semver tag found on GitHub")
}

fn current_version() -> Result<semver::Version> {
    if crate::DISPLAY_VERSION == "dev" {
        anyhow::bail!("this is a `dev` build; `pesto --update` only applies to release binaries");
    }
    env!("CARGO_PKG_VERSION")
        .parse()
        .context("current CARGO_PKG_VERSION is not valid semver")
}

/// Run `pesto --update`: check the latest release, download it if newer,
/// verify its checksum, and replace the running binary.
pub async fn run() -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("pesto-update/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    println!("Checking for updates...");
    let (latest, latest_version) = fetch_latest_release(&client).await?;
    let current_version = current_version()?;

    if latest_version <= current_version {
        println!(
            "pesto {current_version} is already up to date (latest release: {latest_version})."
        );
        return Ok(());
    }

    println!("Updating pesto {current_version} -> {latest_version}...");

    let want = asset_name();
    let asset = latest
        .assets
        .iter()
        .find(|a| a.name == want)
        .with_context(|| format!("release {} has no asset named {want}", latest.tag_name))?;
    let sums_asset = latest
        .assets
        .iter()
        .find(|a| a.name == "SHA256SUMS")
        .with_context(|| {
            format!(
                "release {} is missing SHA256SUMS; refusing to update without checksum verification",
                latest.tag_name
            )
        })?;

    let sums_text = client
        .get(&sums_asset.browser_download_url)
        .send()
        .await
        .context("downloading SHA256SUMS")?
        .error_for_status()
        .context("GitHub returned an error downloading SHA256SUMS")?
        .text()
        .await
        .context("reading SHA256SUMS")?;
    let expected_sha256 = find_checksum(&sums_text, want)
        .with_context(|| format!("SHA256SUMS has no entry for {want}"))?;

    println!("Downloading {want}...");
    let bytes = client
        .get(&asset.browser_download_url)
        .send()
        .await
        .context("downloading the update")?
        .error_for_status()
        .context("GitHub returned an error downloading the update")?
        .bytes()
        .await
        .context("reading the downloaded update")?;

    let actual_sha256 = {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(&bytes);
        format!("{:x}", hasher.finalize())
    };
    ensure!(
        actual_sha256 == expected_sha256,
        "checksum mismatch for {want}: expected {expected_sha256}, got {actual_sha256} — \
         refusing to install a corrupted or tampered binary"
    );

    let mut tmp = tempfile::NamedTempFile::new().context("creating a temp file for the update")?;
    tmp.write_all(&bytes)
        .context("writing the downloaded update to disk")?;
    tmp.flush().context("flushing the downloaded update")?;

    self_replace::self_replace(tmp.path()).context(
        "replacing the running pesto binary (check you have write permission to its directory)",
    )?;

    println!("Updated to pesto {latest_version}. Restart any running pesto process to use it.");
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct UpdateCache {
    /// Unix timestamp (seconds) of the last GitHub check.
    checked_at: u64,
    /// Latest version known as of `checked_at`, as plain semver (no `v` prefix).
    latest_version: String,
}

fn cache_path() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("update_check.json"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_fresh(checked_at: u64, now: u64) -> bool {
    now.saturating_sub(checked_at) < CACHE_TTL.as_secs()
}

fn read_cache(path: &std::path::Path) -> Option<UpdateCache> {
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

fn write_cache(path: &std::path::Path, latest_version: &semver::Version) {
    let cache = UpdateCache {
        checked_at: now_unix(),
        latest_version: latest_version.to_string(),
    };
    // Best-effort: a failed write just means the next run checks again.
    if let Ok(text) = serde_json::to_string(&cache) {
        let _ = std::fs::write(path, text);
    }
}

/// Build the one-line notice to print when `latest` is newer than `current`.
fn notice_text(current: &semver::Version, latest: &semver::Version) -> Option<String> {
    (latest > current).then(|| {
        format!(
            "A new version of pesto is available: v{latest} (you have v{current}). \
             Run `pesto --update` to update."
        )
    })
}

/// Courtesy check for a newer release, meant to be called once near startup
/// of a normal (non-`--update`) run. Backed by a 24h cache
/// (`~/.config/pesto/update_check.json`) so most runs never touch the
/// network; on a cache miss/expiry it hits GitHub with a short timeout so a
/// slow or unreachable API can never hold up an actual upload.
///
/// Returns `None` whenever there is nothing to report — up to date, no cache
/// directory available, or the check failed for any reason. This is a
/// courtesy notice, not a requirement, so errors are swallowed rather than
/// surfaced to the caller.
pub async fn check_notice() -> Option<String> {
    let current = current_version().ok()?;
    let path = cache_path();

    if let Some(path) = path.as_deref() {
        if let Some(cached) = read_cache(path) {
            if is_fresh(cached.checked_at, now_unix()) {
                let latest: semver::Version = cached.latest_version.parse().ok()?;
                return notice_text(&current, &latest);
            }
        }
    }

    let client = reqwest::Client::builder()
        .user_agent(concat!("pesto-update-check/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(2))
        .build()
        .ok()?;
    let (_, latest) = fetch_latest_release(&client).await.ok()?;

    if let Some(path) = path.as_deref() {
        write_cache(path, &latest);
    }

    notice_text(&current, &latest)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gh_release(tag: &str, draft: bool, prerelease: bool) -> GhRelease {
        GhRelease {
            tag_name: tag.to_string(),
            assets: Vec::new(),
            draft,
            prerelease,
        }
    }

    #[test]
    fn select_latest_release_ignores_list_order() {
        // Reproduces the actual GitHub API response order observed for this
        // repo: pesto-v0.5.9 (created earlier) listed before pesto-v0.5.10
        // (created later), with an unrelated tag interleaved.
        let releases = vec![
            gh_release("pesto-v0.5.9", false, false),
            gh_release("pesto-v0.5.10", false, false),
            gh_release("parmesan-v0.4.1", false, false),
            gh_release("pesto-v0.5.8", false, false),
        ];
        let (latest, version) = select_latest_release(releases).unwrap();
        assert_eq!(latest.tag_name, "pesto-v0.5.10");
        assert_eq!(version, semver::Version::parse("0.5.10").unwrap());
    }

    #[test]
    fn select_latest_release_skips_drafts_and_prereleases() {
        let releases = vec![
            gh_release("pesto-v0.6.0", true, false),
            gh_release("pesto-v0.5.9-rc1", false, true),
            gh_release("pesto-v0.5.8", false, false),
        ];
        let (latest, _) = select_latest_release(releases).unwrap();
        assert_eq!(latest.tag_name, "pesto-v0.5.8");
    }

    #[test]
    fn select_latest_release_skips_non_semver_and_non_pesto_tags() {
        let releases = vec![
            gh_release("v0.3.62", false, false),
            gh_release("pesto-vNOTSEMVER", false, false),
            gh_release("pesto-v0.5.8", false, false),
        ];
        let (latest, _) = select_latest_release(releases).unwrap();
        assert_eq!(latest.tag_name, "pesto-v0.5.8");
    }

    #[test]
    fn select_latest_release_errors_when_nothing_matches() {
        let releases = vec![gh_release("parmesan-v0.4.1", false, false)];
        assert!(select_latest_release(releases).is_err());
    }

    #[test]
    fn asset_name_is_one_of_the_release_pesto_workflow_assets() {
        // Must stay in lockstep with the `mv`/`sha256sum` filenames in
        // .github/workflows/release-pesto.yml — a mismatch here means
        // `--update` can never find its own platform's asset.
        assert!(matches!(
            asset_name(),
            "pesto-linux-x86_64" | "pesto-linux-x86_64-musl" | "pesto-windows-x86_64.exe"
        ));
    }

    #[test]
    fn find_checksum_matches_two_space_gnu_format() {
        let sums = "abc123  pesto-linux-x86_64\ndef456  pesto-windows-x86_64.exe\n";
        assert_eq!(
            find_checksum(sums, "pesto-linux-x86_64"),
            Some("abc123".to_string())
        );
        assert_eq!(
            find_checksum(sums, "pesto-windows-x86_64.exe"),
            Some("def456".to_string())
        );
    }

    #[test]
    fn find_checksum_matches_binary_mode_asterisk_format() {
        let sums = "ABC123 *pesto-linux-x86_64-musl\n";
        assert_eq!(
            find_checksum(sums, "pesto-linux-x86_64-musl"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn find_checksum_returns_none_for_unknown_asset() {
        let sums = "abc123  pesto-linux-x86_64\n";
        assert_eq!(find_checksum(sums, "pesto-windows-x86_64.exe"), None);
    }

    #[test]
    fn find_checksum_ignores_blank_lines() {
        let sums = "\nabc123  pesto-linux-x86_64\n\n";
        assert_eq!(
            find_checksum(sums, "pesto-linux-x86_64"),
            Some("abc123".to_string())
        );
    }

    #[test]
    fn notice_text_is_none_when_up_to_date_or_older() {
        let current: semver::Version = "1.2.0".parse().unwrap();
        assert_eq!(notice_text(&current, &"1.2.0".parse().unwrap()), None);
        assert_eq!(notice_text(&current, &"1.1.0".parse().unwrap()), None);
    }

    #[test]
    fn notice_text_mentions_both_versions_and_the_update_flag() {
        let current: semver::Version = "1.2.0".parse().unwrap();
        let latest: semver::Version = "1.3.0".parse().unwrap();
        let text = notice_text(&current, &latest).unwrap();
        assert!(text.contains("1.3.0"));
        assert!(text.contains("1.2.0"));
        assert!(text.contains("--update"));
    }

    #[test]
    fn cache_is_fresh_inside_ttl_and_stale_past_it() {
        let checked_at = 1_000_000;
        assert!(is_fresh(checked_at, checked_at));
        assert!(is_fresh(checked_at, checked_at + CACHE_TTL.as_secs() - 1));
        assert!(!is_fresh(checked_at, checked_at + CACHE_TTL.as_secs()));
        assert!(!is_fresh(checked_at, checked_at + CACHE_TTL.as_secs() + 1));
    }

    #[test]
    fn cache_round_trips_through_json() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update_check.json");
        let version: semver::Version = "9.9.9".parse().unwrap();

        assert!(read_cache(&path).is_none());
        write_cache(&path, &version);
        let cached = read_cache(&path).unwrap();
        assert_eq!(cached.latest_version, "9.9.9");
        assert!(is_fresh(cached.checked_at, now_unix()));
    }
}
