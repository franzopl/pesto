use super::minimal_file;
use crate::config::*;

#[test]
fn cli_overrides_win_over_file() {
    let mut file = FileConfig::default();
    file.server.host = Some("file-host".into());
    file.server.port = Some(119);
    file.posting.from = Some("file <f@x>".into());
    file.posting.groups = Some(vec!["a.b.file".into()]);

    let cli = Overrides {
        host: Some("cli-host".into()),
        ..Default::default()
    };

    let cfg = Config::resolve(file, cli).unwrap();
    assert_eq!(cfg.host, "cli-host");
    assert_eq!(cfg.port, 119);
}

#[test]
fn defaults_apply_when_unset() {
    let mut file = FileConfig::default();
    file.server.host = Some("h".into());
    file.posting.from = Some("f <f@x>".into());
    file.posting.groups = Some(vec!["a.b.c".into()]);

    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.port, DEFAULT_PORT);
    assert_eq!(cfg.connections, DEFAULT_CONNECTIONS);
    assert!(cfg.ssl);
}

#[test]
fn missing_required_field_errors() {
    let cfg = Config::resolve(FileConfig::default(), Overrides::default());
    assert!(cfg.is_err());
}

#[test]
fn all_numeric_defaults_match_constants() {
    let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
    assert_eq!(cfg.port, DEFAULT_PORT);
    assert_eq!(cfg.connections, DEFAULT_CONNECTIONS);
    assert_eq!(cfg.article_size, DEFAULT_ARTICLE_SIZE);
    assert_eq!(cfg.line_length, DEFAULT_LINE_LENGTH);
    assert_eq!(cfg.retries, DEFAULT_RETRIES);
    assert_eq!(cfg.retry_delay, DEFAULT_RETRY_DELAY);
    assert_eq!(cfg.timeout, DEFAULT_TIMEOUT_SECS);
    assert_eq!(cfg.par2, DEFAULT_PAR2);
}

#[test]
fn all_boolean_defaults_are_correct() {
    let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
    assert!(cfg.ssl, "ssl should default to true");
    assert!(!cfg.dry_run);
    assert!(!cfg.par2_only);
    assert!(cfg.check, "check should default to true");
    assert!(!cfg.resume);
    assert!(!cfg.no_archive);
    assert!(cfg.history, "history should default to true");
    assert!(!cfg.nfo);
}

#[test]
fn optional_string_fields_default_to_none() {
    let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
    assert!(cfg.username.is_none());
    assert!(cfg.password.is_none());
    assert!(cfg.compress_format.is_none());
    assert!(cfg.compress_password.is_none());
    assert!(cfg.nzb_title.is_none());
    assert!(cfg.nzb_password.is_none());
    assert!(cfg.nzb_category.is_none());
    assert!(cfg.nzb_tags.is_empty());
    assert!(cfg.nzb_dir.is_none());
    assert!(cfg.date.is_none());
    assert!(cfg.message_id_domain.is_none());
    assert!(cfg.post_hooks.is_empty());
    assert!(cfg.notify_webhook.is_none());
    assert!(cfg.notify_ntfy.is_none());
    assert!(cfg.notify.is_none());
    assert_eq!(cfg.upload_rate, 0);
}

#[test]
fn from_is_generated_randomly_when_not_set() {
    let a = Config::resolve(minimal_file(), Overrides::default())
        .unwrap()
        .from;
    let b = Config::resolve(minimal_file(), Overrides::default())
        .unwrap()
        .from;
    assert_ne!(a, b, "random from should differ between calls");
    assert!(a.contains('@'), "from should be address-shaped");
}

#[test]
fn retries_zero_is_clamped_to_one() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            retries: Some(0),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.retries, 1);
}

#[test]
fn dry_run_does_not_require_host() {
    let mut file = FileConfig::default();
    file.posting.groups = Some(vec!["a.b".into()]);

    let cli = Overrides {
        dry_run: Some(true),
        ..Default::default()
    };

    let cfg = Config::resolve(file, cli).unwrap();
    assert!(cfg.dry_run);
    assert_eq!(cfg.host, "localhost");
}

#[test]
fn par2_only_does_not_require_host_or_groups() {
    let file = FileConfig::default();

    let cli = Overrides {
        par2_only: Some(true),
        ..Default::default()
    };

    let cfg = Config::resolve(file, cli).unwrap();
    assert!(cfg.par2_only);
}

#[test]
fn missing_groups_errors_for_normal_post() {
    let mut file = FileConfig::default();
    file.server.host = Some("h".into());
    assert!(Config::resolve(file, Overrides::default()).is_err());
}
