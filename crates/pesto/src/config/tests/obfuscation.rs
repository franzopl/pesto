use super::minimal_file;
use crate::config::*;

#[test]
fn obfuscate_mode_parses_from_toml_and_defaults_to_none() {
    let file: FileConfig = toml::from_str("[posting]\nobfuscate = \"full\"\n").unwrap();
    assert_eq!(file.posting.obfuscate, Some(ObfuscateMode::Full));
    assert_eq!(ObfuscateMode::default(), ObfuscateMode::None);
}

#[test]
fn paranoid_toml_value_remains_an_alias_for_article() {
    let file: FileConfig = toml::from_str("[posting]\nobfuscate = 'paranoid'\n").unwrap();
    assert_eq!(file.posting.obfuscate, Some(ObfuscateMode::Article));
}

#[test]
fn header_fragmented_toml_value_is_an_alias_for_full() {
    let file: FileConfig = toml::from_str("[posting]\nobfuscate = 'header-fragmented'\n").unwrap();
    assert_eq!(file.posting.obfuscate, Some(ObfuscateMode::Full));
}

#[test]
fn file_counter_defaults_follow_mode_policy() {
    for (mode, expected) in [
        (ObfuscateMode::None, true),
        (ObfuscateMode::FullShared, true),
        (ObfuscateMode::Light, true),
        (ObfuscateMode::Full, false),
        (ObfuscateMode::Article, false),
    ] {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                obfuscate: Some(mode),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(
            cfg.file_counter, expected,
            "obfuscate={mode:?} should default file_counter to {expected}"
        );
    }
}

#[test]
fn obfuscation_policy_locks_the_five_wire_contracts() {
    let none = ObfuscateMode::None.policy();
    assert_eq!(none.subject_scope, IdentityScope::RealPath);
    assert_eq!(none.subject_yenc_relation, NameRelation::Same);
    assert!(none.publish_par2_index && none.allow_file_counter);

    let light = ObfuscateMode::Light.policy();
    assert!(light.shared_release_prefix);
    assert_eq!(light.subject_yenc_relation, NameRelation::Same);

    let shared = ObfuscateMode::FullShared.policy();
    assert_eq!(shared.subject_scope, IdentityScope::Release);
    assert_eq!(shared.yenc_scope, IdentityScope::Release);
    assert_eq!(shared.from_scope, IdentityScope::Release);
    assert_eq!(shared.subject_yenc_relation, NameRelation::SharedPrefix);

    let full = ObfuscateMode::Full.policy();
    assert_eq!(full.subject_scope, IdentityScope::Article);
    assert_eq!(full.yenc_scope, IdentityScope::File);
    assert_eq!(full.from_scope, IdentityScope::Article);
    assert_eq!(full.subject_yenc_relation, NameRelation::Independent);
    assert!(!full.publish_par2_index && !full.allow_file_counter);

    let article = ObfuscateMode::Article.policy();
    assert_eq!(article.subject_scope, IdentityScope::Article);
    assert_eq!(article.yenc_scope, IdentityScope::Article);
    assert_eq!(article.from_scope, IdentityScope::Article);
    assert_eq!(article.subject_yenc_relation, NameRelation::Independent);
    assert!(!article.publish_par2_index && !article.allow_file_counter);
}

#[test]
fn file_counter_can_be_disabled_but_not_enabled_in_private_modes() {
    let forced_off = Config::resolve(
        minimal_file(),
        Overrides {
            obfuscate: Some(ObfuscateMode::FullShared),
            file_counter: Some(false),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!forced_off.file_counter);

    let err = Config::resolve(
        minimal_file(),
        Overrides {
            obfuscate: Some(ObfuscateMode::Full),
            file_counter: Some(true),
            ..Default::default()
        },
    )
    .unwrap_err();
    assert!(err.to_string().contains("contradicts --obfuscate=full"));
}

#[test]
fn obfuscation_no_longer_enables_random_date_implicitly() {
    for mode in [
        ObfuscateMode::Light,
        ObfuscateMode::FullShared,
        ObfuscateMode::Full,
        ObfuscateMode::Article,
    ] {
        let cfg = Config::resolve(
            minimal_file(),
            Overrides {
                obfuscate: Some(mode),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(cfg.date.is_none(), "mode {mode:?} unexpectedly set Date");
    }
}

#[test]
fn cli_overrides_obfuscate_and_par2() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            obfuscate: Some(ObfuscateMode::Full),
            par2: Some(25),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.obfuscate, ObfuscateMode::Full);
    assert_eq!(cfg.par2, 25);
}
