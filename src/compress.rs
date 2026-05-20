//! Archive compression before posting (Phase 13).
//!
//! Bundles the input files into a single archive that the posting pipeline
//! treats as any other file. The default format is **7z in store mode**
//! (no compression — PAR2 handles integrity; store keeps the pipeline fast).
//!
//! Supported formats:
//! - `7z`  — via the `7z` CLI (p7zip); piped through stdout on Unix so no
//!   intermediate file is needed; header encryption with `-mhe=on`
//! - `zip` — via the `7z` CLI; written to a temp file (zip requires seeking)
//! - `rar` — via the `rar` CLI (not distributed; must be in PATH)
//!
//! [`CompressResult`] owns its temp storage and cleans up on drop.

use std::path::PathBuf;
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

/// Owns the temporary storage backing a compressed archive.
// Fields exist solely to keep the temp storage alive until this value is dropped.
#[allow(dead_code)]
enum TempCleanup {
    /// The archive was streamed directly into this named temp file.
    File(tempfile::NamedTempFile),
    /// The archive was written by an external tool into this temp directory.
    Dir(tempfile::TempDir),
}

/// Result of a compression run. Owns its temp storage; the archive file is
/// deleted when this value is dropped.
pub struct CompressResult {
    /// Path to the created archive.
    pub path: PathBuf,
    /// Archive format used.
    pub format: ArchiveFormat,
    _cleanup: TempCleanup,
}

/// Create an archive containing all files listed in `inputs`.
///
/// `archive_stem` is the base name of the archive (without extension).
/// `password` is an optional password to protect the archive.
/// `on_progress` is called periodically with the number of bytes written so far.
///
/// For the 7z format on Unix the archive is streamed through stdout without
/// writing a full intermediate file, so `on_progress` reflects actual bytes
/// flowing through rather than a polled file-size estimate.
pub fn compress(
    inputs: &[PathBuf],
    archive_stem: &str,
    format: ArchiveFormat,
    password: Option<&str>,
    on_progress: impl Fn(u64),
) -> Result<CompressResult> {
    match format {
        ArchiveFormat::SevenZip => compress_7z_streamed(inputs, format, password, &on_progress),
        ArchiveFormat::Zip => {
            compress_7z_file(inputs, archive_stem, format, password, &on_progress)
        }
        ArchiveFormat::Rar => compress_rar_file(inputs, archive_stem, password, &on_progress),
    }
}

/// Split a 7z-format archive into volumes of at most `volume_size` bytes.
///
/// A named pipe (FIFO) is used as the 7z output path so the archive stream
/// can be split into [`NamedTempFile`] volumes purely in software as bytes
/// arrive, without waiting for the full archive to be written first.
///
/// Only supported for [`ArchiveFormat::SevenZip`] on Unix. Returns an error
/// on other platforms or formats.
///
/// `on_progress` is called after each write with bytes written to the current
/// volume so far.
pub fn compress_volumes(
    inputs: &[PathBuf],
    format: ArchiveFormat,
    password: Option<&str>,
    volume_size: u64,
    mut on_volume: impl FnMut(CompressResult) -> Result<()>,
    on_progress: impl Fn(u64),
) -> Result<()> {
    if format != ArchiveFormat::SevenZip {
        anyhow::bail!("--volume-size is only supported with the 7z format (got `{format}`)");
    }
    #[cfg(not(unix))]
    {
        anyhow::bail!(
            "--volume-size requires Unix (named pipes are not available on this platform)"
        );
    }
    #[cfg(unix)]
    {
        use std::io::{Read, Write};
        use std::process::Stdio;

        let bin = find_binary("7z").context(
            "7z not found in PATH; install p7zip (e.g. `apt install p7zip-full` or `brew install p7zip`)",
        )?;

        // Create a named pipe that 7z writes into. We read from the other end
        // and split the stream into fixed-size volumes.
        let pipe_dir = tempfile::TempDir::new().context("creating temp dir for FIFO")?;
        let fifo_path = pipe_dir.path().join("archive.7z");
        let status = Command::new("mkfifo")
            .arg(&fifo_path)
            .status()
            .context("running mkfifo")?;
        if !status.success() {
            anyhow::bail!("mkfifo failed with {status}");
        }

        let mut cmd = Command::new(&bin);
        cmd.arg("a")
            .arg("-t7z")
            .arg("-mx=0") // store mode: no compression
            .arg("-bd") // no progress bar
            .arg("-y"); // assume yes

        if let Some(pass) = password {
            cmd.arg(format!("-p{pass}"));
            cmd.arg("-mhe=on");
        }

        cmd.arg(&fifo_path);
        for input in inputs {
            cmd.arg(input);
        }
        // 7z writes to the FIFO; we don't need to capture its stdout.
        cmd.stdout(Stdio::null());
        cmd.stderr(Stdio::inherit());

        // Spawn 7z before opening the FIFO for reading — a FIFO open blocks
        // until both ends are open, so the writer must be alive first.
        let mut child = cmd.spawn().context("spawning 7z")?;

        // Opening the FIFO for reading unblocks the 7z writer.
        let mut fifo = std::fs::File::open(&fifo_path).context("opening FIFO for reading")?;

        let mut buf = vec![0u8; 64 * 1024];
        let mut current =
            tempfile::NamedTempFile::new().context("creating temp file for volume")?;
        let mut current_bytes: u64 = 0;

        loop {
            let n = fifo.read(&mut buf).context("reading from FIFO")?;
            if n == 0 {
                break;
            }
            let mut chunk = &buf[..n];

            // Split the chunk across volume boundaries as needed.
            while !chunk.is_empty() {
                let space = volume_size.saturating_sub(current_bytes);
                if space == 0 {
                    // Current volume is full — seal it and open a new one.
                    let next =
                        tempfile::NamedTempFile::new().context("creating temp file for volume")?;
                    let sealed = std::mem::replace(&mut current, next);
                    let path = sealed.path().to_path_buf();
                    on_volume(CompressResult {
                        path,
                        format,
                        _cleanup: TempCleanup::File(sealed),
                    })?;
                    current_bytes = 0;
                    continue; // re-check space with new volume
                }

                let write_len = (space as usize).min(chunk.len());
                current
                    .write_all(&chunk[..write_len])
                    .context("writing volume data")?;
                current_bytes += write_len as u64;
                chunk = &chunk[write_len..];
                on_progress(current_bytes);
            }
        }

        drop(fifo);
        let status = child.wait().context("waiting for 7z")?;
        if !status.success() {
            anyhow::bail!("`7z` exited with {status}");
        }

        // Seal the final (possibly partial) volume.
        if current_bytes > 0 {
            let path = current.path().to_path_buf();
            on_volume(CompressResult {
                path,
                format,
                _cleanup: TempCleanup::File(current),
            })?;
        }

        Ok(())
    }
}

/// 7z in SevenZip format: write to a `NamedTempFile` inside a `TempDir`.
///
/// Progress is reported after the archive is written by reading its size.
/// The streamed-stdout approach was unreliable across p7zip versions, so
/// we use the same temp-file strategy as Zip and RAR.
fn compress_7z_streamed(
    inputs: &[PathBuf],
    format: ArchiveFormat,
    password: Option<&str>,
    on_progress: &dyn Fn(u64),
) -> Result<CompressResult> {
    compress_7z_file(inputs, "archive", format, password, on_progress)
}

/// 7z in Zip format (and the non-Unix SevenZip fallback): write to a temp
/// directory so 7z can do the seeking the Zip format requires.
fn compress_7z_file(
    inputs: &[PathBuf],
    archive_stem: &str,
    format: ArchiveFormat,
    password: Option<&str>,
    on_progress: &dyn Fn(u64),
) -> Result<CompressResult> {
    let bin = find_binary("7z").context(
        "7z not found in PATH; install p7zip (e.g. `apt install p7zip-full` or `brew install p7zip`)",
    )?;

    let type_flag = match format {
        ArchiveFormat::SevenZip => "-t7z",
        ArchiveFormat::Zip => "-tzip",
        ArchiveFormat::Rar => unreachable!(),
    };

    let temp_dir = tempfile::TempDir::new().context("creating temp dir for archive")?;
    let archive_path = temp_dir
        .path()
        .join(format!("{}.{}", archive_stem, format.extension()));

    let mut cmd = Command::new(&bin);
    cmd.arg("a")
        .arg(type_flag)
        .arg("-mx=0") // store mode: no compression
        .arg("-bd") // no progress bar
        .arg("-y"); // assume yes

    if let Some(pass) = password {
        cmd.arg(format!("-p{pass}"));
        if format == ArchiveFormat::SevenZip {
            cmd.arg("-mhe=on");
        }
    }

    cmd.arg(&archive_path);
    for input in inputs {
        cmd.arg(input);
    }

    run_command(cmd, "7z")?;

    on_progress(archive_path.metadata().map(|m| m.len()).unwrap_or(0));

    Ok(CompressResult {
        path: archive_path,
        format,
        _cleanup: TempCleanup::Dir(temp_dir),
    })
}

fn compress_rar_file(
    inputs: &[PathBuf],
    archive_stem: &str,
    password: Option<&str>,
    on_progress: &dyn Fn(u64),
) -> Result<CompressResult> {
    let bin = find_binary("rar").context(
        "rar not found in PATH; install the RAR CLI (not distributed with pesto due to licensing)",
    )?;

    let temp_dir = tempfile::TempDir::new().context("creating temp dir for archive")?;
    let archive_path = temp_dir.path().join(format!("{}.rar", archive_stem));

    let mut cmd = Command::new(&bin);
    cmd.arg("a")
        .arg("-m0") // store mode: no compression
        .arg("-ep1") // strip paths up to the first component
        .arg("-inul"); // suppress output

    if let Some(pass) = password {
        // -hp encrypts both data and headers (hides internal file names).
        cmd.arg(format!("-hp{pass}"));
    }

    cmd.arg(&archive_path);
    for input in inputs {
        cmd.arg(input);
    }

    run_command(cmd, "rar")?;

    on_progress(archive_path.metadata().map(|m| m.len()).unwrap_or(0));

    Ok(CompressResult {
        path: archive_path,
        format: ArchiveFormat::Rar,
        _cleanup: TempCleanup::Dir(temp_dir),
    })
}

pub fn find_binary(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .as_deref()
        .unwrap_or_default()
        .to_string_lossy()
        .split(':')
        .map(|dir| PathBuf::from(dir).join(name))
        .find(|p| p.is_file())
}

fn run_command(mut cmd: Command, tool: &str) -> Result<()> {
    let output = cmd.output().with_context(|| format!("running `{tool}`"))?;

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
