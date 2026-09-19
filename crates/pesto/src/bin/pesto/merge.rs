//! Offline merging of per-episode NZBs into season NZBs.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

// ── merge-season ─────────────────────────────────────────────────────────────

/// Group all `.nzb` files in `dir` by season, merge each group into one
/// combined NZB, and write it beside the source files.
pub(super) fn run_merge_season(
    dir: &Path,
    display_name: Option<&str>,
    nzb_tags: Vec<String>,
) -> Result<()> {
    use std::collections::BTreeMap;

    anyhow::ensure!(dir.is_dir(), "{} is not a directory", dir.display());

    // Collect .nzb files, sorted so episodes come out in order.
    let mut nzb_files: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("nzb"))
        .collect();
    nzb_files.sort();

    anyhow::ensure!(
        !nzb_files.is_empty(),
        "no .nzb files found in {}",
        dir.display()
    );

    // Group files by season key.  A season key is the show name plus the
    // season number extracted from the filename, e.g. "Batwheels.S02".
    // Files with no recognisable season marker fall into a catch-all group
    // named after the directory.
    let fallback_key = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "season".into());

    let mut groups: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for path in &nzb_files {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let key = season_key(&stem).unwrap_or_else(|| fallback_key.clone());
        groups.entry(key).or_default().push(path.clone());
    }

    for (key, files) in &groups {
        // Skip if only one file in the group — nothing to merge.
        // (Single-file "seasons" are already complete NZBs.)
        if files.len() < 2 {
            eprintln!("skipping {key}: only one NZB in group");
            continue;
        }

        let output_path = dir.join(format!("{key}.nzb"));

        // Don't include the output file itself if it already exists in `files`.
        let sources: Vec<&PathBuf> = files
            .iter()
            .filter(|p| p.as_path() != output_path.as_path())
            .collect();

        eprintln!(
            "\nmerging {} episodes into {}",
            sources.len(),
            output_path.display()
        );

        let mut combined_segments: Vec<pesto::poster::PostedSegment> = Vec::new();
        let mut poster = String::new();
        let mut all_groups: Vec<String> = Vec::new();

        for src in &sources {
            let content = std::fs::read_to_string(src)
                .with_context(|| format!("reading {}", src.display()))?;
            let parsed = pesto::nzb::parse(&content)
                .with_context(|| format!("parsing {}", src.display()))?;

            let ep_name = src
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| src.display().to_string());
            let file_count = parsed
                .segments
                .iter()
                .map(|s| &s.file_name)
                .collect::<std::collections::HashSet<_>>()
                .len();
            let seg_count = parsed.segments.len();
            eprintln!("  + {ep_name}  ({file_count} file(s), {seg_count} segment(s))");

            if poster.is_empty() {
                poster = parsed.poster;
            }
            for g in parsed.groups {
                if !all_groups.contains(&g) {
                    all_groups.push(g);
                }
            }
            combined_segments.extend(parsed.segments);
        }

        combined_segments.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.part.cmp(&b.part)));

        let meta = pesto::nzb::NzbMeta {
            name: display_name
                .map(str::to_string)
                .or_else(|| Some(key.clone())),
            password: None,
            category: None,
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: None,
            mal_id: None,
            tags: nzb_tags.clone(),
        };
        // Segments here come from `nzb::parse`, which always leaves
        // `wire_name` empty (see its doc comment) — there is no live wire
        // identity to mirror when merging already-generated `.nzb` files,
        // so the obfuscate mode passed here is moot; `None` just keeps this
        // call explicit about that.
        let xml = pesto::nzb::generate(
            &all_groups,
            &combined_segments,
            &meta,
            pesto::config::ObfuscateMode::None,
        );

        std::fs::write(&output_path, &xml)
            .with_context(|| format!("writing {}", output_path.display()))?;

        eprintln!(
            "wrote {} ({} total segments)",
            output_path.display(),
            combined_segments.len()
        );
    }

    Ok(())
}

/// Extract a season group key from an NZB stem.
///
/// `Batwheels.S02E32-E33.1080p.NF.WEB-DL` → `Batwheels.S02`
/// `Show.Name.s01e01.720p`                  → `Show.Name.S01`
/// `Random.File`                            → `None`
fn season_key(stem: &str) -> Option<String> {
    let lower = stem.to_lowercase();
    let bytes = lower.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b's' {
            // Require at least one digit after 's'.
            let mut j = i + 1;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if j == i + 1 {
                continue; // no digits after 's'
            }
            // Require 'e' followed by at least one digit.
            if j < bytes.len()
                && bytes[j] == b'e'
                && j + 1 < bytes.len()
                && bytes[j + 1].is_ascii_digit()
            {
                // stem[..j] covers everything up to 'e', including 'SXX'.
                // Reconstruct with original case up to the 's', then uppercase season.
                let prefix = &stem[..i];
                let season_num = &stem[i + 1..j]; // digits only
                return Some(format!(
                    "{prefix}S{:0>2}",
                    season_num.parse::<u32>().unwrap_or(0)
                ));
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::season_key;

    #[test]
    fn season_key_standard_sxxexx() {
        assert_eq!(
            season_key("Batwheels.S02E32-E33.1080p.NF.WEB-DL.DDP5.1.H.264.DUAL-BiOMA"),
            Some("Batwheels.S02".into())
        );
        assert_eq!(
            season_key("Show.Name.S01E01.720p.BluRay"),
            Some("Show.Name.S01".into())
        );
        assert_eq!(season_key("Series.s03e05.HDTV"), Some("Series.S03".into()));
    }

    #[test]
    fn season_key_no_season_returns_none() {
        assert_eq!(season_key("Random.Movie.2024.1080p"), None);
        assert_eq!(season_key("file"), None);
    }
}
