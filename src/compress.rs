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
    /// The archive was written by an external tool into this temp directory.
    Dir(tempfile::TempDir),
    /// The archive is one of several volume files in a shared temp directory.
    /// The directory is deleted when the last volume holding this Arc is dropped.
    SharedDir(std::sync::Arc<tempfile::TempDir>),
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

/// Split a 7z-format archive into volumes of at most `volume_size` bytes,
/// calling `on_volume` for each volume **as soon as it is sealed** — before
/// the remaining volumes are compressed.
///
/// Uses 7z's native `-v` flag. A volume is considered sealed the moment 7z
/// begins writing the *next* volume: when `archive.7z.00N+1` appears in the
/// temp dir, `archive.7z.00N` is complete and ready for upload. The last
/// volume is dispatched after 7z exits.
///
/// This allows PAR2 generation and upload of volume N to run concurrently
/// with 7z still compressing volume N+1.
///
/// Only supported for [`ArchiveFormat::SevenZip`]. Returns an error for
/// other formats.
///
/// `on_progress` is called every ~200 ms with the total bytes written across
/// all volume files so far.
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

    let bin = find_binary("7z").context(
        "7z not found in PATH; install p7zip (e.g. `apt install p7zip-full` or `brew install p7zip`)",
    )?;

    let temp_dir =
        std::sync::Arc::new(tempfile::TempDir::new().context("creating temp dir for volumes")?);
    // 7z appends .001, .002, … to this base path.
    let archive_base = temp_dir.path().join("archive.7z");

    let mut cmd = Command::new(&bin);
    cmd.arg("a")
        .arg("-t7z")
        .arg("-mx=0") // store mode: no compression
        .arg("-bd") // no progress bar
        .arg("-y") // assume yes
        .arg(format!("-v{volume_size}b")); // native volume splitting

    if let Some(pass) = password {
        cmd.arg(format!("-p{pass}"));
        cmd.arg("-mhe=on");
    }

    cmd.arg(&archive_base);
    for input in inputs {
        cmd.arg(input);
    }
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());

    let mut child = cmd.spawn().context("spawning 7z")?;

    // sorted_volumes lists archive.7z.001, .002, … present in the temp dir.
    let sorted_volumes = |dir: &std::path::Path| -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = std::fs::read_dir(dir)
            .ok()
            .map(|entries| {
                entries
                    .filter_map(|e| e.ok().map(|e| e.path()))
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with("archive.7z."))
                            .unwrap_or(false)
                    })
                    .collect()
            })
            .unwrap_or_default();
        v.sort();
        v
    };

    // Number of volumes already dispatched via on_volume.
    let mut dispatched: usize = 0;

    // Seal volumes whose completeness is proven: a volume at index i is sealed
    // when a volume at index i+1 exists (7z has moved on to the next file).
    let seal_ready = |all: &[PathBuf],
                      dispatched: &mut usize,
                      up_to: usize,
                      temp_dir: &std::sync::Arc<tempfile::TempDir>,
                      on_volume: &mut dyn FnMut(CompressResult) -> Result<()>|
     -> Result<()> {
        while *dispatched < up_to && *dispatched < all.len() {
            let path = all[*dispatched].clone();
            let dir = std::sync::Arc::clone(temp_dir);
            on_volume(CompressResult {
                path,
                format,
                _cleanup: TempCleanup::SharedDir(dir),
            })?;
            *dispatched += 1;
        }
        Ok(())
    };

    loop {
        on_progress(dir_total_size(temp_dir.path()));

        let vols = sorted_volumes(temp_dir.path());
        // All volumes except the last are sealed (7z has moved on).
        let sealable = vols.len().saturating_sub(1);
        seal_ready(&vols, &mut dispatched, sealable, &temp_dir, &mut on_volume)?;

        match child.try_wait().context("waiting for 7z")? {
            Some(status) => {
                on_progress(dir_total_size(temp_dir.path()));
                if !status.success() {
                    let detail = child
                        .stderr
                        .as_mut()
                        .and_then(|s| {
                            use std::io::Read;
                            let mut buf = String::new();
                            s.read_to_string(&mut buf).ok()?;
                            Some(buf)
                        })
                        .unwrap_or_default();
                    bail!(
                        "`7z` exited with {status}{}",
                        if detail.trim().is_empty() {
                            String::new()
                        } else {
                            format!(": {}", detail.trim())
                        }
                    );
                }
                // 7z exited successfully — seal all remaining volumes.
                let vols = sorted_volumes(temp_dir.path());
                if vols.is_empty() {
                    anyhow::bail!(
                        "7z produced no output files in {}",
                        temp_dir.path().display()
                    );
                }
                seal_ready(
                    &vols,
                    &mut dispatched,
                    vols.len(),
                    &temp_dir,
                    &mut on_volume,
                )?;
                return Ok(());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(200)),
        }
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

    run_with_progress(cmd, "7z", &archive_path, on_progress)?;

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

    run_with_progress(cmd, "rar", &archive_path, on_progress)?;

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

/// Sum the sizes of all files directly inside `dir`.
fn dir_total_size(dir: &std::path::Path) -> u64 {
    std::fs::read_dir(dir)
        .ok()
        .map(|entries| {
            entries
                .filter_map(|e| e.ok())
                .filter_map(|e| e.metadata().ok())
                .map(|m| m.len())
                .sum()
        })
        .unwrap_or(0)
}

/// Spawn `cmd`, poll `output_path` file size every 200 ms until the process
/// exits, and call `on_progress` after each poll so the UI stays live.
fn run_with_progress(
    mut cmd: Command,
    tool: &str,
    output_path: &std::path::Path,
    on_progress: &dyn Fn(u64),
) -> Result<()> {
    cmd.stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().with_context(|| format!("spawning `{tool}`"))?;

    loop {
        match child
            .try_wait()
            .with_context(|| format!("waiting for `{tool}`"))?
        {
            Some(status) => {
                // Final progress tick before checking exit code.
                on_progress(output_path.metadata().map(|m| m.len()).unwrap_or(0));
                if !status.success() {
                    let detail = child
                        .stderr
                        .as_mut()
                        .and_then(|s| {
                            use std::io::Read;
                            let mut buf = String::new();
                            s.read_to_string(&mut buf).ok()?;
                            Some(buf)
                        })
                        .unwrap_or_default();
                    bail!(
                        "`{tool}` exited with {status}{}",
                        if detail.trim().is_empty() {
                            String::new()
                        } else {
                            format!(": {}", detail.trim())
                        }
                    );
                }
                return Ok(());
            }
            None => {
                on_progress(output_path.metadata().map(|m| m.len()).unwrap_or(0));
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
        }
    }
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

    // ── compress_volumes tests ────────────────────────────────────────────────

    fn needs_7z() -> bool {
        find_binary("7z").is_some()
    }

    /// Write `size` bytes of a repeating pattern to `path`.
    fn write_test_file(path: &std::path::Path, size: usize) {
        let data: Vec<u8> = (0..size).map(|i| (i % 251) as u8).collect();
        std::fs::write(path, &data).unwrap();
    }

    #[test]
    fn compress_volumes_rejects_non_7z_format() {
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("f.bin");
        write_test_file(&input, 1024);
        let err = compress_volumes(&[input], ArchiveFormat::Zip, None, 512, |_| Ok(()), |_| {});
        assert!(err.is_err());
        assert!(err.unwrap_err().to_string().contains("7z format"));
    }

    #[test]
    fn compress_volumes_single_volume_when_content_fits() {
        if !needs_7z() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("small.bin");
        write_test_file(&input, 1024);

        // Keep CompressResults alive — dropping them releases the shared TempDir.
        let mut results: Vec<CompressResult> = Vec::new();
        compress_volumes(
            &[input],
            ArchiveFormat::SevenZip,
            None,
            10 * 1024 * 1024, // 10 MiB limit — much larger than the file
            |vol| {
                results.push(vol);
                Ok(())
            },
            |_| {},
        )
        .unwrap();

        assert_eq!(results.len(), 1, "expected 1 volume, got {}", results.len());
        assert!(
            results[0].path.exists(),
            "volume file must exist while result is alive"
        );
        assert!(results[0].path.metadata().unwrap().len() > 0);
    }

    #[test]
    fn compress_volumes_splits_into_multiple_volumes() {
        if !needs_7z() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        // Two 1 MiB files; volumes capped at 600 KiB → must produce ≥ 3 volumes.
        let f1 = tmp.path().join("a.bin");
        let f2 = tmp.path().join("b.bin");
        write_test_file(&f1, 1024 * 1024);
        write_test_file(&f2, 1024 * 1024);

        let mut results: Vec<CompressResult> = Vec::new();
        compress_volumes(
            &[f1, f2],
            ArchiveFormat::SevenZip,
            None,
            600 * 1024,
            |vol| {
                assert!(
                    vol.path.exists(),
                    "volume path must exist when on_volume fires"
                );
                assert!(vol.path.metadata().unwrap().len() > 0);
                results.push(vol);
                Ok(())
            },
            |_| {},
        )
        .unwrap();

        assert!(
            results.len() >= 3,
            "expected ≥ 3 volumes for 2×1 MiB with 600 KiB limit, got {}",
            results.len()
        );

        // All volumes while still alive must be readable.
        for r in &results {
            let size = r.path.metadata().unwrap().len();
            assert!(size > 0, "volume {:?} is empty", r.path);
        }
    }

    #[test]
    fn compress_volumes_progress_is_reported_and_non_decreasing() {
        if !needs_7z() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("data.bin");
        write_test_file(&input, 2 * 1024 * 1024);

        let progress = std::sync::Mutex::new(Vec::<u64>::new());
        compress_volumes(
            &[input],
            ArchiveFormat::SevenZip,
            None,
            1024 * 1024,
            |_| Ok(()),
            |bytes| progress.lock().unwrap().push(bytes),
        )
        .unwrap();

        let ticks = progress.into_inner().unwrap();
        assert!(
            !ticks.is_empty(),
            "on_progress must be called at least once"
        );
        assert!(*ticks.last().unwrap() > 0, "final progress must be > 0");
        // Values must be non-decreasing.
        for w in ticks.windows(2) {
            assert!(w[1] >= w[0], "progress went backwards: {} → {}", w[0], w[1]);
        }
    }

    #[test]
    fn compress_volumes_volumes_are_valid_7z_archives() {
        if !needs_7z() {
            return;
        }
        let tmp = tempfile::TempDir::new().unwrap();
        let input = tmp.path().join("data.bin");
        write_test_file(&input, 2 * 1024 * 1024);

        let mut vol_paths: Vec<PathBuf> = Vec::new();
        // Keep results alive so temp storage is not dropped.
        let mut results: Vec<CompressResult> = Vec::new();
        compress_volumes(
            &[input],
            ArchiveFormat::SevenZip,
            None,
            1024 * 1024,
            |vol| {
                vol_paths.push(vol.path.clone());
                results.push(vol);
                Ok(())
            },
            |_| {},
        )
        .unwrap();

        // 7z l (list) on the first volume must succeed — validates the archive header.
        let status = Command::new("7z")
            .arg("l")
            .arg(&vol_paths[0])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "7z l failed on first volume");
    }
}
