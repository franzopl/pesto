//! Archive compression before posting (Phase 13).
//!
//! Bundles the input files into a single archive that the posting pipeline
//! treats as any other file. The default format is **7z in store mode**
//! (no compression — PAR2 handles integrity; store keeps the pipeline fast).
//!
//! Supported formats:
//! - `7z`  — via the `7z` CLI (p7zip); header encryption with `-mhe=on`;
//!   supports splitting into volumes
//! - `zip` — via the `7z` CLI; no header encryption (zip spec limitation);
//!   no volume support (7z's zip backend rejects `-v`)
//! - `rar` — via the `rar` CLI (not distributed; must be in PATH); supports
//!   splitting into volumes
//!
//! The caller is responsible for deleting the returned archive path(s) when
//! done — see [`CompressResult::extra_paths`] for the multi-volume case.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use tracing::debug;

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
#[derive(Debug)]
pub struct CompressResult {
    /// Path to the first (or only) archive file created, in a temp directory.
    pub path: PathBuf,
    /// Any additional volumes, in order, when `volume_size` split the
    /// archive into multiple parts. Empty for a single-file archive.
    /// Callers that post `path` must also post these or the release will be
    /// incomplete.
    pub extra_paths: Vec<PathBuf>,
    /// Archive format used.
    pub format: ArchiveFormat,
}

/// Create an archive containing all files listed in `inputs`.
///
/// `archive_stem` is the base name of the archive file (without extension).
/// `dest_dir` is where the archive file is written.
/// `password` is an optional password to protect the archive.
/// `volume_size` splits the archive into multiple volumes instead of one
/// monolithic file, e.g. `Some("500m")`; supported for `ArchiveFormat::Rar`
/// (`stem.partNN.rar`) and `ArchiveFormat::SevenZip` (`stem.7z.NNN`) — 7z's
/// zip backend has no volume support, so `ArchiveFormat::Zip` rejects it.
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
    volume_size: Option<&str>,
) -> Result<CompressResult> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("creating temp dir `{}`", dest_dir.display()))?;

    if let Some(size) = volume_size {
        if format == ArchiveFormat::Zip {
            bail!(
                "--compress-volume-size does not support --compress=zip \
                 (7z's zip backend has no volume support); use rar or 7z"
            );
        }
        validate_volume_size(size)?;
    }

    let archive_name = format!("{}.{}", archive_stem, format.extension());
    let archive_path = dest_dir.join(&archive_name);

    match format {
        ArchiveFormat::SevenZip | ArchiveFormat::Zip => {
            compress_with_7z(&archive_path, inputs, format, password, volume_size)?;
            let mut paths = match volume_size {
                Some(_) => collect_7z_volumes(dest_dir, &archive_name)?,
                None => vec![archive_path],
            };
            let path = paths.remove(0);
            Ok(CompressResult {
                path,
                extra_paths: paths,
                format,
            })
        }
        ArchiveFormat::Rar => {
            compress_with_rar(&archive_path, inputs, password, volume_size)?;
            let mut paths = match volume_size {
                Some(_) => collect_rar_volumes(dest_dir, archive_stem)?,
                None => vec![archive_path],
            };
            let path = paths.remove(0);
            Ok(CompressResult {
                path,
                extra_paths: paths,
                format,
            })
        }
    }
}

/// Validate a `-v<size>[u]` value shared by the `rar` and `7z` CLIs: digits
/// followed by an optional single unit character from `bBkKmMgGtT`.
fn validate_volume_size(size: &str) -> Result<()> {
    let digits = match size.chars().last() {
        Some(c) if c.is_ascii_alphabetic() => &size[..size.len() - 1],
        _ => size,
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        bail!(
            "invalid --compress-volume-size `{size}`; expected a number optionally \
             followed by a unit (b/k/m/g/t), e.g. `500m` or `4g`"
        );
    }
    Ok(())
}

/// Find the volume files `rar -v` produced for `archive_stem`, e.g.
/// `stem.part01.rar`, `stem.part02.rar`, ... sorted in volume order (the
/// zero-padded numbering rar generates sorts correctly as plain strings).
fn collect_rar_volumes(dest_dir: &Path, archive_stem: &str) -> Result<Vec<PathBuf>> {
    let prefix = format!("{archive_stem}.part");
    let volumes = list_matching(dest_dir, |name| {
        name.starts_with(&prefix) && name.ends_with(".rar")
    })?;
    if volumes.is_empty() {
        bail!(
            "rar reported success but no `{archive_stem}.part*.rar` volumes were found in `{}`",
            dest_dir.display()
        );
    }
    Ok(volumes)
}

/// Find the volume files `7z -v` produced for `archive_name` (the full file
/// name including its `.7z`/`.zip` extension), e.g. `stem.7z.001`,
/// `stem.7z.002`, ... sorted in volume order (zero-padded, sorts correctly
/// as plain strings).
fn collect_7z_volumes(dest_dir: &Path, archive_name: &str) -> Result<Vec<PathBuf>> {
    let prefix = format!("{archive_name}.");
    let volumes = list_matching(dest_dir, |name| {
        name.strip_prefix(&prefix)
            .is_some_and(|suffix| !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit()))
    })?;
    if volumes.is_empty() {
        bail!(
            "7z reported success but no `{archive_name}.NNN` volumes were found in `{}`",
            dest_dir.display()
        );
    }
    Ok(volumes)
}

/// List and sort files in `dest_dir` whose file name matches `predicate`.
fn list_matching(dest_dir: &Path, predicate: impl Fn(&str) -> bool) -> Result<Vec<PathBuf>> {
    let mut matches: Vec<PathBuf> = std::fs::read_dir(dest_dir)
        .with_context(|| format!("reading temp dir `{}`", dest_dir.display()))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(&predicate)
        })
        .collect();
    matches.sort();
    Ok(matches)
}

fn compress_with_7z(
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
        cmd.arg(input);
    }

    run_command(cmd, "7z")
}

fn compress_with_rar(
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
        cmd.arg(input);
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

fn run_command(mut cmd: Command, tool: &str) -> Result<()> {
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

/// Generate a random archive password: 24 ASCII alphanumeric characters.
///
/// Uses the same `RandomState`-based entropy source as `Message-ID` generation
/// and obfuscated names — OS-seeded on every construction, no RNG crate needed.
pub fn random_password() -> String {
    use std::collections::hash_map::RandomState;
    use std::hash::{BuildHasher, Hasher};

    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
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
        assert!(
            p.chars().all(|c| c.is_ascii_alphanumeric()),
            "non-alphanumeric: {p}"
        );
    }

    #[test]
    fn random_passwords_are_not_identical() {
        // Two calls in the same process should differ (LCG advances seed).
        let a = random_password();
        let b = random_password();
        assert_ne!(a, b);
    }

    #[test]
    fn volume_size_accepts_digits_with_optional_unit() {
        for ok in ["500", "500m", "4g", "700k", "1T", "1b"] {
            assert!(
                validate_volume_size(ok).is_ok(),
                "expected `{ok}` to be accepted"
            );
        }
    }

    #[test]
    fn volume_size_rejects_malformed_values() {
        for bad in ["", "mb", "500mb", "-5m", "5 m", "m5"] {
            assert!(
                validate_volume_size(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn volume_size_rejects_zip_format() {
        let dir = std::env::temp_dir().join(format!("pesto_compress_test_{}", std::process::id()));
        let err = compress(&[], "stem", &dir, ArchiveFormat::Zip, None, Some("500m")).unwrap_err();
        assert!(err.to_string().contains("--compress-volume-size"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rar_volume_size_splits_archive_into_parts() {
        if find_binary("rar").is_none() {
            eprintln!("skipping: rar CLI not installed");
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("pesto_compress_test_vol_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("input.bin");
        std::fs::write(&src, vec![0u8; 5_000_000]).unwrap();

        let result = compress(&[src], "stem", &dir, ArchiveFormat::Rar, None, Some("1m")).unwrap();

        assert!(result.path.exists());
        assert!(
            !result.extra_paths.is_empty(),
            "expected multiple volumes for a 5MB input split at 1MB"
        );
        for p in std::iter::once(&result.path).chain(result.extra_paths.iter()) {
            assert!(p.exists(), "volume `{}` missing on disk", p.display());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sevenzip_volume_size_splits_archive_into_parts() {
        if find_binary("7z").is_none() {
            eprintln!("skipping: 7z CLI not installed");
            return;
        }
        let dir =
            std::env::temp_dir().join(format!("pesto_compress_test_7z_vol_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("input.bin");
        std::fs::write(&src, vec![0u8; 5_000_000]).unwrap();

        let result = compress(
            &[src],
            "stem",
            &dir,
            ArchiveFormat::SevenZip,
            None,
            Some("1m"),
        )
        .unwrap();

        assert!(result.path.exists());
        assert!(
            !result.extra_paths.is_empty(),
            "expected multiple volumes for a 5MB input split at 1MB"
        );
        for p in std::iter::once(&result.path).chain(result.extra_paths.iter()) {
            assert!(p.exists(), "volume `{}` missing on disk", p.display());
        }

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn format_round_trips() {
        for (s, expected) in &[
            ("7z", ArchiveFormat::SevenZip),
            ("zip", ArchiveFormat::Zip),
            ("rar", ArchiveFormat::Rar),
        ] {
            assert_eq!(ArchiveFormat::parse(s), Some(*expected));
            assert_eq!(expected.extension(), *s);
        }
        assert_eq!(ArchiveFormat::parse("tar"), None);
    }
}
