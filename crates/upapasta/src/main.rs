use anyhow::Result;

#[tokio::main]
async fn main() -> Result<()> {
    println!("UpaPasta v2 (Rust)");
    println!("====================================");
    println!();
    println!("Monorepo structure successfully created on branch 'upapasta-v2'.");
    println!();
    println!("Current crates:");
    println!("  • pesto      → Core library + lightweight CLI");
    println!("  • upapasta   → Full TUI application (this binary)");
    println!("  • parmesan   → High-performance PAR2 engine");
    println!();
    println!("Next steps:");
    println!("  1. Build rich ratatui TUI (file tree + catalog + queue)");
    println!("  2. Integrate directly with pesto::post() API");
    println!("  3. Port catalog/history from Python version");
    println!("  4. Add watch mode and metadata features");
    println!();
    println!("The Python version can now be gradually retired.");
    println!();
    println!("Press Ctrl+C to exit.");

    tokio::signal::ctrl_c().await?;
    println!("\nExiting upapasta v2.");

    Ok(())
}
