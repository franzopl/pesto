use super::base_overrides;
use crate::config::*;

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
