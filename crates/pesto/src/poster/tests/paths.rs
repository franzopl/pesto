use super::*;

// ── target_label ──────────────────────────────────────────────────────────

fn test_server(host: &str) -> crate::config::ServerEntry {
    crate::config::ServerEntry {
        host: host.to_string(),
        port: 563,
        ssl: true,
        connections: 50,
        username: None,
        password: None,
        retry_delay: 1,
        timeout: 60,
        proxy: None,
    }
}

#[test]
fn target_label_single_server_shows_host_and_port() {
    let servers = vec![test_server("news.example.com")];
    assert_eq!(target_label(&servers, 50), "news.example.com:563");
}

#[test]
fn target_label_two_servers_lists_both_hosts() {
    let servers = vec![
        test_server("usnews.blocknews.net"),
        test_server("news.newshosting.com"),
    ];
    assert_eq!(
        target_label(&servers, 100),
        "usnews.blocknews.net + news.newshosting.com"
    );
}

#[test]
fn target_label_many_servers_falls_back_to_a_count() {
    let servers = vec![
        test_server("a.example.com"),
        test_server("b.example.com"),
        test_server("c.example.com"),
        test_server("d.example.com"),
    ];
    assert_eq!(target_label(&servers, 200), "4 servers (200 conn)");
}
