use super::minimal_file;
use crate::config::*;

#[test]
fn no_hooks_defaults_to_false_when_unset() {
    let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
    assert!(!cfg.no_hooks);
}

#[test]
fn cli_no_hooks_overrides_config_false() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            no_hooks: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(cfg.no_hooks, "--no-hooks should override an unset config");
}

#[test]
fn config_no_hooks_true_survives_absent_cli_flag() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [posting]
        groups = ["alt.test"]
        [output]
        no_hooks = true
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert!(
        cfg.no_hooks,
        "no_hooks = true in config.toml should apply without --no-hooks on the CLI"
    );
}

// Regression for #17: `check_delay` set in the TOML config must imply
// `check = true`, matching the documented `--check-delay` CLI behaviour.
// Previously the check only auto-enabled for the CLI flag, so a config-only
// `check_delay` silently skipped the post-upload STAT pass.
#[test]
fn config_check_delay_implies_check() {
    let file: FileConfig =
        toml::from_str("[server]\nhost=\"h\"\n[posting]\ngroups=[\"a\"]\ncheck_delay=60\n")
            .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert!(cfg.check, "check_delay in config must enable check");
    assert_eq!(cfg.check_delay_secs, 60);
}

#[test]
fn config_check_on_by_default() {
    let file: FileConfig =
        toml::from_str("[server]\nhost=\"h\"\n[posting]\ngroups=[\"a\"]\n").unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert!(cfg.check, "streaming check should default to on");
    assert_eq!(cfg.check_delay_secs, 5);
}

#[test]
fn config_explicit_check_false_disables_it() {
    let file: FileConfig =
        toml::from_str("[server]\nhost=\"h\"\n[posting]\ngroups=[\"a\"]\ncheck=false\n").unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert!(!cfg.check);
    assert_eq!(cfg.check_delay_secs, 5);
}

#[test]
fn exclusion_config_defaults_and_precedence() {
    let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
    assert!(cfg.exclude.is_empty());
    assert!(!cfg.no_exclude);
    let file = || {
        toml::from_str::<FileConfig>(
            r#"
            exclude = ["*.tmp"]
            no_exclude = true
            [server]
            host = "h"
            [posting]
            groups = ["alt.test"]
        "#,
        )
        .unwrap()
    };
    let cfg = Config::resolve(file(), Overrides::default()).unwrap();
    assert_eq!(cfg.exclude, ["*.tmp"]);
    assert!(cfg.no_exclude);
    let cfg = Config::resolve(
        file(),
        Overrides {
            exclude: Some(vec!["*.bak".into()]),
            no_exclude: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.exclude, ["*.bak"]);
    assert!(!cfg.no_exclude);
    let cfg = Config::resolve(
        file(),
        Overrides {
            exclude: Some(vec![]),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(cfg.exclude.is_empty());
}

#[test]
fn invalid_exclusion_globs_fail_during_config_resolution_unless_disabled() {
    let file = || {
        let mut file = minimal_file();
        file.exclude = vec!["[".into()];
        file
    };
    assert!(Config::resolve(file(), Overrides::default()).is_err());
    assert!(Config::resolve(
        file(),
        Overrides {
            no_exclude: Some(true),
            ..Default::default()
        }
    )
    .is_ok());
}

#[test]
fn extension_config_defaults_normalization_and_cli_precedence() {
    let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
    assert!(cfg.ext.is_empty());
    let file = || {
        toml::from_str::<FileConfig>(
            r#"
            ext = [".MKV", "srt"]
            exclude = ["*.tmp"]
            [server]
            host = "h"
            [posting]
            groups = ["alt.test"]
        "#,
        )
        .unwrap()
    };
    let cfg = Config::resolve(file(), Overrides::default()).unwrap();
    assert_eq!(cfg.ext, ["mkv", "srt"]);
    assert_eq!(cfg.exclude, ["*.tmp"]);
    let cfg = Config::resolve(
        file(),
        Overrides {
            ext: Some(vec![".MP4".into()]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.ext, ["mp4"]);
    let cfg = Config::resolve(
        file(),
        Overrides {
            ext: Some(vec![]),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(cfg.ext.is_empty());
}
