//! Post-upload hook execution.
//!
//! Runs each command in `config.post_hooks` via `sh -c`, then runs every
//! executable file in `~/.config/pesto/hooks/` (sorted by name), skipping
//! disabled ones (see [`is_disabled`]). `no_hooks` suppresses only the
//! directory scripts — `post_hooks` still run regardless. A `post_hooks`
//! entry that resolves to a script inside the hooks directory would run
//! twice (once from each phase); [`post_hook_targets_hooks_dir`] detects that
//! and logs a warning (issue #40).
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
    let hooks_dir = crate::config::config_dir().map(|d| d.join("hooks"));

    for cmd in &config.post_hooks {
        // A post_hooks entry that also lives in hooks_dir would run twice:
        // once here, once more during the directory scan below. See issue #40.
        if !config.no_hooks {
            if let Some(dir) = &hooks_dir {
                if post_hook_targets_hooks_dir(cmd, dir) {
                    let warning = format!(
                        "post_hooks entry targets a script inside {}; it will also be executed by the directory scan. Set no_hooks = true to suppress the directory scan, or move this script out of {} to rely on post_hooks alone.",
                        dir.display(),
                        dir.display()
                    );
                    tracing::warn!(cmd, hooks_dir = %dir.display(), "{}", warning);
                    logs.push(warning);
                }
            }
        }

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

    if let Some(hooks_dir) = &hooks_dir {
        if hooks_dir.is_dir() {
            let scripts = sorted_executables(hooks_dir);
            if !scripts.is_empty() {
                let summary = format!(
                    "discovered {} hook script(s) in {}; running in alphabetical order",
                    scripts.len(),
                    hooks_dir.display()
                );
                info!(dir = %hooks_dir.display(), count = scripts.len(), "hooks directory scan");
                logs.push(summary);
            }
            for script in scripts {
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

/// True when `cmd` looks like it invokes a script that lives directly inside
/// `hooks_dir` — meaning the same script would also be picked up by the
/// directory scan, running twice per upload (issue #40).
///
/// This is a heuristic string match, not a shell parse: it normalizes path
/// separators and looks for the resolved `hooks_dir` path inside `cmd`, then
/// falls back to matching the `<parent>/<hooks>` suffix so unresolved
/// placeholders like Windows' `%APPDATA%\pesto\hooks\...` still match even
/// though the literal env var was never expanded.
pub fn post_hook_targets_hooks_dir(cmd: &str, hooks_dir: &Path) -> bool {
    let normalized_cmd = cmd.replace('\\', "/").to_lowercase();

    let normalized_dir = hooks_dir
        .to_string_lossy()
        .replace('\\', "/")
        .to_lowercase();
    if !normalized_dir.is_empty() && normalized_cmd.contains(&normalized_dir) {
        return true;
    }

    if let (Some(hooks_name), Some(parent_name)) = (
        hooks_dir.file_name().and_then(|n| n.to_str()),
        hooks_dir
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str()),
    ) {
        let suffix = format!(
            "{}/{}/",
            parent_name.to_lowercase(),
            hooks_name.to_lowercase()
        );
        if normalized_cmd.contains(&suffix) {
            return true;
        }
    }

    false
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
    let mut c = script_command(path);
    apply_env(&mut c, ctx);
    run_command(c)
}

/// Build the command used to launch a hook script.
///
/// On Windows, `CreateProcess` can launch `.exe`/`.bat`/`.cmd` directly, but
/// has no knowledge of `.ps1` files (that association only exists in
/// `ShellExecute`/Explorer). Running a `.ps1` via `Command::new(path)` fails
/// with "%1 is not a valid Win32 application" (os error 193), so it must be
/// invoked through `powershell.exe -File` explicitly.
#[cfg(windows)]
fn script_command(path: &Path) -> Command {
    let is_ps1 = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ps1"));
    if is_ps1 {
        let mut c = Command::new("powershell");
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        c.arg(path);
        c
    } else {
        Command::new(path)
    }
}

#[cfg(not(windows))]
fn script_command(path: &Path) -> Command {
    Command::new(path)
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
        .filter(|p| p.is_file() && is_executable(p) && !is_disabled(p))
        .collect();
    scripts.sort();
    scripts
}

/// True when a hook script should be skipped despite being executable: a
/// backup copy (`foo.ps1.bak`, `foo.old`), an explicitly disabled script
/// (`foo.disabled`, `foo.off`), or any file inside a `disabled/` subfolder.
/// Lets users keep old copies of a hook script next to the active one
/// without them being auto-executed (see issue #40).
pub fn is_disabled(path: &Path) -> bool {
    let in_disabled_dir = path
        .parent()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.eq_ignore_ascii_case("disabled"));
    if in_disabled_dir {
        return true;
    }

    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    name.ends_with(".disabled")
        || name.ends_with(".off")
        || name.ends_with(".bak")
        || name.ends_with(".old")
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

    #[test]
    fn is_disabled_matches_known_suffixes() {
        assert!(is_disabled(Path::new("/hooks/foo.disabled")));
        assert!(is_disabled(Path::new("/hooks/foo.off")));
        assert!(is_disabled(Path::new("/hooks/foo.ps1.bak")));
        assert!(is_disabled(Path::new("/hooks/foo.ps1.old")));
        assert!(is_disabled(Path::new("/hooks/FOO.DISABLED")));
    }

    #[test]
    fn is_disabled_matches_disabled_subfolder() {
        assert!(is_disabled(Path::new("/hooks/disabled/foo.ps1")));
        assert!(is_disabled(Path::new("/hooks/Disabled/foo.sh")));
    }

    #[test]
    fn is_disabled_false_for_active_scripts() {
        assert!(!is_disabled(Path::new("/hooks/foo.ps1")));
        assert!(!is_disabled(Path::new("/hooks/foo.sh")));
        assert!(!is_disabled(Path::new("/hooks/foo-backup.ps1")));
    }

    #[test]
    fn post_hook_targets_hooks_dir_matches_resolved_path() {
        let dir = Path::new("/home/user/.config/pesto/hooks");
        assert!(post_hook_targets_hooks_dir(
            "/home/user/.config/pesto/hooks/pesto-hook.sh",
            dir
        ));
        assert!(!post_hook_targets_hooks_dir(
            "curl https://example.com/notify",
            dir
        ));
    }

    // `Path` parses separators per the compilation target, so a Windows-style
    // path only behaves as expected (`.parent()`/`.file_name()` split on `\`)
    // when actually compiled for Windows.
    #[cfg(windows)]
    #[test]
    fn post_hook_targets_hooks_dir_matches_unexpanded_windows_placeholder() {
        let dir = Path::new(r"C:\Users\alice\AppData\Roaming\pesto\hooks");
        assert!(post_hook_targets_hooks_dir(
            r#"pwsh -ExecutionPolicy Bypass -File %APPDATA%\pesto\hooks\pesto-hook.ps1 -ManualInput"#,
            dir
        ));
    }

    #[cfg(windows)]
    #[test]
    fn post_hook_targets_hooks_dir_false_for_unrelated_command() {
        let dir = Path::new(r"C:\Users\alice\AppData\Roaming\pesto\hooks");
        assert!(!post_hook_targets_hooks_dir(
            r#"pwsh -File C:\Scripts\notify.ps1"#,
            dir
        ));
    }

    #[cfg(unix)]
    #[test]
    fn post_hook_targets_hooks_dir_matches_unexpanded_unix_placeholder() {
        let dir = Path::new("/home/alice/.config/pesto/hooks");
        assert!(post_hook_targets_hooks_dir(
            "sh -c $HOME/.config/pesto/hooks/pesto-hook.sh",
            dir
        ));
    }

    #[cfg(unix)]
    #[test]
    fn post_hook_targets_hooks_dir_false_for_unrelated_command() {
        let dir = Path::new("/home/alice/.config/pesto/hooks");
        assert!(!post_hook_targets_hooks_dir("curl -X POST https://x", dir));
    }
}
