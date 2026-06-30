//! Post-upload hook execution.
//!
//! Runs each command in `config.post_hooks` via `sh -c`, then runs every
//! executable file in `~/.config/pesto/hooks/` (sorted by name). `no_hooks`
//! suppresses only the directory scripts — `post_hooks` still run regardless.
//!
//! Each hook receives the same environment variables:
//! `PESTO_NAME`, `PESTO_BYTES`, `PESTO_INPUT_PATHS`, `PESTO_SERVER`,
//! `PESTO_GROUP`, `PESTO_GROUPS`, `PESTO_PASSWORD`, `PESTO_NZB`, `PESTO_NFO`,
//! `PESTO_CATEGORY`, `PESTO_NZB_NAME`, `PESTO_OBFUSCATE`, `PESTO_PAR2`,
//! `PESTO_TAGS`.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use tracing::info;

use crate::config::Config;

/// Information passed to each hook via environment variables.
#[derive(Debug, Clone)]
pub struct HookContext {
    pub name: String,
    pub total_bytes: u64,
    pub input_paths: String,
    pub server: String,
    pub group: String,
    pub groups: String,
    pub password: String,
    pub category: String,
    pub nzb_name: String,
    pub obfuscate: String,
    pub par2: u8,
    pub tags: String,
    pub nzb_path: String,
    pub nfo_path: String,
}

/// Run all configured post-upload hooks.
///
/// Returns a list of log lines describing what ran and any errors.
/// This function is synchronous — wrap in `spawn_blocking` when calling
/// from async code.
pub fn run_hooks(config: &Config, ctx: &HookContext) -> Vec<String> {
    let mut logs: Vec<String> = Vec::new();

    for cmd in &config.post_hooks {
        logs.push(format!("Running post-hook: {}", cmd));
        info!(cmd, "post-hook starting");
        let t = Instant::now();
        match run_shell(cmd, ctx) {
            Ok(output) => {
                let elapsed_ms = t.elapsed().as_millis();
                for line in output.lines() {
                    logs.push(format!("  hook> {}", line));
                }
                logs.push("post-hook exited ok".to_string());
                info!(cmd, exit_code = 0, elapsed_ms, "post-hook completed");
            }
            Err(e) => {
                let elapsed_ms = t.elapsed().as_millis();
                info!(cmd, elapsed_ms, error = %e, "post-hook failed");
                logs.push(format!("post-hook error: {}", e));
            }
        }
    }

    if config.no_hooks {
        logs.push("Directory hooks disabled (no_hooks=true)".to_string());
        return logs;
    }

    if let Some(hooks_dir) = crate::config::config_dir().map(|d| d.join("hooks")) {
        if hooks_dir.is_dir() {
            for script in sorted_executables(&hooks_dir) {
                let label = script.display().to_string();
                logs.push(format!("Running hook script: {}", label));
                info!(script = %label, "post-hook starting");
                let t = Instant::now();
                match run_script(&script, ctx) {
                    Ok(output) => {
                        let elapsed_ms = t.elapsed().as_millis();
                        for line in output.lines() {
                            logs.push(format!("  hook> {}", line));
                        }
                        logs.push(format!("{}: exited ok", label));
                        info!(script = %label, exit_code = 0, elapsed_ms, "post-hook completed");
                    }
                    Err(e) => {
                        let elapsed_ms = t.elapsed().as_millis();
                        info!(script = %label, elapsed_ms, error = %e, "post-hook failed");
                        logs.push(format!("{}: error: {}", label, e));
                    }
                }
            }
        }
    }

    if logs.is_empty() {
        logs.push("No post-upload hooks configured".to_string());
    }

    logs
}

/// List the executable hook scripts in `~/.config/pesto/hooks/`, sorted by name.
///
/// Exposed so a front-end (e.g. upapasta) can let the user pick a single hook
/// to run manually instead of running them all.
pub fn list_hook_scripts() -> Vec<PathBuf> {
    crate::config::config_dir()
        .map(|d| d.join("hooks"))
        .filter(|d| d.is_dir())
        .map(|d| sorted_executables(&d))
        .unwrap_or_default()
}

/// Run a single hook script (one returned by [`list_hook_scripts`]) and return
/// `(success, log_lines)`. `success` is true only when the script exited 0.
/// Unlike [`run_hooks`] this ignores `no_hooks` and `post_hook`: the caller
/// chose exactly this script, so only it runs.
pub fn run_one_hook(path: &Path, ctx: &HookContext) -> (bool, Vec<String>) {
    let label = path.display().to_string();
    let mut logs = vec![format!("Running hook script: {label}")];
    info!(script = %label, "post-hook starting");
    let t = Instant::now();
    match run_script(path, ctx) {
        Ok(output) => {
            let elapsed_ms = t.elapsed().as_millis();
            for line in output.lines() {
                logs.push(format!("  hook> {line}"));
            }
            logs.push(format!("{label}: exited ok"));
            info!(script = %label, exit_code = 0, elapsed_ms, "post-hook completed");
            (true, logs)
        }
        Err(e) => {
            let elapsed_ms = t.elapsed().as_millis();
            info!(script = %label, elapsed_ms, error = %e, "post-hook failed");
            logs.push(format!("{label}: error: {e}"));
            (false, logs)
        }
    }
}

fn apply_env(cmd: &mut Command, ctx: &HookContext) {
    cmd.env("PESTO_NAME", &ctx.name)
        .env("PESTO_BYTES", ctx.total_bytes.to_string())
        .env("PESTO_INPUT_PATHS", &ctx.input_paths)
        .env("PESTO_SERVER", &ctx.server)
        .env("PESTO_GROUP", &ctx.group)
        .env("PESTO_GROUPS", &ctx.groups)
        .env("PESTO_PASSWORD", &ctx.password)
        .env("PESTO_CATEGORY", &ctx.category)
        .env("PESTO_NZB_NAME", &ctx.nzb_name)
        .env("PESTO_OBFUSCATE", &ctx.obfuscate)
        .env("PESTO_PAR2", ctx.par2.to_string())
        .env("PESTO_TAGS", &ctx.tags)
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
