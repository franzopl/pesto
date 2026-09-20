use super::*;
use pesto::config::{Config, FileConfig, Overrides};

use crate::cleanup::CleanupMode;

fn test_upload_params(groups: Vec<String>) -> Arc<UploadParams> {
    let mut file = FileConfig::default();
    file.server.host = Some("news.example.com".into());
    file.posting.groups = Some(groups);
    let config = Config::resolve(file, Overrides::default()).unwrap();
    Arc::new(UploadParams {
        config: Arc::new(config),
        archive_password_raw: None,
        nzb_default: None,
        json_mode: true,
        out: None,
        write_history: false,
        renderer_opts: pesto::progress::RendererOptions::default(),
        ext_filter: Vec::new(),
        cleanup_mode: CleanupMode::Leave,
    })
}

#[test]
fn force_season_group_is_noop_outside_season() {
    let params = test_upload_params(vec!["alt.binaries.a".into(), "alt.binaries.b".into()]);
    let original_groups = params.config.groups.clone();

    let out = force_season_group(Arc::clone(&params), false);

    assert_eq!(
        out.config.groups, original_groups,
        "a plain --each batch must keep letting each entry pick its own group"
    );
}

#[test]
fn force_season_group_collapses_multiple_configured_groups_to_one_target() {
    let params = test_upload_params(vec![
        "alt.binaries.a".into(),
        "alt.binaries.b".into(),
        "alt.binaries.c".into(),
    ]);

    let out = force_season_group(params, true);

    assert_eq!(
        out.config.groups.len(),
        1,
        "must collapse to a single forced target: {:?}",
        out.config.groups
    );
    assert!(["alt.binaries.a", "alt.binaries.b", "alt.binaries.c"]
        .contains(&out.config.groups[0].as_str()));
}

#[test]
fn force_season_group_preserves_a_cross_post_target() {
    let params = test_upload_params(vec!["alt.binaries.a+alt.binaries.b".into()]);

    let out = force_season_group(params, true);

    assert_eq!(
        out.config.groups,
        vec!["alt.binaries.a+alt.binaries.b".to_string()]
    );
}

#[test]
fn force_season_group_result_is_deterministic_for_every_episode() {
    let params = test_upload_params(vec![
        "alt.binaries.a".into(),
        "alt.binaries.b".into(),
        "alt.binaries.c".into(),
    ]);

    let forced = force_season_group(params, true);

    for _ in 0..50 {
        let picked_again = pesto::poster::pick_post_group(&forced.config.groups);
        assert_eq!(picked_again.join("+"), forced.config.groups[0]);
    }
}

#[test]
fn is_artifact_entry_matches_nfo_and_nzb_case_insensitively() {
    assert!(is_artifact_entry(Path::new("Show.nfo")));
    assert!(is_artifact_entry(Path::new("Show.NZB")));
    assert!(is_artifact_entry(Path::new("/a/b/c.NfO")));
    assert!(!is_artifact_entry(Path::new("Show.mkv")));
    assert!(!is_artifact_entry(Path::new("Show")));
    assert!(!is_artifact_entry(Path::new("nfo")));
}

#[test]
fn top_level_entries_skips_generated_artifacts() {
    let dir = std::env::temp_dir().join(format!(
        "pesto_each_artifact_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ep01.mkv"), b"x").unwrap();
    // Orphan artifacts left in the input directory by a previous run.
    std::fs::write(dir.join("ep01.nfo"), b"x").unwrap();
    std::fs::write(dir.join("ep01.nzb"), b"x").unwrap();

    let names: Vec<String> = top_level_entries(&dir, &[])
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["ep01.mkv"]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn matches_ext_filter_is_case_insensitive_and_empty_means_everything() {
    assert!(matches_ext_filter(Path::new("Show.MKV"), &["mkv".into()]));
    assert!(matches_ext_filter(Path::new("Show.mkv"), &["MKV".into()]));
    assert!(!matches_ext_filter(Path::new("Show.srt"), &["mkv".into()]));
    assert!(matches_ext_filter(Path::new("Show.srt"), &[]));
    assert!(!matches_ext_filter(Path::new("Show"), &["mkv".into()]));
}

#[test]
fn top_level_entries_filters_loose_files_by_ext_but_keeps_directories() {
    let dir = std::env::temp_dir().join(format!(
        "pesto_each_ext_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(dir.join("Extras")).unwrap();
    std::fs::write(dir.join("ep01.mkv"), b"x").unwrap();
    std::fs::write(dir.join("ep01.srt"), b"x").unwrap();

    let names: Vec<String> = top_level_entries(&dir, &["mkv".to_string()])
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    // The loose .srt sibling is dropped; the subdirectory is kept even
    // though "Extras" has no matching extension of its own, since a
    // matching file could live inside it.
    assert_eq!(names, ["ep01.mkv", "Extras"]);

    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn apply_ext_filter_drops_non_matching_and_errors_when_nothing_left() {
    let mut inputs = vec![
        pesto::walk::InputFile {
            path: PathBuf::from("ep01.mkv"),
            name: "ep01.mkv".to_string(),
        },
        pesto::walk::InputFile {
            path: PathBuf::from("ep01.srt"),
            name: "ep01.srt".to_string(),
        },
    ];
    apply_ext_filter(&mut inputs, &["mkv".to_string()], "entry").unwrap();
    assert_eq!(inputs.len(), 1);
    assert_eq!(inputs[0].name, "ep01.mkv");

    let mut only_subs = vec![pesto::walk::InputFile {
        path: PathBuf::from("ep01.srt"),
        name: "ep01.srt".to_string(),
    }];
    assert!(apply_ext_filter(&mut only_subs, &["mkv".to_string()], "entry").is_err());

    // Empty filter is a no-op.
    let mut untouched = vec![pesto::walk::InputFile {
        path: PathBuf::from("ep01.srt"),
        name: "ep01.srt".to_string(),
    }];
    apply_ext_filter(&mut untouched, &[], "entry").unwrap();
    assert_eq!(untouched.len(), 1);
}

#[test]
fn derive_season_nzb_path_prefers_explicit_out() {
    let path = derive_season_nzb_path(
        Some(Path::new("/custom/out.nzb")),
        Path::new("/downloads/Show.S01"),
        Some("/nzbs"),
    );
    assert_eq!(path, PathBuf::from("/custom/out.nzb"));
}

#[test]
fn derive_season_nzb_path_names_after_entry_under_nzb_dir() {
    let path = derive_season_nzb_path(None, Path::new("/downloads/Show.S01"), Some("/nzbs"));
    assert_eq!(path, PathBuf::from("/nzbs/Show.S01.nzb"));
}

#[test]
fn derive_season_nzb_path_names_dot_after_current_directory() {
    let cwd = std::env::current_dir().unwrap();
    let expected_name = cwd
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "season".to_string());

    let path = derive_season_nzb_path(None, Path::new("."), Some("/nzbs"));

    assert_eq!(
        path,
        PathBuf::from("/nzbs").join(format!("{expected_name}.nzb"))
    );
}

#[test]
fn derive_season_nzb_path_falls_back_to_cwd_relative_name() {
    let path = derive_season_nzb_path(None, Path::new("/downloads/Show.S01"), None);
    assert_eq!(path, PathBuf::from("Show.S01.nzb"));
}
