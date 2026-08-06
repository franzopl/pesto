use std::path::{Path, PathBuf};
use pesto::Config;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    // Use the test files from /tmp/season_test
    let test_dir = Path::new("/tmp/season_test");
    let mut episode_paths: Vec<PathBuf> = (1..=15)
        .map(|i| test_dir.join(format!("episode_{:02}.mkv", i)))
        .collect();

    // Filter to only existing files
    episode_paths.retain(|p| p.exists());

    println!("Found {} episodes", episode_paths.len());
    for (i, path) in episode_paths.iter().enumerate() {
        let size = std::fs::metadata(path)?.len();
        println!("  Episode {}: {} bytes", i + 1, size);
    }

    // Create a test config
    let config = Config::default();

    // Call generate_season_par2 directly
    println!("\nGenerating season PAR2...");
    match pesto::poster::generate_season_par2(&episode_paths, &config).await {
        Ok(recovery_slices) => {
            println!("✓ Success! Generated {} recovery slices", recovery_slices.len());
        }
        Err(e) => {
            println!("✗ Error: {}", e);
            return Err(e);
        }
    }

    Ok(())
}
