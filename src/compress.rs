//! Archive compression before posting (Phase 13).
//!
//! Bundles the input files into a single archive that the posting pipeline
//! treats as any other file. The default format is **7z in store mode**
//! (no compression — PAR2 handles integrity; store keeps the pipeline fast).
//!
//! Supported formats:
//! - `7z`  — via the `7z` CLI (p7zip); header encryption with `-mhe=on`
//! - `zip` — via the `7z` CLI; no header encryption (zip spec limitation)
//! - `rar` — via the `rar` CLI (not distributed; must be in PATH)
//!
//! The caller is responsible for deleting the returned archive path when done.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};

/// Supported archive formats.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ArchiveFormat {
    #[default]
    SevenZip,
    Zip,
    Rar,
}

impl ArchiveFormat {
    pub fn extension(self) -> &'static str {
        match self {
            ArchiveFormat::SevenZip => "7z",
            ArchiveFormat::Zip => "zip",
            ArchiveFormat::Rar => "rar",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "7z" => Some(ArchiveFormat::SevenZip),
            "zip" => Some(ArchiveFormat::Zip),
            "rar" => Some(ArchiveFormat::Rar),
            _ => None,
        }
    }
}

impl std::fmt::Display for ArchiveFormat {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.extension())
    }
}

/// Result of a compression run.
pub struct CompressResult {
    /// Path to the created archive (in a temp directory).
    pub path: PathBuf,
    /// Archive format used.
    pub format: ArchiveFormat,
}

/// Create an archive containing all files listed in `inputs`.
///
/// `archive_stem` is the base name of the archive file (without extension).
/// `dest_dir` is where the archive file is written.
/// `password` is an optional password to protect the archive.
///
/// Each path in `inputs` is added at the root of the archive, preserving
/// only the base name (not the full filesystem path). For a directory upload
/// the caller should pass the root directory path so the internal structure
/// is preserved.
pub fn compress(
    inputs: &[PathBuf],
    archive_stem: &str,
    dest_dir: &Path,
    format: ArchiveFormat,
    password: Option<&str>,
) -> Result<CompressResult> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating temp dir `{}`", dest_dir.display()))?;

    let archive_name = format!("{}.{}", archive_stem, format.extension());
    let archive_path = dest_dir.join(&archive_name);

    match format {
        ArchiveFormat::SevenZip | ArchiveFormat::Zip => {
            compress_with_7z(&archive_path, inputs, format, password)?;
        }
        ArchiveFormat::Rar => {
            compress_with_rar(&archive_path, inputs, password)?;
        }
    }

    Ok(CompressResult {
        path: archive_path,
        format,
    })
}

fn compress_with_7z(
    archive_path: &Path,
    inputs: &[PathBuf],
    format: ArchiveFormat,
    password: Option<&str>,
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
        .arg("-bd")   // no progress bar
        .arg("-y");   // assume yes

    if let Some(pass) = password {
        cmd.arg(format!("-p{pass}"));
        // Encrypt archive headers too (hides internal file names).
        // Only supported by 7z format; zip has no header encryption.
        if format == ArchiveFormat::SevenZip {
            cmd.arg("-mhe=on");
        }
    }

    cmd.arg(archive_path);
    for input in inputs {
        cmd.arg(input);
    }

    run_command(cmd, "7z")
}

fn compress_with_rar(
    archive_path: &Path,
    inputs: &[PathBuf],
    password: Option<&str>,
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

    cmd.arg(archive_path);
    for input in inputs {
        cmd.arg(input);
    }

    run_command(cmd, "rar")
}

fn find_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .as_deref()
        .unwrap_or_default()
        .to_string_lossy()
        .split(':')
        .map(|dir| PathBuf::from(dir).join(name))
        .find(|p| p.is_file())
}

fn run_command(mut cmd: Command, tool: &str) -> Result<()> {
    let output = cmd
        .output()
        .with_context(|| format!("running `{tool}`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if !stderr.is_empty() {
            stderr.trim().to_string()
        } else {
            stdout.trim().to_string()
        };
        bail!("`{tool}` exited with {}: {detail}", output.status);
    }
    Ok(())
}

/// Generate a random archive password: 24 ASCII alphanumeric characters.
///
/// Uses the same `RandomState`-based entropy source as `Message-ID` generation
/// and obfuscated names — OS-seeded on every construction, no RNG crate needed.
pub fn random_password() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    const ALPHABET: &[u8] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(24);
    let (mut bits, mut left) = (0u64, 0u32);
    for _ in 0..24 {
        if left < 8 {
            // Each `RandomState` construction is seeded by the OS, giving a
            // fresh 64-bit value on every call — the same idiom used in article.rs.
            let mut h = RandomState::new().build_hasher();
            h.write_u8(0); // need at least one write before finish()
            bits = h.finish();
            left = 64;
        }
        out.push(ALPHABET[(bits & 0xff) as usize % ALPHABET.len()] as char);
        bits >>= 8;
        left -= 8;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_password_is_24_alphanumeric_chars() {
        let p = random_password();
        assert_eq!(p.len(), 24);
        assert!(p.chars().all(|c| c.is_ascii_alphanumeric()), "non-alphanumeric: {p}");
    }

    #[test]
    fn random_passwords_are_not_identical() {
        // Two calls in the same process should differ (LCG advances seed).
        let a = random_password();
        let b = random_password();
        assert_ne!(a, b);
    }

    #[test]
    fn format_round_trips() {
        for (s, expected) in &[("7z", ArchiveFormat::SevenZip), ("zip", ArchiveFormat::Zip), ("rar", ArchiveFormat::Rar)] {
            assert_eq!(ArchiveFormat::parse(s), Some(*expected));
            assert_eq!(expected.extension(), *s);
        }
        assert_eq!(ArchiveFormat::parse("tar"), None);
    }
}
