use super::minimal_file;
use crate::config::*;

#[test]
fn cli_overrides_article_size_and_retries() {
    let mut file = FileConfig::default();
    file.server.host = Some("h".into());
    file.posting.groups = Some(vec!["a.b".into()]);
    file.posting.article_size = Some(500_000);
    file.posting.retries = Some(2);

    let cli = Overrides {
        article_size: Some(999_000),
        retries: Some(5),
        ..Default::default()
    };

    let cfg = Config::resolve(file, cli).unwrap();
    assert_eq!(cfg.article_size, 999_000);
    assert_eq!(cfg.retries, 5);
}

#[test]
fn cli_overrides_ssl_and_connections() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            ssl: Some(false),
            connections: Some(16),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!cfg.ssl);
    assert_eq!(cfg.connections, 16);
}

#[test]
fn cli_overrides_username_and_password() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            username: Some("alice".into()),
            password: Some("hunter2".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.username.as_deref(), Some("alice"));
    assert_eq!(cfg.password.as_deref(), Some("hunter2"));
}

#[test]
fn cli_overrides_line_length_and_retry_delay() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            line_length: Some(64),
            retry_delay: Some(10),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.line_length, 64);
    assert_eq!(cfg.retry_delay, 10);
}

#[test]
fn cli_overrides_check_resume_no_archive() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            check: Some(false),
            resume: Some(true),
            no_archive: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!cfg.check);
    assert!(cfg.resume);
    assert!(cfg.no_archive);
}

#[test]
fn cli_overrides_date_and_message_id_domain() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            date: Some("random".into()),
            message_id_domain: Some("example.net".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.date.as_deref(), Some("random"));
    assert_eq!(cfg.message_id_domain.as_deref(), Some("example.net"));
}

#[test]
fn cli_overrides_from_and_groups() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            from: Some("Bot <bot@x>".into()),
            groups: Some(vec!["alt.binaries.test".into(), "alt.test".into()]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.from, "Bot <bot@x>");
    assert_eq!(cfg.groups, vec!["alt.binaries.test", "alt.test"]);
}

#[test]
fn cli_overrides_upload_rate() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            upload_rate: Some(5 * 1024 * 1024),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.upload_rate, 5 * 1024 * 1024);
}

#[test]
fn cli_overrides_compress_format_and_password() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            compress_format: Some("zip".into()),
            compress_password: Some("pass123".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.compress_format.as_deref(), Some("zip"));
    assert_eq!(cfg.compress_password.as_deref(), Some("pass123"));
}

#[test]
fn cli_overrides_nzb_metadata() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            nzb_title: Some("My Show S01".into()),
            nzb_password: Some("abc".into()),
            nzb_category: Some("TV".into()),
            nzb_tags: vec!["hd".into(), "2024".into()],
            nzb_dir: Some("/out".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.nzb_title.as_deref(), Some("My Show S01"));
    assert_eq!(cfg.nzb_password.as_deref(), Some("abc"));
    assert_eq!(cfg.nzb_category.as_deref(), Some("TV"));
    assert_eq!(cfg.nzb_tags, vec!["hd", "2024"]);
    assert_eq!(cfg.nzb_dir.as_deref(), Some("/out"));
}

#[test]
fn cli_overrides_history_and_nfo_and_post_hook() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            history: Some(false),
            nfo: Some(true),
            post_hooks: vec!["notify.sh".into()],
            ..Default::default()
        },
    )
    .unwrap();
    assert!(!cfg.history);
    assert!(cfg.nfo);
    assert_eq!(cfg.post_hooks, vec!["notify.sh"]);
}

#[test]
fn cli_upload_rate_wins_over_file_upload_rate() {
    let file: FileConfig = toml::from_str(
        "[server]\nhost=\"h\"\n[posting]\ngroups=[\"a\"]\nupload_rate=\"100 MiB/s\"\n",
    )
    .unwrap();
    let cfg = Config::resolve(
        file,
        Overrides {
            upload_rate: Some(1024),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.upload_rate, 1024);
}

#[test]
fn file_upload_rate_used_when_cli_absent() {
    let file: FileConfig = toml::from_str(
        "[server]\nhost=\"h\"\n[posting]\ngroups=[\"a\"]\nupload_rate=\"1 KiB/s\"\n",
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.upload_rate, 1024);
}
