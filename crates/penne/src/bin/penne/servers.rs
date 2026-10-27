use std::path::PathBuf;

use anyhow::{Context, Result};

/// `penne servers`: list the selected `[[servers]]` entries. Built from an
/// explicit allowlist of fields rather than serializing `RawServer`, so a
/// credential field can never leak into the output.
pub(super) fn run(config_path: Option<PathBuf>, server_names: &[String], json: bool) -> Result<()> {
    let config_path = super::check::config_path_or_default(config_path)?;
    let config_toml = std::fs::read_to_string(&config_path)
        .with_context(|| format!("reading {}", config_path.display()))?;
    let config = penne::config::RawConfig::parse(&config_toml)?.select(server_names)?;

    if json {
        let entries: Vec<serde_json::Value> = config
            .servers
            .iter()
            .map(|s| {
                serde_json::json!({
                    "name": s.name,
                    "host": s.host,
                    "retention_days": s.retention_days,
                })
            })
            .collect();
        println!("{}", serde_json::to_string(&entries)?);
        return Ok(());
    }

    if config.servers.is_empty() {
        println!("no [[servers]] configured in {}", config_path.display());
        return Ok(());
    }
    for s in &config.servers {
        let retention = s.retention_days.map_or_else(
            || "retention not declared".to_string(),
            |d| format!("{d} days retention"),
        );
        match &s.name {
            Some(name) => println!("{} ({name}) — {retention}", s.host),
            None => println!("{} — {retention}", s.host),
        }
    }
    Ok(())
}
