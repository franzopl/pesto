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

use anyhow::{bail, Context, Result};

mod backend;
mod volumes;

pub use backend::find_binary;
use backend::*;
use volumes::*;
pub use volumes::{client_archive_name, portable_archive_stem, volume_suffix};

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

/// If `dest_dir` already holds a complete archive for `archive_stem`,
/// return it so a `--resume` run can skip recompression (and keep the
/// recorded `{size, mtime}` fingerprints valid).
pub fn existing_archive(
    dest_dir: &Path,
    archive_stem: &str,
    format: ArchiveFormat,
    volume_size: Option<&str>,
) -> Option<CompressResult> {
    if !dest_dir.is_dir() {
        return None;
    }
    let archive_name = format!("{}.{}", archive_stem, format.extension());
    let archive_path = dest_dir.join(&archive_name);
    let mut paths = match (format, volume_size) {
        (ArchiveFormat::Rar, Some(_)) => collect_rar_volumes(dest_dir, archive_stem).ok()?,
        (ArchiveFormat::SevenZip | ArchiveFormat::Zip, Some(_)) => {
            collect_7z_volumes(dest_dir, &archive_name).ok()?
        }
        (_, None) if archive_path.is_file() => vec![archive_path],
        _ => return None,
    };
    if paths.is_empty() || !paths.iter().all(|p| p.is_file()) {
        return None;
    }
    let path = paths.remove(0);
    Some(CompressResult {
        path,
        extra_paths: paths,
        format,
    })
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
mod tests;
