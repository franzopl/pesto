use crate::config::*;

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
        username = "test-user"
        password = "test-password"
        [posting]
        groups = ["alt.test"]
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.username.as_deref(), Some("test-user"));
    assert_eq!(cfg.password.as_deref(), Some("test-password"));
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
        nzb_title = "My Release"
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
    assert_eq!(cfg.nzb_title.as_deref(), Some("My Release"));
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
fn toml_deprecated_nzb_name_still_works() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [posting]
        groups = ["alt.test"]
        [output]
        nzb_name = "My Release"
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.nzb_title.as_deref(), Some("My Release"));
}

#[test]
fn toml_nzb_title_takes_precedence_over_deprecated_nzb_name() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [posting]
        groups = ["alt.test"]
        [output]
        nzb_title = "New Name"
        nzb_name = "Old Name"
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(file, Overrides::default()).unwrap();
    assert_eq!(cfg.nzb_title.as_deref(), Some("New Name"));
}

#[test]
fn cli_nzb_title_takes_precedence_over_toml_deprecated_nzb_name() {
    let file: FileConfig = toml::from_str(
        r#"
        [server]
        host = "h"
        [posting]
        groups = ["alt.test"]
        [output]
        nzb_name = "From File"
        "#,
    )
    .unwrap();
    let cfg = Config::resolve(
        file,
        Overrides {
            nzb_title: Some("From CLI".into()),
            ..Default::default()
        },
    )
    .unwrap();
    assert_eq!(cfg.nzb_title.as_deref(), Some("From CLI"));
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
