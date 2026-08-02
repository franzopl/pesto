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

use anyhow::{ensure, Context, Result};
use serde::Deserialize;

const REPO: &str = "franzopl/pesto";

#[derive(Debug, Deserialize)]
struct GhRelease {
    tag_name: String,
    assets: Vec<GhAsset>,
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

/// Run `pesto --update`: check the latest release, download it if newer,
/// verify its checksum, and replace the running binary.
pub async fn run() -> Result<()> {
    let client = reqwest::Client::builder()
        .user_agent(concat!("pesto-update/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("building HTTP client")?;

    println!("Checking for updates...");
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

    // GitHub lists releases newest-first, and pesto is tagged/released
    // independently of parmesan/penne (see RELEASING.md) — the first
    // `pesto-v*` tag encountered here is the latest pesto release.
    let latest = releases
        .into_iter()
        .find(|r| r.tag_name.starts_with("pesto-v"))
        .context("no pesto-v* release found on GitHub")?;

    let latest_version_str = latest.tag_name.trim_start_matches("pesto-v");
    let latest_version: semver::Version = latest_version_str
        .parse()
        .with_context(|| format!("release tag `{}` is not valid semver", latest.tag_name))?;
    let current_version: semver::Version = env!("CARGO_PKG_VERSION")
        .parse()
        .context("current CARGO_PKG_VERSION is not valid semver")?;

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

#[cfg(test)]
mod tests {
    use super::*;

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
}
