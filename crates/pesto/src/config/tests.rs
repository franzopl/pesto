use super::*;
use std::io::Write;
use tempfile::NamedTempFile;

fn base_overrides() -> Overrides {
    Overrides {
        groups: Some(vec!["alt.test".into()]),
        ..Default::default()
    }
}

fn minimal_file() -> FileConfig {
    let mut f = FileConfig::default();
    f.server.host = Some("h".into());
    f.posting.groups = Some(vec!["alt.test".into()]);
    f
}

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
fn obfuscate_mode_parses_from_toml_and_defaults_to_none() {
    let file: FileConfig = toml::from_str("[posting]\nobfuscate = \"full\"\n").unwrap();
    assert_eq!(file.posting.obfuscate, Some(ObfuscateMode::Full));
    assert_eq!(ObfuscateMode::default(), ObfuscateMode::None);
}

#[test]
fn parse_rate_bare_bytes() {
    assert_eq!(parse_upload_rate("1024").unwrap(), 1024);
}

#[test]
fn parse_rate_kib() {
    assert_eq!(parse_upload_rate("10 KiB/s").unwrap(), 10 * 1024);
}

#[test]
fn parse_rate_mib() {
    assert_eq!(parse_upload_rate("50 MiB/s").unwrap(), 50 * 1024 * 1024);
}

#[test]
fn parse_rate_mb_case_insensitive() {
    assert_eq!(parse_upload_rate("2 MB/s").unwrap(), 2 * 1024 * 1024);
}

#[test]
fn parse_rate_gib() {
    assert_eq!(parse_upload_rate("1 GiB/s").unwrap(), 1024 * 1024 * 1024);
}

#[test]
fn parse_rate_unknown_unit_errors() {
    assert!(parse_upload_rate("10 TiB/s").is_err());
}

#[test]
fn parse_rate_not_a_number_errors() {
    assert!(parse_upload_rate("fast").is_err());
}

#[test]
fn single_servers_entry_becomes_primary() {
    let file: FileConfig = toml::from_str(
        r#"
        [[servers]]
        host = "news.example.com"
        port = 119
        ssl = false
        connections = 8
        "#,
    )
    .unwrap();

    let cfg = Config::resolve(file, base_overrides()).unwrap();
    assert_eq!(cfg.host, "news.example.com");
    assert_eq!(cfg.port, 119);
    assert!(!cfg.ssl);
    assert_eq!(cfg.connections, 8);
    assert!(cfg.extra_servers.is_empty());
}

#[test]
fn multiple_servers_first_is_primary_rest_are_extra() {
    let file: FileConfig = toml::from_str(
        r#"
        [[servers]]
        host = "primary.example.com"
        [[servers]]
        host = "backup.example.com"
        connections = 2
        "#,
    )
    .unwrap();

    let cfg = Config::resolve(file, base_overrides()).unwrap();
    assert_eq!(cfg.host, "primary.example.com");
    assert_eq!(cfg.extra_servers.len(), 1);
    assert_eq!(cfg.extra_servers[0].host, "backup.example.com");
    assert_eq!(cfg.extra_servers[0].connections, 2);
}

#[test]
fn servers_entry_missing_host_errors() {
    let file: FileConfig = toml::from_str(
        r#"
        [[servers]]
        port = 119
        "#,
    )
    .unwrap();

    assert!(Config::resolve(file, base_overrides()).is_err());
}

#[test]
fn extra_server_missing_host_errors() {
    let file: FileConfig = toml::from_str(
        r#"
        [[servers]]
        host = "primary.example.com"
        [[servers]]
        port = 119
        "#,
    )
    .unwrap();

    assert!(Config::resolve(file, base_overrides()).is_err());
}

#[test]
fn total_connections_sums_all_servers() {
    let file: FileConfig = toml::from_str(
        r#"
        [[servers]]
        host = "a.example.com"
        connections = 4
        [[servers]]
        host = "b.example.com"
        connections = 2
        "#,
    )
    .unwrap();

    let cfg = Config::resolve(file, base_overrides()).unwrap();
    assert_eq!(cfg.total_connections(), 6);
}

#[test]
fn missing_host_error_mentions_host() {
    let mut file = FileConfig::default();
    file.posting.groups = Some(vec!["alt.test".into()]);
    let err = Config::resolve(file, Overrides::default()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("host"), "expected 'host' in error: {msg}");
}

#[test]
fn missing_groups_error_mentions_groups() {
    let mut file = FileConfig::default();
    file.server.host = Some("h".into());
    let err = Config::resolve(file, Overrides::default()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("groups"), "expected 'groups' in error: {msg}");
}

#[test]
fn extra_server_missing_host_error_is_actionable() {
    let file: FileConfig =
        toml::from_str("[[servers]]\nhost = \"primary\"\n[[servers]]\nport = 119\n").unwrap();
    let err = Config::resolve(file, base_overrides()).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("host"), "expected 'host' in error: {msg}");
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
fn timeout_resolves_from_server_section_and_propagates_to_all_servers() {
    let file: FileConfig = toml::from_str(
        "[[servers]]\nhost = \"primary\"\ntimeout = 45\n[[servers]]\nhost = \"backup\"\n",
    )
    .unwrap();
    let cfg = Config::resolve(file, base_overrides()).unwrap();
    assert_eq!(cfg.timeout, 45);
    // The primary's timeout is surfaced through all_servers()...
    let servers: Vec<_> = cfg.all_servers().collect();
    assert_eq!(servers[0].timeout, 45);
    // ...and an entry without its own timeout inherits the primary's.
    assert_eq!(servers[1].timeout, 45);
}

#[test]
fn all_boolean_defaults_are_correct() {
    let cfg = Config::resolve(minimal_file(), Overrides::default()).unwrap();
    assert!(cfg.ssl, "ssl should default to true");
    assert!(!cfg.dry_run);
    assert!(!cfg.par2_only);
    assert!(!cfg.verify);
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
    assert!(cfg.nzb_name.is_none());
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
fn toml_server_section_is_parsed() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "news.example.com"
        port = 119
        ssl = false
        connections = 8
        retry_delay = 5
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(
        file,
        Overrides {
            groups: Some(vec!["alt.test".into()]),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.host, "news.example.com");
    assert_eq!(cfg.port, 119);
    assert!(!cfg.ssl);
    assert_eq!(cfg.connections, 8);
    assert_eq!(cfg.retry_delay, 5);
}

#[test]
fn toml_auth_section_sets_credentials() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [auth]
        username = "alice"
        password = "s3cr3t"
        [posting]
        groups = ["alt.test"]
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.username.as_deref(), Some("alice"));
    assert_eq!(cfg.password.as_deref(), Some("s3cr3t"));
}

#[test]
fn toml_posting_section_sets_all_fields() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [posting]
        groups = ["alt.test"]
        from = "Bot <bot@example.com>"
        article_size = 500000
        line_length = 64
        retries = 5
        par2 = 20
        verify = true
        obfuscate = "full"
        date = "now"
        no_archive = true
        message_id_domain = "example.com"
        upload_rate = "10 MiB/s"
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.from, "Bot <bot@example.com>");
    assert_eq!(cfg.article_size, 500_000);
    assert_eq!(cfg.line_length, 64);
    assert_eq!(cfg.retries, 5);
    assert_eq!(cfg.par2, 20);
    assert!(cfg.verify);
    assert_eq!(cfg.obfuscate, ObfuscateMode::Full);
    assert_eq!(cfg.date.as_deref(), Some("now"));
    assert!(cfg.no_archive);
    assert_eq!(cfg.message_id_domain.as_deref(), Some("example.com"));
    assert_eq!(cfg.upload_rate, 10 * 1024 * 1024);
}

#[test]
fn toml_output_section_sets_fields() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [posting]
        groups = ["alt.test"]
        [output]
        nzb_name = "My Release"
        nzb_category = "TV > HD"
        nzb_tags = ["hd", "2024", "dts"]
        nzb_dir = "/tmp/nzb"
        history = false
        resume = true
        post_hook = "notify.sh"
        nfo = true
        no_hooks = true
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.nzb_name.as_deref(), Some("My Release"));
    assert_eq!(cfg.nzb_category.as_deref(), Some("TV > HD"));
    assert_eq!(cfg.nzb_tags, vec!["hd", "2024", "dts"]);
    assert_eq!(cfg.nzb_dir.as_deref(), Some("/tmp/nzb"));
    assert!(!cfg.history);
    assert!(cfg.resume);
    assert_eq!(cfg.post_hooks, vec!["notify.sh"]);
    assert!(cfg.nfo);
    assert!(cfg.no_hooks, "output.no_hooks = true should be honored");
}

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

#[test]
fn toml_compression_section_sets_format() {
    let file: FileConfig = toml::from_str(
        "[server]\nhost = \"h\"\n[posting]\ngroups = [\"a\"]\n[compression]\nformat = \"rar\"\n",
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.compress_format.as_deref(), Some("rar"));
}

#[test]
fn toml_notify_section_sets_webhook_and_ntfy() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [posting]
        groups = ["alt.test"]
        [notify]
        webhook_url = "https://discord.com/api/webhooks/x"
        ntfy_topic = "my-alerts"
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(
        cfg.notify_webhook.as_deref(),
        Some("https://discord.com/api/webhooks/x")
    );
    assert_eq!(cfg.notify_ntfy.as_deref(), Some("my-alerts"));
}

#[test]
fn toml_unknown_field_is_rejected() {
    let result: Result<FileConfig, _> =
        toml::from_str("[server]\nhost = \"h\"\nunknown_key = true\n");
    assert!(
        result.is_err(),
        "deny_unknown_fields should reject unknown keys"
    );
}

#[test]
fn file_config_load_from_disk() {
    let mut f = NamedTempFile::new().unwrap();
    write!(
        f,
        "[server]\nhost = \"disk-host\"\n[posting]\ngroups = [\"a\"]\n"
    )
    .unwrap();
    let loaded = FileConfig::load(f.path()).unwrap();
    assert_eq!(loaded.server.host.as_deref(), Some("disk-host"));
}

#[test]
fn file_config_load_missing_file_errors() {
    let err = FileConfig::load(std::path::Path::new("/no/such/file.toml")).unwrap_err();
    let msg = format!("{err:#}");
    assert!(msg.contains("reading config file"));
}

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

#[test]
fn cli_overrides_verify_resume_no_archive() {
    let cfg = Config::resolve(
        minimal_file(),
        Overrides {
            verify: Some(true),
            resume: Some(true),
            no_archive: Some(true),
            ..Default::default()
        },
    )
    .unwrap();
    assert!(cfg.verify);
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
            nzb_name: Some("My Show S01".into()),
            nzb_password: Some("abc".into()),
            nzb_category: Some("TV".into()),
            nzb_tags: vec!["hd".into(), "2024".into()],
            nzb_dir: Some("/out".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.nzb_name.as_deref(), Some("My Show S01"));
    assert_eq!(cfg.nzb_password.as_deref(), Some("abc"));
    assert_eq!(cfg.nzb_category.as_deref(), Some("TV"));
    assert_eq!(cfg.nzb_tags, vec!["hd", "2024"]);
    assert_eq!(cfg.nzb_dir.as_deref(), Some("/out"));
}

#[test]
fn cli_nzb_tags_replace_file_tags() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [posting]
        groups = ["alt.test"]
        [output]
        nzb_tags = ["file-a", "file-b"]
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(
        file,
        Overrides {
            nzb_tags: vec!["cli-only".into()],
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.nzb_tags, vec!["cli-only"]);
}

#[test]
fn file_nzb_tags_used_when_cli_absent() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [posting]
        groups = ["alt.test"]
        [output]
        nzb_tags = ["config-a", "config-b"]
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.nzb_tags, vec!["config-a", "config-b"]);
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
fn config_check_off_by_default() {
    let file: FileConfig =
        toml::from_str("[server]\nhost=\"h\"\n[posting]\ngroups=[\"a\"]\n").unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert!(!cfg.check);
    assert_eq!(cfg.check_delay_secs, 30);
}

#[test]
fn config_explicit_check_true_uses_default_delay() {
    let file: FileConfig =
        toml::from_str("[server]\nhost=\"h\"\n[posting]\ngroups=[\"a\"]\ncheck=true\n").unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert!(cfg.check);
    assert_eq!(cfg.check_delay_secs, 30);
}
