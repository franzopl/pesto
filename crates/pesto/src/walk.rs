//! Expansion of CLI path arguments into a flat list of files to post.
//!
//! A `FILE` argument may be a plain file or a directory (for example a TV-show
//! season, possibly with nested subfolders). Directories are walked
//! recursively and every contained file becomes one input. Each input keeps a
//! *relative name* that preserves its position in the tree, so later stages
//! (`.nzb`, PAR2) can rebuild the original directory layout.
//!
//! Traversal rules (deliberately simple and predictable):
//!
//! - **Hidden entries** — files and directories whose name starts with `.`
//!   are included like any other; the upload is a faithful copy of the tree.
//! - **Symlinks** — symlinks found *inside* a directory are skipped, with a
//!   warning. This avoids traversal loops and links that escape the tree. A
//!   symlink passed *directly* as an argument is followed, since the user
//!   named it explicitly.
//! - **Empty directories** — carry no files and are simply not represented;
//!   an upload that resolves to zero files is rejected.
//! - **Unreadable entries** — a directory entry that cannot be read is skipped
//!   with a warning so one bad file does not abort the whole upload; a
//!   top-level argument that cannot be read is a hard error.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

/// A single file selected for posting, with the name it is published under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputFile {
    /// Filesystem path used to read the bytes.
    pub path: PathBuf,
    /// Name published in the `.nzb` and PAR2 metadata. For a file given
    /// directly this is its base name; for a file found inside a directory
    /// argument it is the path relative to that directory's parent, so the
    /// directory name is kept as the top-level component
    /// (`season01/episode01.mkv`). Always uses `/` separators.
    pub name: String,
}

/// Compare two published names in "natural" order: runs of digits compare by
/// numeric value, so `part2.rar` sorts before `part10.rar` — plain
/// lexicographic order gets that backwards whenever the volume number is
/// unpadded (`rar` only pads to the digit count the volume total needs, so a
/// 9-volume set is `part1..part9`).
///
/// This is the release's canonical order: what [`expand_inputs`] sorts by,
/// what `--obfuscate=full-shared`'s `{prefix}-NN` wire names are numbered by,
/// what `--file-counter`'s `[N/M]` counts in, and the order the `.nzb`'s file
/// list is written in. It is the same comparator `--each`/`--season` order
/// their entries with, so a release's internal order and a batch's entry order
/// never disagree.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    lexical_sort::natural_lexical_cmp(a, b)
}

/// Expand the CLI `paths` into a sorted, de-duplicated list of files.
///
/// Plain files are kept as-is; directories are walked recursively. The result
/// is sorted by `name` ([`natural_cmp`]) so a run — and the PAR2 set derived
/// from it — is reproducible, and so every stage that walks this list in order
/// (notably `--obfuscate=full-shared`'s `{prefix}-NN` wire names) numbers the
/// release the way a human reads it. Returns an error when `paths` resolves to
/// no files or when two inputs would be published under the same name.
pub fn expand_inputs(paths: &[PathBuf]) -> Result<Vec<InputFile>> {
    if paths.is_empty() {
        bail!("no files or directories given");
    }

    let mut out = Vec::new();
    for path in paths {
        let md = fs::metadata(path).with_context(|| format!("reading `{}`", path.display()))?;
        if md.is_file() {
            out.push(InputFile {
                path: path.clone(),
                name: base_name(path)?,
            });
        } else if md.is_dir() {
            let root = base_name(path).with_context(|| {
                format!(
                    "cannot determine a name for directory `{}`; pass it by name",
                    path.display()
                )
            })?;
            walk_dir(path, &root, &mut out)?;
        } else {
            bail!("`{}` is neither a file nor a directory", path.display());
        }
    }

    if out.is_empty() {
        bail!("no files to post: the given directories were empty or held only skipped entries");
    }

    out.sort_by(|a, b| natural_cmp(&a.name, &b.name));
    for pair in out.windows(2) {
        if pair[0].name == pair[1].name {
            bail!(
                "two inputs map to the same name `{}`: `{}` and `{}`",
                pair[0].name,
                pair[0].path.display(),
                pair[1].path.display()
            );
        }
    }

    Ok(out)
}

/// Recursively collect the files under `dir`, prefixing each relative name
/// with `prefix` (the path built so far, starting at the root folder name).
fn walk_dir(dir: &Path, prefix: &str, out: &mut Vec<InputFile>) -> Result<()> {
    let entries =
        fs::read_dir(dir).with_context(|| format!("reading directory `{}`", dir.display()))?;

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(e) => {
                eprintln!(
                    "warning: skipping unreadable entry in `{}`: {e}",
                    dir.display()
                );
                continue;
            }
        };

        let raw_name = entry.file_name();
        let name = match raw_name.to_str() {
            Some(s) => s,
            None => {
                eprintln!("warning: skipping non-UTF-8 name in `{}`", dir.display());
                continue;
            }
        };
        let file_type = match entry.file_type() {
            Ok(t) => t,
            Err(e) => {
                eprintln!("warning: skipping `{}`: {e}", entry.path().display());
                continue;
            }
        };
        if file_type.is_symlink() {
            eprintln!("warning: skipping symlink `{}`", entry.path().display());
            continue;
        }

        let rel = format!("{prefix}/{name}");
        if file_type.is_dir() {
            walk_dir(&entry.path(), &rel, out)?;
        } else if file_type.is_file() {
            out.push(InputFile {
                path: entry.path(),
                name: rel,
            });
        }
    }

    Ok(())
}

/// The final path component of `path` as a UTF-8 string.
fn base_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("invalid path: `{}`", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// Create a unique temp directory for one test.
    fn temp_dir() -> PathBuf {
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("pesto_walk_{}_{n}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn touch(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"x").unwrap();
    }

    #[test]
    fn plain_files_keep_their_base_name() {
        let dir = temp_dir();
        let a = dir.join("a.bin");
        let b = dir.join("b.bin");
        touch(&a);
        touch(&b);

        let out = expand_inputs(&[b.clone(), a.clone()]).unwrap();
        assert_eq!(out.len(), 2);
        // Sorted by name regardless of argument order.
        assert_eq!(out[0].name, "a.bin");
        assert_eq!(out[1].name, "b.bin");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn directory_is_walked_recursively_with_relative_names() {
        let dir = temp_dir();
        let season = dir.join("Season 01");
        touch(&season.join("ep01.mkv"));
        touch(&season.join("ep02.mkv"));
        touch(&season.join("extras/behind.mkv"));

        let out = expand_inputs(std::slice::from_ref(&season)).unwrap();
        let names: Vec<&str> = out.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "Season 01/ep01.mkv",
                "Season 01/ep02.mkv",
                "Season 01/extras/behind.mkv"
            ]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn hidden_entries_are_included() {
        let dir = temp_dir();
        let root = dir.join("show");
        touch(&root.join("ep01.mkv"));
        touch(&root.join(".hidden.nfo"));
        touch(&root.join(".meta/info.txt"));

        let out = expand_inputs(std::slice::from_ref(&root)).unwrap();
        let names: Vec<&str> = out.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(
            names,
            ["show/.hidden.nfo", "show/.meta/info.txt", "show/ep01.mkv"]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_directory_is_rejected() {
        let dir = temp_dir();
        let root = dir.join("empty");
        fs::create_dir_all(&root).unwrap();

        assert!(expand_inputs(std::slice::from_ref(&root)).is_err());

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_path_is_an_error() {
        let dir = temp_dir();
        let missing = dir.join("nope.bin");
        assert!(expand_inputs(&[missing]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn colliding_names_are_rejected() {
        let dir = temp_dir();
        let a = dir.join("one/movie.mkv");
        let b = dir.join("two/movie.mkv");
        touch(&a);
        touch(&b);
        // Two files given directly, both with base name `movie.mkv`.
        assert!(expand_inputs(&[a, b]).is_err());
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn symlinks_inside_directory_are_skipped() {
        let dir = temp_dir();
        let root = dir.join("show");
        touch(&root.join("ep01.mkv"));

        // Create a symlink inside the directory; it should be skipped.
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("ep01.mkv"), root.join("link.mkv")).unwrap();
        }
        #[cfg(not(unix))]
        {
            // On non-Unix, skip the symlink part of this test.
            fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let out = expand_inputs(std::slice::from_ref(&root)).unwrap();
        // Only the real file; the symlink is silently skipped.
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].name, "show/ep01.mkv");

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_paths_list_is_an_error() {
        assert!(expand_inputs(&[]).is_err());
    }

    #[test]
    fn output_is_sorted_by_name() {
        let dir = temp_dir();
        touch(&dir.join("z.bin"));
        touch(&dir.join("a.bin"));
        touch(&dir.join("m.bin"));

        let out =
            expand_inputs(&[dir.join("z.bin"), dir.join("a.bin"), dir.join("m.bin")]).unwrap();
        let names: Vec<&str> = out.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(names, ["a.bin", "m.bin", "z.bin"]);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn output_is_sorted_naturally_not_lexicographically() {
        let dir = temp_dir();
        for n in ["1", "2", "10", "12"] {
            touch(&dir.join(format!("ep{n}.mkv")));
        }

        let out = expand_inputs(std::slice::from_ref(&dir)).unwrap();
        let names: Vec<&str> = out.iter().map(|f| f.name.as_str()).collect();
        let root = dir.file_name().unwrap().to_string_lossy();
        assert_eq!(
            names,
            [
                format!("{root}/ep1.mkv"),
                format!("{root}/ep2.mkv"),
                format!("{root}/ep10.mkv"),
                format!("{root}/ep12.mkv"),
            ]
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    // ── natural_cmp ──────────────────────────────────────────────────────────

    fn natural_sorted(names: &[&str]) -> Vec<String> {
        let mut v: Vec<String> = names.iter().map(|s| s.to_string()).collect();
        v.sort_by(|a, b| natural_cmp(a, b));
        v
    }

    #[test]
    fn natural_cmp_orders_unpadded_volume_numbers_numerically() {
        assert_eq!(
            natural_sorted(&[
                "release.part10.rar",
                "release.part2.rar",
                "release.part1.rar",
                "release.part12.rar",
            ]),
            vec![
                "release.part1.rar",
                "release.part2.rar",
                "release.part10.rar",
                "release.part12.rar",
            ]
        );
    }

    #[test]
    fn natural_cmp_orders_padded_and_7z_volume_numbers() {
        assert_eq!(
            natural_sorted(&["a.7z.010", "a.7z.002", "a.7z.001"]),
            vec!["a.7z.001", "a.7z.002", "a.7z.010"]
        );
        assert_eq!(
            natural_sorted(&["r.part003.rar", "r.part001.rar", "r.part002.rar"]),
            vec!["r.part001.rar", "r.part002.rar", "r.part003.rar"]
        );
    }

    #[test]
    fn natural_cmp_falls_back_to_byte_order_outside_digit_runs() {
        assert_eq!(
            natural_sorted(&["s01/ep10.mkv", "s01/ep2.mkv", "s01/ep1.mkv", "s01/a.nfo"]),
            vec!["s01/a.nfo", "s01/ep1.mkv", "s01/ep2.mkv", "s01/ep10.mkv"]
        );
    }

    #[test]
    fn natural_cmp_is_a_total_order_across_equal_valued_padding() {
        // Same numeric value, different padding: whichever way the tie breaks,
        // it must break — two distinct names comparing Equal would make the
        // sort order depend on directory-listing order, and with it the PAR2
        // set and every wire name derived from the file's position.
        assert_eq!(natural_cmp("p1.rar", "p1.rar"), std::cmp::Ordering::Equal);
        assert_ne!(
            natural_cmp("p01.rar", "p1.rar"),
            std::cmp::Ordering::Equal,
            "distinct names must never compare Equal"
        );
        assert_eq!(
            natural_cmp("p01.rar", "p1.rar").reverse(),
            natural_cmp("p1.rar", "p01.rar"),
            "the comparator must be antisymmetric"
        );
    }

    #[test]
    fn natural_cmp_matches_the_comparator_each_orders_entries_with() {
        // `top_level_entries` (`--each`/`--season`) sorts with this same
        // function; if the two ever diverged, a season's entry order and a
        // release's internal file order would disagree.
        for (a, b) in [
            ("ep1.mkv", "ep10.mkv"),
            ("B.txt", "b.txt"),
            ("Show/ep2.bin", "Show/ep11.bin"),
        ] {
            assert_eq!(natural_cmp(a, b), lexical_sort::natural_lexical_cmp(a, b));
        }
    }
}
