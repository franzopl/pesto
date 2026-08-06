use parmesan::SimdPath;
use pesto::config::Config;
use std::path::{Path, PathBuf};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging with DEBUG level
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .with_writer(std::io::stderr)
        .init();

    // Use the test files from /tmp/season_test
    let test_dir = Path::new("/tmp/season_test");
    let mut episode_paths: Vec<PathBuf> = (1..=15)
        .map(|i| test_dir.join(format!("episode_{:02}.mkv", i)))
        .collect();

    // Filter to only existing files
    episode_paths.retain(|p| p.exists());

    if episode_paths.is_empty() {
        println!("No test files found in {}", test_dir.display());
        return Ok(());
    }

    println!("Found {} episodes", episode_paths.len());
    let total_size: u64 = episode_paths
        .iter()
        .map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0))
        .sum();
    println!(
        "Total size: {} bytes ({:.2} GB)",
        total_size,
        total_size as f64 / 1e9
    );

    for (i, path) in episode_paths.iter().enumerate() {
        let size = std::fs::metadata(path)?.len();
        println!(
            "  Episode {:2}: {} bytes ({:.2} MB)",
            i + 1,
            size,
            size as f64 / 1e6
        );
    }

    // Create a minimal Config for testing
    let config = Config {
        host: "localhost".to_string(),
        port: 119,
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        retry_delay: 0,
        timeout: 60,
        extra_servers: vec![],
        from: "test@example.com".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 768000,
        line_length: 128,
        retries: 0,
        obfuscate: pesto::config::ObfuscateMode::None,
        date: None,
        no_archive: false,
        file_counter: false,
        message_id_domain: None,
        dry_run: true,
        par2: 10,
        par2_memory_limit: None,
        memory_limit: None,
        par2_temp_dir: None,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_only: false,
        par2_before_upload: false,
        threads: 4,
        simd: SimdPath::Auto,
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_temp_dir: None,
        compress_password: None,
        compress_volume_size: None,
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        nzb_tags: vec![],
        tmdb_id: None,
        tmdb_kind: None,
        imdb_id: None,
        tvdb_id: None,
        mal_id: None,
        indexer_url: None,
        indexer_api_key: None,
        nzb_dir: None,
        history: false,
        history_dir: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        pre_hooks: vec![],
        post_hooks: vec![],
        no_hooks: true,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 0,
        check_retries: 0,
        check_connections: 1,
        check_post_retries: 0,
        allow_incomplete_nzb: false,
        check_recover_percent: 0,
        check_recover_max: 0,
        pipeline_depth: 0,
        keepalive_interval: 0,
    };
    println!("\nConfig: par2={}%", config.par2);

    // Call generate_season_par2 directly
    println!("\nGenerating season PAR2...");
    match pesto::poster::generate_season_par2(&episode_paths, &config).await {
        Ok(recovery_slices) => {
            println!(
                "✓ Success! Generated {} recovery slices",
                recovery_slices.len()
            );
        }
        Err(e) => {
            println!("✗ Error: {}", e);
            eprintln!("Error details: {:?}", e);
            return Err(e);
        }
    }

    Ok(())
}
