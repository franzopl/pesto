//! Published paths, wire identities and stable per-run naming decisions.

use std::path::Path;
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

/// Return an opaque yEnc filename, retaining `.par2` for recovery volumes.
pub(super) fn obfuscated_yenc_name(real_name: &str) -> String {
    let name = obfuscated_name();
    if Path::new(real_name)
        .extension()
        .is_some_and(|extension| extension.to_string_lossy().eq_ignore_ascii_case("par2"))
    {
        format!("{name}.par2")
    } else {
        name
    }
}

/// Canonical relative path presented to download clients and PAR2 FileDesc.
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
    if !path.is_ascii() {
        bail!(
            "published path `{name}` is not ASCII; use --compress=7z to preserve Unicode names inside an archive"
        );
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
    fn client_path_rejects_unsafe_or_non_ascii_names() {
        for name in [
            "",
            "/abs.bin",
            "Release/../x",
            "Release//x",
            "a\\b",
            "Árvore/legenda.txt",
        ] {
            assert!(
                normalize_client_path(name, Some("Release")).is_err(),
                "{name}"
            );
        }
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
    fn obfuscated_yenc_name_preserves_only_the_par2_extension() {
        assert!(obfuscated_yenc_name("recovery.PAR2").ends_with(".par2"));
        assert!(!obfuscated_yenc_name("movie.mkv").contains(".mkv"));
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
