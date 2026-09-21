//! Archive volume naming, discovery and validation helpers.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Validate a `-v<size>[u]` value shared by the `rar` and `7z` CLIs: digits
/// followed by an optional single unit character from `bBkKmMgGtT`.
pub(super) fn validate_volume_size(size: &str) -> Result<()> {
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
/// `stem.part01.rar`, `stem.part02.rar`, ... sorted in volume order.
///
/// `rar` pads `.partNN` only to the digit count the volume total needs — a
/// 6-volume set really is `part1..part6` — so the sort is natural, not plain
/// byte order (see [`list_matching`]).
///
/// When the requested volume size is larger than the whole archive, rar
/// decides a single volume is enough and falls back to writing the plain
/// `stem.rar` (no `.partNN` suffix at all) instead — checked for as a
/// fallback rather than an error.
pub(super) fn collect_rar_volumes(dest_dir: &Path, archive_stem: &str) -> Result<Vec<PathBuf>> {
    let prefix = format!("{archive_stem}.part");
    let volumes = list_matching(dest_dir, |name| {
        name.starts_with(&prefix) && name.ends_with(".rar")
    })?;
    if !volumes.is_empty() {
        return Ok(volumes);
    }
    let single = dest_dir.join(format!("{archive_stem}.rar"));
    if single.is_file() {
        return Ok(vec![single]);
    }
    bail!(
        "rar reported success but neither `{archive_stem}.part*.rar` volumes nor \
         `{archive_stem}.rar` were found in `{}`",
        dest_dir.display()
    );
}

/// If `real_name` ends with a volume suffix this module's `compress()`
/// produces (`.partNNN.rar` from rar, or `.NNN` after a `.7z`/`.zip` stem
/// from 7z), return that suffix, e.g. `.part07.rar`. `None` for a
/// single-file archive or a name unrelated to archive volumes.
///
/// Lets full-shared obfuscation (`poster::mod`) preserve the
/// indexer-recognizable volume pattern on the wire instead of a generic
/// numbered suffix — a `--compress-volume-size` release wouldn't group
/// under `--obfuscate=full-shared` otherwise, since indexers key their
/// "same release" grouping off this exact naming convention (issue #68).
pub fn volume_suffix(real_name: &str) -> Option<&str> {
    if let Some(rest) = real_name.strip_suffix(".rar") {
        let part_at = rest.rfind(".part")?;
        let digits = &rest[part_at + 5..];
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            return Some(&real_name[part_at..]);
        }
        return None;
    }
    let seven_zip_at = real_name.rfind(".7z.")?;
    let digits = &real_name[seven_zip_at + 4..];
    if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
        return Some(&real_name[seven_zip_at..]);
    }
    None
}

/// Translate the private, on-disk archive name used while compressing an
/// obfuscated upload into the stable name download clients must see.
///
/// Compression may use a random `physical_stem` so an interrupted upload can
/// find and reuse its scratch archive without exposing that identity on NNTP.
/// The poster already assigns an independent wire Subject and yEnc name, so
/// leaking the scratch stem into the NZB or PAR2 FileDesc is both unnecessary
/// and contrary to the client-path contract. The complete archive suffix is
/// retained for single files and split volumes alike (`.7z`, `.7z.001`,
/// `.part01.rar`, and so on).
pub fn client_archive_name(path: &Path, physical_stem: &str, client_stem: &str) -> String {
    let physical_name = path
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned();
    let suffix = physical_name
        .strip_prefix(physical_stem)
        .filter(|suffix| suffix.starts_with('.'))
        .map(str::to_owned);
    suffix.map_or(physical_name, |suffix| format!("{client_stem}{suffix}"))
}

/// Return the portable external archive stem used in NZB and PAR2 metadata.
///
/// PAR2's base File Description name is ASCII, and NZBGet's bundled
/// par2cmdline currently treats those bytes as Latin-1. Keeping the archive
/// name ASCII prevents a compressed upload from reintroducing that ambiguity;
/// non-ASCII source names are still retained inside formats that support them.
pub fn portable_archive_stem(stem: &str) -> String {
    if !stem.is_empty() && stem.is_ascii() {
        stem.to_owned()
    } else {
        "archive".to_owned()
    }
}

/// Find the volume files `7z -v` produced for `archive_name` (the full file
/// name including its `.7z`/`.zip` extension), e.g. `stem.7z.001`,
/// `stem.7z.002`, ... sorted in volume order (zero-padded, sorts correctly
/// as plain strings).
pub(super) fn collect_7z_volumes(dest_dir: &Path, archive_name: &str) -> Result<Vec<PathBuf>> {
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

/// List files in `dest_dir` whose file name matches `predicate`, in volume
/// order.
///
/// Sorted with [`crate::walk::natural_cmp`] rather than plain byte order: the
/// caller treats the first entry as the archive's *first* volume and posts the
/// rest in this order, and `rar` only pads `.partNN` to the digit count the
/// volume total needs — a set that came out unpadded would otherwise order
/// `part10` before `part2`. 7z's `.NNN` is always 3-digit padded, so this is a
/// no-op there.
pub(super) fn list_matching(
    dest_dir: &Path,
    predicate: impl Fn(&str) -> bool,
) -> Result<Vec<PathBuf>> {
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
    matches.sort_by(|a, b| crate::walk::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    Ok(matches)
}
