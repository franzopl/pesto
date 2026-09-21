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
