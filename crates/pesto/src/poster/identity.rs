//! Published paths, wire identities and stable per-run naming decisions.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Result};

use crate::article::{format_rfc2822, obfuscated_name, rand_u64};
use crate::resume::PersistedWireIdentity;

pub(super) fn persisted_identity(
    subject_name: &str,
    yenc_name: &str,
    from: &str,
    date: &(Option<String>, Option<u64>),
) -> PersistedWireIdentity {
    PersistedWireIdentity {
        subject_name: subject_name.to_owned(),
        yenc_name: yenc_name.to_owned(),
        from: from.to_owned(),
        date: date.0.clone(),
        unix_date: date.1,
    }
}

/// Choose one posting target for the run and expand its cross-post groups.
pub fn pick_post_group(groups: &[String]) -> Vec<String> {
    let target = match groups {
        [] => return Vec::new(),
        [one] => one.as_str(),
        many => {
            let idx = (rand_u64() % many.len() as u64) as usize;
            many[idx].as_str()
        }
    };
    target
        .split(['+', ','])
        .map(|group| group.trim().to_string())
        .collect()
}

/// Base name used by a flat PAR2 recovery set.
pub(super) fn par2_base(name: &str) -> &str {
    name.split('/').next().unwrap_or(name)
}

/// PAR2 base with a split-archive volume suffix removed.
pub(super) fn par2_release_base(name: &str) -> &str {
    let trimmed = match crate::compress::volume_suffix(name) {
        Some(suffix) => &name[..name.len() - suffix.len()],
        None => name,
    };
    par2_base(trimmed)
}

/// Return an opaque yEnc filename, retaining the PAR2 technical suffix.
pub(super) fn obfuscated_yenc_name(real_name: &str) -> String {
    with_par2_suffix(obfuscated_name(), real_name)
}

/// Append `real_name`'s PAR2 suffix (`.volNNN+NNN.par2` or `.par2`) to an
/// opaque stem; non-PAR2 names get the stem alone.
///
/// NZBGet's direct rename (`DirectRenamer::BuildNewParName`, v26.1) keeps a
/// `.volNNN+NNN.par2` suffix from the downloaded name and otherwise invents
/// `.volNNN+01.PAR2`; par-check then counts the recovery blocks it can still
/// fetch from that `+NN`. Without the real suffix every volume looks like one
/// block, and a repair needing more blocks than there are volumes fails
/// without fetching them. The suffix only states volume geometry, which the
/// volume's own Recovery Slice packets already carry.
pub(super) fn with_par2_suffix(stem: String, real_name: &str) -> String {
    match par2_suffix(real_name) {
        Some(suffix) => format!("{stem}{suffix}"),
        None => stem,
    }
}

/// The trailing `.volNNN+NNN.par2` of a recovery volume, `.par2` of an index,
/// or `None` for a non-PAR2 name. Matching is ASCII case-insensitive.
fn par2_suffix(name: &str) -> Option<&str> {
    // ASCII lowercasing keeps byte offsets, so indices map back onto `name`.
    let lower = name.to_ascii_lowercase();
    let stem = lower.strip_suffix(".par2")?;
    let is_digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    let start = stem
        .rfind(".vol")
        .filter(|&at| {
            stem[at + 4..]
                .split_once('+')
                .is_some_and(|(first, count)| is_digits(first) && is_digits(count))
        })
        .unwrap_or(stem.len());
    Some(&name[start..])
}

/// Canonical relative path presented to download clients and PAR2 FileDesc.
///
/// Non-ASCII names are published unchanged as UTF-8, with no Unicode
/// normalization, matching nyuu, ngPost and ParPar (whose FileDesc
/// `--ascii-charset` defaults to `utf-8`).
pub(super) fn normalize_client_path<'a>(
    name: &'a str,
    release_root: Option<&str>,
) -> Result<&'a str> {
    if name.is_empty() || name.starts_with('/') || name.contains('\\') || name.contains('\0') {
        bail!("invalid published path `{name}`");
    }
    let path = match release_root {
        Some(root) => name
            .strip_prefix(root)
            .and_then(|rest| rest.strip_prefix('/'))
            .unwrap_or(name),
        None => name,
    };
    if path.is_empty()
        || path
            .split('/')
            .any(|component| component.is_empty() || component == "." || component == "..")
    {
        bail!("published path `{name}` does not produce a safe relative client path");
    }
    Ok(path)
}

/// Resolve the configured article date to its header and optional Unix time.
pub(super) fn resolve_date(mode: Option<&str>) -> (Option<String>, Option<u64>) {
    match mode {
        None => (None, None),
        Some("now") => {
            let now = SystemTime::now();
            let timestamp = now
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            (Some(format_rfc2822(now)), Some(timestamp))
        }
        Some("random") => {
            // Stay inside servers' age windows while avoiding a shared batch
            // timestamp that correlates every article in an obfuscated run.
            let offset_secs = rand_u64() % (2 * 3600);
            let time = SystemTime::now()
                .checked_sub(Duration::from_secs(offset_secs))
                .unwrap_or(UNIX_EPOCH);
            let timestamp = time
                .duration_since(UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs();
            (Some(format_rfc2822(time)), Some(timestamp))
        }
        Some(fixed) => (Some(fixed.to_string()), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn persisted_wire_identity_copies_every_wire_field() {
        let date = (
            Some("Tue, 14 Jan 2025 10:00:00 +0000".to_string()),
            Some(42),
        );
        let identity = persisted_identity("subject", "file.bin", "poster@example", &date);
        assert_eq!(identity.subject_name, "subject");
        assert_eq!(identity.yenc_name, "file.bin");
        assert_eq!(identity.from, "poster@example");
        assert_eq!(identity.date, date.0);
        assert_eq!(identity.unix_date, date.1);
    }

    #[test]
    fn post_group_empty_is_empty() {
        assert!(pick_post_group(&[]).is_empty());
    }

    #[test]
    fn post_group_single_returns_that_group() {
        let groups = vec!["alt.binaries.test".to_string()];
        assert_eq!(pick_post_group(&groups), groups);
    }

    #[test]
    fn post_group_picks_one_configured_target() {
        let groups = vec![
            "alt.a".to_string(),
            "alt.b".to_string(),
            "alt.c".to_string(),
        ];
        for _ in 0..100 {
            let picked = pick_post_group(&groups);
            assert_eq!(picked.len(), 1);
            assert!(groups.contains(&picked[0]));
        }
    }

    #[test]
    fn post_group_expands_and_trims_cross_post_targets() {
        let groups = vec!["alt.a  +  alt.b".to_string()];
        assert_eq!(pick_post_group(&groups), vec!["alt.a", "alt.b"]);
    }

    #[test]
    fn post_group_accepts_deprecated_comma_separator() {
        let groups = vec!["alt.a,alt.b".to_string()];
        assert_eq!(pick_post_group(&groups), vec!["alt.a", "alt.b"]);
    }

    #[test]
    fn post_group_expands_the_selected_target_from_a_pool() {
        let groups = vec!["alt.a+alt.b".to_string(), "alt.c".to_string()];
        for _ in 0..100 {
            let picked = pick_post_group(&groups);
            assert!(picked == ["alt.a", "alt.b"] || picked == ["alt.c"]);
        }
    }

    #[test]
    fn client_path_strips_one_common_release_root() {
        assert_eq!(
            normalize_client_path("Release/Season01/ep01.mkv", Some("Release")).unwrap(),
            "Season01/ep01.mkv"
        );
    }

    #[test]
    fn client_path_preserves_distinct_top_level_roots() {
        assert_eq!(
            normalize_client_path("ShowA/s01/ep01.mkv", None).unwrap(),
            "ShowA/s01/ep01.mkv"
        );
    }

    #[test]
    fn client_path_keeps_a_loose_file_unchanged() {
        assert_eq!(
            normalize_client_path("movie.mkv", None).unwrap(),
            "movie.mkv"
        );
    }

    #[test]
    fn client_path_rejects_unsafe_names() {
        for name in ["", "/abs.bin", "Release/../x", "Release//x", "a\\b"] {
            assert!(
                normalize_client_path(name, Some("Release")).is_err(),
                "{name}"
            );
        }
    }

    #[test]
    fn client_path_keeps_non_ascii_names_unchanged() {
        assert_eq!(
            normalize_client_path("Curso/1_ Todo dia/010 - Quem é você.ts", Some("Curso")).unwrap(),
            "1_ Todo dia/010 - Quem é você.ts"
        );
        assert_eq!(normalize_client_path("Ação.pdf", None).unwrap(), "Ação.pdf");
        // NFD input (combining acute, as macOS stores names) stays NFD.
        assert_eq!(
            normalize_client_path("Cafe\u{301}.txt", None).unwrap(),
            "Cafe\u{301}.txt"
        );
    }

    #[test]
    fn par2_base_keeps_a_single_component() {
        assert_eq!(par2_base("movie.mkv"), "movie.mkv");
    }

    #[test]
    fn par2_base_uses_the_top_level_path_component() {
        assert_eq!(par2_base("Season01/ep01.mkv"), "Season01");
        assert_eq!(par2_base("a/b/c.bin"), "a");
    }

    #[test]
    fn par2_base_accepts_an_empty_name_without_panicking() {
        assert_eq!(par2_base(""), "");
    }

    #[test]
    fn par2_release_base_strips_rar_volume_suffixes() {
        assert_eq!(par2_release_base("archive.part01.rar"), "archive");
        assert_eq!(par2_release_base("archive.part1.rar"), "archive");
    }

    #[test]
    fn par2_release_base_strips_seven_zip_volume_suffixes() {
        assert_eq!(par2_release_base("archive.7z.001"), "archive");
    }

    #[test]
    fn par2_release_base_preserves_regular_names() {
        assert_eq!(par2_release_base("movie.mkv"), "movie.mkv");
        assert_eq!(par2_release_base("archive.rar"), "archive.rar");
        assert_eq!(par2_release_base("archive.7z"), "archive.7z");
    }

    #[test]
    fn par2_release_base_uses_the_release_root_for_season_paths() {
        assert_eq!(par2_release_base("Season01/ep01.mkv"), "Season01");
    }

    #[test]
    fn obfuscated_yenc_name_preserves_only_the_par2_suffix() {
        assert!(obfuscated_yenc_name("recovery.PAR2").ends_with(".PAR2"));
        assert!(!obfuscated_yenc_name("recovery.PAR2").contains("recovery"));
        assert!(!obfuscated_yenc_name("movie.mkv").contains(".mkv"));

        let volume = obfuscated_yenc_name("Show/movie.mkv.vol007+008.par2");
        assert!(volume.ends_with(".vol007+008.par2"), "{volume}");
        assert!(!volume.contains("movie"), "{volume}");
        assert!(obfuscated_yenc_name("big.vol1023+1024.PAR2").ends_with(".vol1023+1024.PAR2"));
    }

    #[test]
    fn par2_suffix_requires_a_well_formed_volume_marker() {
        assert_eq!(par2_suffix("a.vol000+001.par2"), Some(".vol000+001.par2"));
        assert_eq!(par2_suffix("a.par2"), Some(".par2"));
        assert_eq!(par2_suffix("a.volume.par2"), Some(".par2"));
        assert_eq!(par2_suffix("a.vol12.par2"), Some(".par2"));
        assert_eq!(par2_suffix("a.vol+1.par2"), Some(".par2"));
        assert_eq!(par2_suffix("a.mkv"), None);
    }

    #[test]
    fn date_none_omits_the_header() {
        assert_eq!(resolve_date(None), (None, None));
    }

    #[test]
    fn date_now_returns_rfc2822_and_unix_time() {
        let (date, timestamp) = resolve_date(Some("now"));
        let date = date.unwrap();
        assert!(date.ends_with("+0000"));
        assert!(date.contains(':'));
        assert!(timestamp.unwrap() > 0);
    }

    #[test]
    fn random_date_returns_rfc2822_and_unix_time() {
        let (date, timestamp) = resolve_date(Some("random"));
        assert!(date.unwrap().ends_with("+0000"));
        assert!(timestamp.unwrap() > 0);
    }

    #[test]
    fn random_date_is_within_two_hours() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let (_, timestamp) = resolve_date(Some("random"));
        let timestamp = timestamp.unwrap();
        assert!(timestamp <= now);
        assert!(now - timestamp < 2 * 3600 + 1);
    }

    #[test]
    fn fixed_date_is_returned_verbatim_without_a_unix_time() {
        let fixed = "Tue, 14 Jan 2025 10:00:00 +0000";
        assert_eq!(resolve_date(Some(fixed)), (Some(fixed.to_string()), None));
    }
}
