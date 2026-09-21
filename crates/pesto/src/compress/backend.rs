//! External archive-tool invocation.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::debug;

use super::ArchiveFormat;

pub(super) fn compress_with_7z(
    archive_path: &Path,
    inputs: &[PathBuf],
    format: ArchiveFormat,
    password: Option<&str>,
    volume_size: Option<&str>,
) -> Result<()> {
    let bin = find_binary("7z").context(
        "7z not found in PATH; install p7zip (e.g. `apt install p7zip-full` or `brew install p7zip`)",
    )?;

    let type_flag = match format {
        ArchiveFormat::SevenZip => "-t7z",
        ArchiveFormat::Zip => "-tzip",
        ArchiveFormat::Rar => unreachable!(),
    };

    let mut cmd = Command::new(&bin);
    cmd.arg("a")
        .arg(type_flag)
        .arg("-mx=0") // store mode: no compression
        .arg("-bd") // no progress bar
        .arg("-y"); // assume yes

    if let Some(pass) = password {
        cmd.arg(format!("-p{pass}"));
        // Encrypt archive headers too (hides internal file names).
        // Only supported by 7z format; zip has no header encryption.
        if format == ArchiveFormat::SevenZip {
            cmd.arg("-mhe=on");
        }
    }

    if let Some(size) = volume_size {
        // Splits the archive into `archive_name.001`, `archive_name.002`, ...
        // instead of one monolithic file — see issue #68 for why smaller
        // volumes are worth testing against indexer grouping on very large
        // releases. Only reached for SevenZip; `compress()` rejects this for
        // Zip before we get here (7z's zip backend errors out on -v).
        cmd.arg(format!("-v{size}"));
    }

    cmd.arg(archive_path);
    for input in inputs {
        let abs = if input.is_absolute() {
            input.clone()
        } else {
            std::env::current_dir()
                .context("resolving the current directory for a 7z input")?
                .join(input)
        };
        cmd.arg(abs);
    }

    run_command(cmd, "7z")
}

pub(super) fn compress_with_rar(
    archive_path: &Path,
    inputs: &[PathBuf],
    password: Option<&str>,
    volume_size: Option<&str>,
) -> Result<()> {
    let bin = find_binary("rar").context(
        "rar not found in PATH; install the RAR CLI (not distributed with pesto due to licensing)",
    )?;

    let mut cmd = Command::new(&bin);
    cmd.arg("a")
        .arg("-m0") // store mode: no compression
        .arg("-ep1") // strip paths up to the first component
        .arg("-inul"); // suppress output

    if let Some(pass) = password {
        // -hp encrypts both data and headers (hides internal file names).
        cmd.arg(format!("-hp{pass}"));
    }

    if let Some(size) = volume_size {
        // Splits the archive into `stem.partNN.rar` volumes instead of one
        // monolithic file (this rar version defaults to that naming with -v,
        // no -vn switch needed) — see issue #68 for why smaller volumes are
        // worth testing against indexer grouping on very large releases.
        cmd.arg(format!("-v{size}"));
    }

    cmd.arg(archive_path);
    for input in inputs {
        let abs = if input.is_absolute() {
            input.clone()
        } else {
            std::env::current_dir()
                .context("resolving the current directory for a RAR input")?
                .join(input)
        };
        cmd.arg(abs);
    }

    run_command(cmd, "rar")
}

pub fn find_binary(name: &str) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let bare = dir.join(name);
        if bare.is_file() {
            return Some(bare);
        }
        #[cfg(windows)]
        for ext in ["exe", "cmd", "bat"] {
            let with_ext = dir.join(format!("{name}.{ext}"));
            if with_ext.is_file() {
                return Some(with_ext);
            }
        }
    }
    None
}

pub(super) fn run_command(mut cmd: Command, tool: &str) -> Result<()> {
    // Log the sanitized command (password arguments are never in the visible
    // args here because callers embed -p<pass>/-hp<pass> as a single opaque
    // arg string; we still redact anything that looks like a password flag).
    if tracing::enabled!(tracing::Level::DEBUG) {
        let prog = cmd.get_program().to_string_lossy();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| {
                let s = a.to_string_lossy();
                // Redact -p<pass>, -hp<pass>, -p<pass> patterns (7z / rar).
                if (s.starts_with("-p") || s.starts_with("-hp")) && s.len() > 3 {
                    let prefix = if s.starts_with("-hp") { "-hp" } else { "-p" };
                    format!("{prefix}<redacted>")
                } else {
                    s.into_owned()
                }
            })
            .collect();
        debug!(program = %prog, args = ?args, "compressor command");
    }

    let output = cmd.output().with_context(|| format!("running `{tool}`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        debug!(tool, status = %output.status, stderr = %stderr.trim(), "compressor failed");
        bail!("`{tool}` exited with {}: {detail}", output.status);
    }

    debug!(tool, status = %output.status, "compressor ok");
    Ok(())
}
