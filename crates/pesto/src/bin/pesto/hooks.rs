//! CLI hook environment and pre/post-upload hook execution policy.

use std::path::PathBuf;

use anyhow::Result;
use pesto::config::Config;

pub(super) struct HookEnv<'a> {
    pub(super) nzb_path: Option<&'a std::path::Path>,
    pub(super) nfo_path: Option<&'a std::path::Path>,
    pub(super) name: &'a str,
    pub(super) total_bytes: u64,
    /// Colon-separated list of input paths (empty string when unknown).
    pub(super) input_paths: &'a str,
    pub(super) group: Option<&'a str>,
    /// Colon-separated list of all newsgroups.
    pub(super) groups: &'a str,
    pub(super) password: Option<&'a str>,
    /// The server that actually accepted at least one article this run (the
    /// first entry of `servers` below), not just the statically configured
    /// primary — see `servers` for why this can differ in a multi-server
    /// (failover) config.
    pub(super) server: &'a str,
    /// Colon-separated list of every server that actually accepted at least
    /// one article this run, derived from `PostedSegment::server_idx` on the
    /// real posted results — not the configured list, which can include a
    /// failover server that never ended up receiving anything, or omit which
    /// one of several equally-configured servers a given run landed on.
    pub(super) servers: &'a str,
    pub(super) category: Option<&'a str>,
    pub(super) nzb_title: Option<&'a str>,
    pub(super) obfuscate: &'a str,
    pub(super) par2: u8,
    /// Space-separated list of NZB tags (empty string when none).
    pub(super) tags: &'a str,
    /// TMDb reference, e.g. `movie/12345` or `tv/12345` (`--tmdb`).
    pub(super) tmdb_id: Option<&'a str>,
    /// IMDb ID, e.g. `tt1234567` (`--imdb-id`).
    pub(super) imdb_id: Option<&'a str>,
    /// TheTVDB ID (`--tvdb-id`).
    pub(super) tvdb_id: Option<&'a str>,
    /// MyAnimeList ID (`--mal-id`).
    pub(super) mal_id: Option<&'a str>,
    /// True when the NZB was published despite MissingConfirmed.
    pub(super) incomplete: bool,
}

fn apply_hook_env(child: &mut std::process::Command, env: &HookEnv<'_>) {
    child.env("PESTO_NAME", env.name);
    child.env("PESTO_BYTES", env.total_bytes.to_string());
    child.env("PESTO_INPUT_PATHS", env.input_paths);
    child.env("PESTO_SERVER", env.server);
    child.env("PESTO_SERVERS", env.servers);
    child.env("PESTO_GROUP", env.group.unwrap_or(""));
    child.env("PESTO_GROUPS", env.groups);
    child.env("PESTO_PASSWORD", env.password.unwrap_or(""));
    child.env("PESTO_CATEGORY", env.category.unwrap_or(""));
    child.env("PESTO_NZB_TITLE", env.nzb_title.unwrap_or(""));
    // Deprecated alias of PESTO_NZB_TITLE, same value — kept so existing
    // hook scripts written before the --nzb-name -> --nzb-title rename keep
    // working. Will stop being set in a future release.
    child.env("PESTO_NZB_NAME", env.nzb_title.unwrap_or(""));
    child.env("PESTO_OBFUSCATE", env.obfuscate);
    child.env("PESTO_PAR2", env.par2.to_string());
    child.env("PESTO_TAGS", env.tags);
    child.env("PESTO_TMDB_ID", env.tmdb_id.unwrap_or(""));
    child.env("PESTO_IMDB_ID", env.imdb_id.unwrap_or(""));
    child.env("PESTO_TVDB_ID", env.tvdb_id.unwrap_or(""));
    child.env("PESTO_MAL_ID", env.mal_id.unwrap_or(""));
    child.env(
        "PESTO_NZB",
        env.nzb_path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    child.env(
        "PESTO_NFO",
        env.nfo_path
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default(),
    );
    child.env("PESTO_INCOMPLETE", if env.incomplete { "1" } else { "0" });
}

/// Execute a shell command as a pre-upload hook.
///
/// Runs via `sh -c` on Unix and `cmd /c` on Windows. Returns `Ok(())` when
/// the command exits with status 0, or an error (which aborts the upload) on
/// non-zero exit or if the process could not be started.
pub(super) fn run_pre_hook(cmd: &str, env: &HookEnv<'_>) -> Result<()> {
    #[cfg(unix)]
    let mut child = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    #[cfg(windows)]
    let mut child = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", cmd]);
        c
    };
    apply_hook_env(&mut child, env);
    match child.status() {
        Ok(s) if s.success() => Ok(()),
        Ok(s) => anyhow::bail!("pre-hook exited with status {s} — upload aborted"),
        Err(e) => anyhow::bail!("pre-hook failed to start: {e} — upload aborted"),
    }
}

/// Run every executable file in `pre_hooks_dir` as a pre-upload hook, sorted by name.
///
/// Each script must exit 0 to allow the upload to proceed. The first non-zero
/// exit aborts immediately — remaining scripts in the directory are skipped.
pub(super) fn run_pre_hooks_dir(pre_hooks_dir: &std::path::Path, env: &HookEnv<'_>) -> Result<()> {
    let Ok(entries) = std::fs::read_dir(pre_hooks_dir) else {
        return Ok(());
    };
    let mut scripts: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_executable(p))
        .collect();
    scripts.sort();
    for script in &scripts {
        println!("running pre-hook: {}", script.display());
        let mut child = hook_script_command(script);
        apply_hook_env(&mut child, env);
        match child.status() {
            Ok(s) if s.success() => println!("  pre-hook exited ok"),
            Ok(s) => anyhow::bail!(
                "pre-hook {} exited with status {s} — upload aborted",
                script.display()
            ),
            Err(e) => anyhow::bail!(
                "pre-hook {} failed to start: {e} — upload aborted",
                script.display()
            ),
        }
    }
    Ok(())
}

/// Execute a shell command as a post-upload hook.
///
/// Runs via `sh -c` on Unix and `cmd /c` on Windows so any interpreter works.
/// Errors are logged but never abort the caller.
fn run_post_hook(cmd: &str, env: &HookEnv<'_>) {
    #[cfg(unix)]
    let mut child = {
        let mut c = std::process::Command::new("sh");
        c.args(["-c", cmd]);
        c
    };
    #[cfg(windows)]
    let mut child = {
        let mut c = std::process::Command::new("cmd");
        c.args(["/c", cmd]);
        c
    };
    apply_hook_env(&mut child, env);
    match child.status() {
        Ok(s) if s.success() => println!("post-hook exited ok"),
        Ok(s) => eprintln!("post-hook exited with status {s}"),
        Err(e) => eprintln!("post-hook failed to start: {e}"),
    }
}

/// Run `config.post_hooks`, then every executable script in the hooks
/// directory (skipped when `config.no_hooks` is set). Warns when a
/// `post_hooks` entry resolves to a script inside the hooks directory, since
/// it would then run a second time during the directory scan (issue #40).
pub(super) fn run_all_hooks(config: &Config, env: &HookEnv<'_>) {
    let hooks_dir = pesto::config::config_dir().map(|d| d.join("hooks"));

    for cmd in &config.post_hooks {
        if !config.no_hooks {
            if let Some(dir) = &hooks_dir {
                if pesto::hooks::post_hook_targets_hooks_dir(cmd, dir) {
                    tracing::warn!(
                        cmd,
                        hooks_dir = %dir.display(),
                        "post_hooks entry targets a script inside the hooks directory; it will also be executed by the directory scan. Set no_hooks = true to suppress the directory scan, or move this script out of the hooks directory to rely on post_hooks alone."
                    );
                }
            }
        }
        run_post_hook(cmd, env);
    }

    if !config.no_hooks {
        if let Some(dir) = &hooks_dir {
            run_hooks_dir(dir, env);
        }
    }
}

/// Run every executable file in `hooks_dir`, sorted by name, skipping
/// disabled ones (see [`pesto::hooks::is_disabled`]).
///
/// Each script is executed directly (not via a shell) so it must have a
/// shebang line on Unix or a registered extension on Windows. Errors per
/// script are logged individually; one failing hook does not skip the rest.
fn run_hooks_dir(hooks_dir: &std::path::Path, env: &HookEnv<'_>) {
    let Ok(entries) = std::fs::read_dir(hooks_dir) else {
        return;
    };
    let mut scripts: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && is_executable(p) && !pesto::hooks::is_disabled(p))
        .collect();
    scripts.sort();
    if !scripts.is_empty() {
        println!(
            "discovered {} hook script(s) in {}; running in alphabetical order",
            scripts.len(),
            hooks_dir.display()
        );
    }
    pesto::memory::set_phase(pesto::memory::Phase::Hooks);
    for script in &scripts {
        println!("running hook: {}", script.display());
        let mut child = hook_script_command(script);
        apply_hook_env(&mut child, env);
        match child.status() {
            Ok(s) if s.success() => println!("  hook exited ok"),
            Ok(s) => eprintln!("  hook exited with status {s}"),
            Err(e) => eprintln!("  hook failed to start: {e}"),
        }
    }
}

/// Build the command used to launch a pre/post hook script.
///
/// On Windows, `CreateProcess` can launch `.exe`/`.bat`/`.cmd` directly, but
/// has no knowledge of `.ps1` files (that association only exists in
/// `ShellExecute`/Explorer). Running a `.ps1` via `Command::new(path)` fails
/// with "%1 is not a valid Win32 application" (os error 193), so it must be
/// invoked through an explicit PowerShell executable with `-File`. See
/// [`pesto::hooks::windows_powershell_exe`] for which one.
#[cfg(windows)]
fn hook_script_command(path: &std::path::Path) -> std::process::Command {
    let is_ps1 = path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| e.eq_ignore_ascii_case("ps1"));
    if is_ps1 {
        let mut c = std::process::Command::new(pesto::hooks::windows_powershell_exe());
        c.args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"]);
        c.arg(path);
        c
    } else {
        std::process::Command::new(path)
    }
}

#[cfg(not(windows))]
fn hook_script_command(path: &std::path::Path) -> std::process::Command {
    std::process::Command::new(path)
}

#[cfg(unix)]
fn is_executable(path: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|m| m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_executable(path: &std::path::Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some("exe" | "cmd" | "bat" | "ps1" | "py")
    )
}
