//! Post-upload hook execution for upapasta.
//!
//! Mirrors the behaviour of the `pesto` CLI:
//!   1. Runs `config.post_hook` via `sh -c` (if set and `no_hooks` is false).
//!   2. Runs every executable file in `~/.config/pesto/hooks/` (sorted by name).
//!
//! Each hook receives the same environment variables as the `pesto` CLI:
//! `PESTO_NAME`, `PESTO_BYTES`, `PESTO_SERVER`, `PESTO_GROUP`,
//! `PESTO_PASSWORD`, `PESTO_NZB`.

use std::path::{Path, PathBuf};
use std::process::Command;

use pesto::config::Config;

/// Information passed to each hook via environment variables.
#[derive(Debug, Clone)]
pub struct HookContext {
    /// Display name for the uploaded content (first file, or a combined label).
    pub name: String,
    /// Total bytes uploaded.
    pub total_bytes: u64,
    /// Primary NNTP server hostname.
    pub server: String,
    /// Primary newsgroup.
    pub group: String,
    /// Extraction / archive password (nzb_password or compress_password).
    pub password: String,
    /// Path to the generated `.nzb` file, if any.
    pub nzb_path: String,
    /// Path to the `.nfo` file, if any.
    pub nfo_path: String,
}

impl HookContext {
    /// Build a context from the effective upload config and queue file names.
    pub fn from_config(
        config: &Config,
        queue_names: &[String],
        total_bytes: u64,
        nzb_path: Option<&str>,
    ) -> Self {
        let name = if queue_names.len() == 1 {
            queue_names[0].clone()
        } else if queue_names.is_empty() {
            "unknown".to_string()
        } else {
            format!("{} (+{})", queue_names[0], queue_names.len() - 1)
        };
        Self {
            name,
            total_bytes,
            server: config.host.clone(),
            group: config.groups.first().cloned().unwrap_or_default(),
            password: config
                .nzb_password
                .as_deref()
                .or(config.compress_password.as_deref())
                .unwrap_or("")
                .to_string(),
            nzb_path: nzb_path.unwrap_or("").to_string(),
            nfo_path: String::new(),
        }
    }
}

/// Run all configured post-upload hooks.
///
/// Returns a list of log lines describing what ran and any errors.
/// This function is synchronous — wrap in `spawn_blocking` when calling
/// from async code.
pub fn run_hooks(config: &Config, ctx: &HookContext) -> Vec<String> {
    if config.no_hooks {
        return vec!["Hooks disabled (no_hooks=true)".to_string()];
    }

    let mut logs: Vec<String> = Vec::new();

    // 1. --post-hook / config.post_hook
    if let Some(ref cmd) = config.post_hook {
        logs.push(format!("Running post-hook: {}", cmd));
        match run_shell(cmd, ctx) {
            Ok(output) => {
                for line in output.lines() {
                    logs.push(format!("  hook> {}", line));
                }
                logs.push("post-hook exited ok".to_string());
            }
            Err(e) => logs.push(format!("post-hook error: {}", e)),
        }
    }

    // 2. ~/.config/pesto/hooks/ directory
    if let Some(hooks_dir) = pesto::config::config_dir().map(|d| d.join("hooks")) {
        if hooks_dir.is_dir() {
            for script in sorted_executables(&hooks_dir) {
                let label = script.display().to_string();
                logs.push(format!("Running hook script: {}", label));
                match run_script(&script, ctx) {
                    Ok(output) => {
                        for line in output.lines() {
                            logs.push(format!("  hook> {}", line));
                        }
                        logs.push(format!("{}: exited ok", label));
                    }
                    Err(e) => logs.push(format!("{}: error: {}", label, e)),
                }
            }
        }
    }

    if logs.is_empty() {
        logs.push("No post-upload hooks configured".to_string());
    }

    logs
}

fn apply_env(cmd: &mut Command, ctx: &HookContext) {
    cmd.env("PESTO_NAME", &ctx.name)
        .env("PESTO_BYTES", ctx.total_bytes.to_string())
        .env("PESTO_SERVER", &ctx.server)
        .env("PESTO_GROUP", &ctx.group)
        .env("PESTO_PASSWORD", &ctx.password)
        .env("PESTO_NZB", &ctx.nzb_path)
        .env("PESTO_NFO", &ctx.nfo_path);
}

#[cfg(unix)]
fn run_shell(cmd: &str, ctx: &HookContext) -> Result<String, String> {
    let mut c = Command::new("sh");
    c.args(["-c", cmd]);
    apply_env(&mut c, ctx);
    run_command(c)
}

#[cfg(windows)]
fn run_shell(cmd: &str, ctx: &HookContext) -> Result<String, String> {
    let mut c = Command::new("cmd");
    c.args(["/c", cmd]);
    apply_env(&mut c, ctx);
    run_command(c)
}

fn run_script(path: &Path, ctx: &HookContext) -> Result<String, String> {
    let mut c = Command::new(path);
    apply_env(&mut c, ctx);
    run_command(c)
}

fn run_command(mut cmd: Command) -> Result<String, String> {
    let output = cmd
        .output()
        .map_err(|e| format!("failed to start: {}", e))?;
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let combined = if stderr.is_empty() {
        stdout
    } else if stdout.is_empty() {
        stderr
    } else {
        format!("{}{}", stdout, stderr)
    };
    if output.status.success() {
        Ok(combined)
    } else {
        Err(format!(
            "exited with {}: {}",
            output.status,
            combined.trim()
        ))
    }
}

fn sorted_executables(dir: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return vec![];
    };
    let mut scripts: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_executable(p))
        .collect();
    scripts.sort();
    scripts
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|e| e.to_str()),
        Some("exe" | "cmd" | "bat" | "ps1")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_ctx() -> HookContext {
        HookContext {
            name: "test.mkv".to_string(),
            total_bytes: 1_000_000,
            server: "news.example.com".to_string(),
            group: "alt.binaries.test".to_string(),
            password: "".to_string(),
            nzb_path: "".to_string(),
            nfo_path: "".to_string(),
        }
    }

    #[test]
    fn no_hooks_returns_disabled_message() {
        use pesto::config::ObfuscateMode;
        let mut cfg = pesto::config::Config {
            host: "h".into(),
            port: 563,
            ssl: true,
            connections: 1,
            username: None,
            password: None,
            retry_delay: 1,
            extra_servers: vec![],
            from: "f".into(),
            groups: vec!["alt.test".into()],
            article_size: 768_000,
            line_length: 128,
            retries: 1,
            obfuscate: ObfuscateMode::None,
            date: None,
            no_archive: false,
            message_id_domain: None,
            dry_run: true,
            par2: 0,
            par2_memory_limit: None,
            par2_slice_size: None,
            par2_slice_count: None,
            par2_recovery_count: None,
            par2_only: false,
            threads: 1,
            simd: parmesan::SimdPath::Auto,
            verify: false,
            resume: false,
            upload_rate: 0,
            compress_format: None,
            compress_password: None,
            nzb_name: None,
            nzb_password: None,
            nzb_category: None,
            indexer_url: None,
            indexer_api_key: None,
            indexer_category: None,
            nzb_dir: None,
            no_upload: false,
            history: false,
            history_dir: None,
            notify_webhook: None,
            notify_ntfy: None,
            notify: None,
            post_hook: None,
            no_hooks: true,
            nfo: false,
            quiet: false,
            bell: false,
            check: false,
            check_delay_secs: 0,
            check_retries: 1,
            pipeline_depth: 0,
        };
        cfg.no_hooks = true;
        let logs = run_hooks(&cfg, &dummy_ctx());
        assert_eq!(logs.len(), 1);
        assert!(logs[0].contains("disabled"));
    }

    #[test]
    fn from_config_single_file() {
        use pesto::config::ObfuscateMode;
        let cfg = pesto::config::Config {
            host: "news.example.com".into(),
            port: 563,
            ssl: true,
            connections: 4,
            username: None,
            password: None,
            retry_delay: 1,
            extra_servers: vec![],
            from: "f".into(),
            groups: vec!["alt.binaries.test".into()],
            article_size: 768_000,
            line_length: 128,
            retries: 1,
            obfuscate: ObfuscateMode::None,
            date: None,
            no_archive: false,
            message_id_domain: None,
            dry_run: false,
            par2: 0,
            par2_memory_limit: None,
            par2_slice_size: None,
            par2_slice_count: None,
            par2_recovery_count: None,
            par2_only: false,
            threads: 1,
            simd: parmesan::SimdPath::Auto,
            verify: false,
            resume: false,
            upload_rate: 0,
            compress_format: None,
            compress_password: None,
            nzb_name: None,
            nzb_password: None,
            nzb_category: None,
            indexer_url: None,
            indexer_api_key: None,
            indexer_category: None,
            nzb_dir: None,
            no_upload: false,
            history: false,
            history_dir: None,
            notify_webhook: None,
            notify_ntfy: None,
            notify: None,
            post_hook: None,
            no_hooks: false,
            nfo: false,
            quiet: false,
            bell: false,
            check: false,
            check_delay_secs: 0,
            check_retries: 1,
            pipeline_depth: 0,
        };
        let ctx = HookContext::from_config(&cfg, &["movie.mkv".to_string()], 500_000, None);
        assert_eq!(ctx.name, "movie.mkv");
        assert_eq!(ctx.server, "news.example.com");
        assert_eq!(ctx.group, "alt.binaries.test");
        assert_eq!(ctx.total_bytes, 500_000);
        assert_eq!(ctx.nzb_path, "");
    }
}
