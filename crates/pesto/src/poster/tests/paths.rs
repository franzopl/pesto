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

// ── normalize_client_path ─────────────────────────────────────────────────

#[test]
fn client_path_strips_one_common_release_root() {
    assert_eq!(
        normalize_client_path("Release/Season01/ep01.mkv", Some("Release")).unwrap(),
        "Season01/ep01.mkv"
    );
}

#[test]
fn client_path_preserves_distinct_top_level_roots() {
    assert_eq!(
        normalize_client_path("ShowA/s01/ep01.mkv", None).unwrap(),
        "ShowA/s01/ep01.mkv"
    );
}

#[test]
fn client_path_keeps_loose_file_unchanged() {
    assert_eq!(
        normalize_client_path("movie.mkv", None).unwrap(),
        "movie.mkv"
    );
}

#[test]
fn client_path_rejects_unsafe_components_and_separators() {
    for name in [
        "",
        "/abs.bin",
        "Release/../x",
        "Release//x",
        "a\\b",
        "Árvore/legenda.txt",
    ] {
        assert!(
            normalize_client_path(name, Some("Release")).is_err(),
            "{name}"
        );
    }
}

// ── par2_base ─────────────────────────────────────────────────────────────

#[test]
fn par2_base_single_component() {
    assert_eq!(par2_base("movie.mkv"), "movie.mkv");
}

#[test]
fn par2_base_relative_path_returns_root_folder() {
    assert_eq!(par2_base("Season01/ep01.mkv"), "Season01");
    assert_eq!(par2_base("a/b/c.bin"), "a");
}

#[test]
fn par2_base_empty_string() {
    // Should not panic; returns the whole (empty) string.
    assert_eq!(par2_base(""), "");
}

// ── par2_release_base ────────────────────────────────────────────────────

#[test]
fn par2_release_base_strips_rar_volume_suffix() {
    assert_eq!(
        par2_release_base("archive.part01.rar"),
        "archive",
        "PAR2 set for a volume-split rar archive must not be named after \
         one specific volume"
    );
    assert_eq!(par2_release_base("archive.part1.rar"), "archive");
}

#[test]
fn par2_release_base_strips_sevenzip_volume_suffix() {
    assert_eq!(par2_release_base("archive.7z.001"), "archive");
}

#[test]
fn par2_release_base_leaves_non_volume_names_untouched() {
    assert_eq!(par2_release_base("movie.mkv"), "movie.mkv");
    assert_eq!(par2_release_base("archive.rar"), "archive.rar");
    assert_eq!(par2_release_base("archive.7z"), "archive.7z");
}

#[test]
fn par2_release_base_still_roots_season_packs_at_the_folder() {
    assert_eq!(par2_release_base("Season01/ep01.mkv"), "Season01");
}
