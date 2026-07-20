//! Archive extraction for downloads that arrive compressed (`.rar`, `.7z`,
//! `.zip`).
//!
//! `pesto::compress` only *creates* archives before posting; it has no
//! extraction path to build on, so this is new code. Mirrors
//! `pesto::compress`'s conventions (shelling out to the `7z`/`unrar` CLIs,
//! `find_binary`, password redaction in debug logs) rather than
//! reimplementing archive format parsing — extraction, like PAR2, is
//! deliberately left to well-tested external tools.
//!
//! Runs after PAR2 verify/repair ([`crate::repair`]), never before: there is
//! no point extracting a `.rar`/`.7z` that PAR2 hasn't yet confirmed (or
//! repaired into) an intact state.

use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context, Result};
use pesto::compress::find_binary;
use tracing::debug;

/// Archive format `penne` knows how to extract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ArchiveKind {
    Rar,
    SevenZip,
    Zip,
}

/// One archive (possibly split across several volume files) found under a
/// download directory, and the single file that should be handed to the
/// extractor — the tool discovers sibling volumes itself from there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractionTarget {
    pub kind: ArchiveKind,
    pub base_name: String,
    pub entry_path: PathBuf,
}

/// Result of extracting one [`ExtractionTarget`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtractedArchive {
    pub base_name: String,
    pub kind: ArchiveKind,
    pub entry_path: PathBuf,
}

/// Identify an archive format from its leading bytes, regardless of file
/// name/extension. Used by [`crate::deobfuscate`] to classify obfuscated
/// files that [`classify`] (name-based) can't recognise.
pub fn sniff(bytes: &[u8]) -> Option<ArchiveKind> {
    if bytes.starts_with(b"Rar!\x1a\x07\x00") || bytes.starts_with(b"Rar!\x1a\x07\x01\x00") {
        Some(ArchiveKind::Rar)
    } else if bytes.starts_with(b"7z\xbc\xaf\x27\x1c") {
        Some(ArchiveKind::SevenZip)
    } else if bytes.starts_with(b"PK\x03\x04") {
        Some(ArchiveKind::Zip)
    } else {
        None
    }
}

/// Find every archive directly under `dir`, grouping multi-volume sets
/// (`.rar`+`.r00`+`.r01`+…, `.partN.rar`, `.7z.NNN`) so each becomes exactly
/// one [`ExtractionTarget`] rather than one per volume file.
pub fn find_extractable(dir: &Path) -> Result<Vec<ExtractionTarget>> {
    use std::collections::HashMap;

    /// One archive-set key's candidate volume files, as `(path, volume)` —
    /// see [`classify`] for what `volume` means.
    type VolumeFiles = Vec<(PathBuf, Option<u32>)>;

    let mut groups: HashMap<(ArchiveKind, String), VolumeFiles> = HashMap::new();

    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if !path.is_file() {
            continue;
        }
        let Some(file_name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if let Some((kind, base_name, volume)) = classify(file_name) {
            groups
                .entry((kind, base_name))
                .or_default()
                .push((path, volume));
        }
    }

    let mut targets: Vec<ExtractionTarget> = groups
        .into_iter()
        .filter_map(|((kind, base_name), files)| {
            let entry_path = files
                .iter()
                .find(|(_, v)| v.is_none())
                .or_else(|| files.iter().min_by_key(|(_, v)| v.unwrap()))
                .map(|(p, _)| p.clone())?;
            Some(ExtractionTarget {
                kind,
                base_name,
                entry_path,
            })
        })
        .collect();
    // Deterministic order for output/tests.
    targets.sort_by(|a, b| a.base_name.cmp(&b.base_name));
    Ok(targets)
}

/// Extract every archive found under `dir` (see [`find_extractable`]) into
/// `dir` itself, using `password` when the archive needs one.
pub async fn extract_all(dir: &Path, password: Option<&str>) -> Result<Vec<ExtractedArchive>> {
    let dir = dir.to_path_buf();
    let password = password.map(str::to_string);
    tokio::task::spawn_blocking(move || extract_all_blocking(&dir, password.as_deref()))
        .await
        .context("archive extraction task panicked")?
}

fn extract_all_blocking(dir: &Path, password: Option<&str>) -> Result<Vec<ExtractedArchive>> {
    let targets = find_extractable(dir)?;
    let mut extracted = Vec::with_capacity(targets.len());
    for target in targets {
        extract_one(&target, dir, password)?;
        extracted.push(ExtractedArchive {
            base_name: target.base_name,
            kind: target.kind,
            entry_path: target.entry_path,
        });
    }
    Ok(extracted)
}

fn extract_one(target: &ExtractionTarget, dest_dir: &Path, password: Option<&str>) -> Result<()> {
    match target.kind {
        ArchiveKind::SevenZip | ArchiveKind::Zip => {
            extract_with_7z(&target.entry_path, dest_dir, password)
        }
        ArchiveKind::Rar => extract_with_unrar(&target.entry_path, dest_dir, password),
    }
}

fn extract_with_7z(archive: &Path, dest_dir: &Path, password: Option<&str>) -> Result<()> {
    let bin = find_binary("7z").context(
        "7z not found in PATH; install p7zip (e.g. `apt install p7zip-full` or `brew install p7zip`)",
    )?;

    let mut cmd = Command::new(&bin);
    cmd.arg("x") // extract with full paths
        .arg("-y") // assume yes
        .arg(format!("-o{}", dest_dir.display())); // no space: 7z syntax
    if let Some(pass) = password {
        cmd.arg(format!("-p{pass}"));
    }
    cmd.arg(archive);

    run_command(cmd, "7z")
}

fn extract_with_unrar(archive: &Path, dest_dir: &Path, password: Option<&str>) -> Result<()> {
    let bin = find_binary("unrar").context(
        "unrar not found in PATH; install the unrar CLI (e.g. `apt install unrar` or `brew install unrar`)",
    )?;

    let mut cmd = Command::new(&bin);
    cmd.arg("x") // extract with full paths
        .arg("-y") // assume yes
        .arg("-o+"); // overwrite existing files without prompting
    if let Some(pass) = password {
        cmd.arg(format!("-p{pass}"));
    }
    cmd.arg(archive);
    // Trailing separator tells unrar this is a directory, not an output
    // filename.
    cmd.arg(format!(
        "{}{}",
        dest_dir.display(),
        std::path::MAIN_SEPARATOR
    ));

    run_command(cmd, "unrar")
}

/// Classify one file name as part of an archive set, if it is one.
///
/// Returns `(kind, base_name, volume)`, where `volume` is `None` for the
/// natural, un-numbered entry point (a bare `.rar`, `.7z` or `.zip`) and
/// `Some(n)` for one numbered volume of a split archive — the entry point
/// among a group of files sharing a `(kind, base_name)` is whichever one has
/// `None`, or failing that the smallest `Some(n)`.
///
/// `pub(crate)`, not private: [`crate::cleanup`] reuses this to recognize
/// every archive volume worth deleting after a successful extraction,
/// without duplicating the file-name rules this module already owns.
pub(crate) fn classify(file_name: &str) -> Option<(ArchiveKind, String, Option<u32>)> {
    let lower = file_name.to_ascii_lowercase();

    if let Some(base) = lower.strip_suffix(".zip") {
        return Some((ArchiveKind::Zip, base.to_string(), None));
    }

    if let Some(base) = lower.strip_suffix(".rar") {
        if let Some((stripped, n)) = strip_part_suffix(base) {
            return Some((ArchiveKind::Rar, stripped, Some(n)));
        }
        return Some((ArchiveKind::Rar, base.to_string(), None));
    }

    if let Some((base, ext)) = split_last_extension(&lower) {
        if let Some(n) = old_style_rar_suffix(ext) {
            return Some((ArchiveKind::Rar, base, Some(n)));
        }
    }

    if let Some(base) = lower.strip_suffix(".7z") {
        return Some((ArchiveKind::SevenZip, base.to_string(), None));
    }
    if let Some((base, ext)) = split_last_extension(&lower) {
        if ext.len() >= 3 && ext.chars().all(|c| c.is_ascii_digit()) {
            if let Some(base) = base.strip_suffix(".7z") {
                if let Ok(n) = ext.parse() {
                    return Some((ArchiveKind::SevenZip, base.to_string(), Some(n)));
                }
            }
        }
    }

    None
}

/// Split `"base.ext"` into `("base", "ext")` on the last `.`.
fn split_last_extension(name: &str) -> Option<(String, &str)> {
    let dot = name.rfind('.')?;
    Some((name[..dot].to_string(), &name[dot + 1..]))
}

/// `"r00"`, `"r01"`, … -> `Some(0)`, `Some(1)`, … — old-style RAR
/// continuation volumes (the `.rar` file itself is the implicit first
/// volume, classified separately with `volume = None`).
fn old_style_rar_suffix(ext: &str) -> Option<u32> {
    let digits = ext.strip_prefix('r')?;
    if digits.len() < 2 || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `"name.part1"` / `"name.part01"` -> `("name", 1)` — new-style RAR
/// volumes.
fn strip_part_suffix(base: &str) -> Option<(String, u32)> {
    let dot = base.rfind(".part")?;
    let digits = &base[dot + ".part".len()..];
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((base[..dot].to_string(), digits.parse().ok()?))
}

fn run_command(mut cmd: Command, tool: &str) -> Result<()> {
    if tracing::enabled!(tracing::Level::DEBUG) {
        let prog = cmd.get_program().to_string_lossy();
        let args: Vec<String> = cmd
            .get_args()
            .map(|a| {
                let s = a.to_string_lossy();
                if s.starts_with("-p") && s.len() > 2 {
                    "-p<redacted>".to_string()
                } else {
                    s.into_owned()
                }
            })
            .collect();
        debug!(program = %prog, args = ?args, "extractor command");
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
        debug!(tool, status = %output.status, stderr = %stderr.trim(), "extractor failed");
        bail!("`{tool}` exited with {}: {detail}", output.status);
    }

    debug!(tool, status = %output.status, "extractor ok");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniff_recognises_rar4_and_rar5_signatures() {
        assert_eq!(
            sniff(b"Rar!\x1a\x07\x00rest of the archive"),
            Some(ArchiveKind::Rar)
        );
        assert_eq!(
            sniff(b"Rar!\x1a\x07\x01\x00rest of the archive"),
            Some(ArchiveKind::Rar)
        );
    }

    #[test]
    fn sniff_recognises_7z_and_zip_signatures() {
        assert_eq!(
            sniff(b"7z\xbc\xaf\x27\x1crest"),
            Some(ArchiveKind::SevenZip)
        );
        assert_eq!(sniff(b"PK\x03\x04rest"), Some(ArchiveKind::Zip));
    }

    #[test]
    fn sniff_returns_none_for_unrelated_bytes() {
        assert_eq!(sniff(b"just a normal media file header"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn classifies_plain_archives() {
        assert_eq!(
            classify("movie.rar"),
            Some((ArchiveKind::Rar, "movie".into(), None))
        );
        assert_eq!(
            classify("movie.7z"),
            Some((ArchiveKind::SevenZip, "movie".into(), None))
        );
        assert_eq!(
            classify("movie.zip"),
            Some((ArchiveKind::Zip, "movie".into(), None))
        );
        assert_eq!(classify("movie.mkv"), None);
    }

    #[test]
    fn classifies_old_style_rar_volumes() {
        assert_eq!(
            classify("movie.r00"),
            Some((ArchiveKind::Rar, "movie".into(), Some(0)))
        );
        assert_eq!(
            classify("movie.r01"),
            Some((ArchiveKind::Rar, "movie".into(), Some(1)))
        );
    }

    #[test]
    fn classifies_new_style_rar_volumes() {
        assert_eq!(
            classify("movie.part1.rar"),
            Some((ArchiveKind::Rar, "movie".into(), Some(1)))
        );
        assert_eq!(
            classify("movie.part02.rar"),
            Some((ArchiveKind::Rar, "movie".into(), Some(2)))
        );
    }

    #[test]
    fn classifies_7z_volumes() {
        assert_eq!(
            classify("movie.7z.001"),
            Some((ArchiveKind::SevenZip, "movie".into(), Some(1)))
        );
        assert_eq!(
            classify("movie.7z.002"),
            Some((ArchiveKind::SevenZip, "movie".into(), Some(2)))
        );
    }

    #[test]
    fn does_not_misclassify_unrelated_names() {
        // "party" contains "part" but not ".partN" followed by digits only.
        assert_eq!(
            classify("movie.party.rar"),
            Some((ArchiveKind::Rar, "movie.party".into(), None))
        );
        assert_eq!(classify("readme.txt"), None);
    }

    #[test]
    fn find_extractable_groups_old_style_volumes_and_picks_the_rar() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["movie.rar", "movie.r00", "movie.r01"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let targets = find_extractable(dir.path()).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].kind, ArchiveKind::Rar);
        assert_eq!(targets[0].base_name, "movie");
        assert_eq!(targets[0].entry_path, dir.path().join("movie.rar"));
    }

    #[test]
    fn find_extractable_groups_new_style_volumes_and_picks_part1() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["movie.part2.rar", "movie.part1.rar", "movie.part3.rar"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let targets = find_extractable(dir.path()).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].entry_path, dir.path().join("movie.part1.rar"));
    }

    #[test]
    fn find_extractable_groups_7z_volumes_and_picks_the_first() {
        let dir = tempfile::tempdir().unwrap();
        for name in ["movie.7z.002", "movie.7z.001", "movie.7z.003"] {
            std::fs::write(dir.path().join(name), b"x").unwrap();
        }
        let targets = find_extractable(dir.path()).unwrap();
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].entry_path, dir.path().join("movie.7z.001"));
    }

    #[test]
    fn find_extractable_treats_unrelated_files_and_multiple_archives_separately() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rar"), b"x").unwrap();
        std::fs::write(dir.path().join("b.zip"), b"x").unwrap();
        std::fs::write(dir.path().join("readme.nfo"), b"x").unwrap();

        let targets = find_extractable(dir.path()).unwrap();
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].base_name, "a");
        assert_eq!(targets[1].base_name, "b");
    }

    #[test]
    fn find_extractable_empty_dir_yields_nothing() {
        let dir = tempfile::tempdir().unwrap();
        assert!(find_extractable(dir.path()).unwrap().is_empty());
    }
}
