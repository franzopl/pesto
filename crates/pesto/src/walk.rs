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
//!   are included unless they match the OS/FUSE metadata exclusions.
//!   Explicit file arguments bypass exclusions; custom globs and an opt-out
//!   are available through [`expand_inputs_with_options`].
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

/// OS/FUSE metadata names excluded at every depth by default.
pub const DEFAULT_EXCLUDES: &[&str] = &[
    ".DS_Store",
    "._*",
    ".fuse_hidden*",
    ".Spotlight-V100",
    ".Trashes",
    ".fseventsd",
    "Thumbs.db",
    "ehthumbs.db",
    "desktop.ini",
    "@eaDir",
];

/// Compiled directory-entry exclusions. Construct once per traversal.
///
/// Matching is case-sensitive. Patterns without `/` match basenames at any
/// depth; patterns with `/` match paths relative to the directory argument,
/// using `/` separators. `*` does not cross separators; `**` does.
#[derive(Debug)]
pub struct Exclusions {
    patterns: Vec<glob::Pattern>,
    root: Option<PathBuf>,
    warned: std::sync::Mutex<std::collections::HashSet<PathBuf>>,
}

impl Exclusions {
    /// Add custom patterns to the defaults, or disable all exclusions.
    /// Disabled exclusions ignore even invalid custom patterns.
    pub fn new(exclude: &[String], no_exclude: bool) -> Result<Self> {
        let patterns = if no_exclude {
            Vec::new()
        } else {
            DEFAULT_EXCLUDES
                .iter()
                .copied()
                .chain(exclude.iter().map(String::as_str))
                .map(|pattern| {
                    glob::Pattern::new(pattern)
                        .with_context(|| format!("invalid exclusion glob `{pattern}`"))
                })
                .collect::<Result<Vec<_>>>()?
        };
        Ok(Self {
            patterns,
            root: None,
            warned: Default::default(),
        })
    }

    /// Keep path globs relative to the original discovery root when splitting uploads.
    pub fn with_root(mut self, root: &Path) -> Self {
        self.root = Some(root.to_path_buf());
        self
    }

    /// Match a discovered entry, warning at most once per path for this traversal.
    pub fn excludes_entry(&self, path: &Path, name: &str, relative_path: &str) -> bool {
        let scoped_path = self
            .root
            .as_ref()
            .and_then(|root| path.strip_prefix(root).ok())
            .map(|path| {
                path.iter()
                    .map(|part| part.to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/")
            });
        if !self.matches(name, scoped_path.as_deref().unwrap_or(relative_path)) {
            return false;
        }
        let mut warned = self.warned.lock().unwrap_or_else(|err| err.into_inner());
        if warned.insert(path.to_path_buf()) {
            eprintln!("warning: excluding `{}`", path.display());
        }
        true
    }

    /// Test an entry name and its path relative to the traversal root.
    pub fn matches(&self, name: &str, relative_path: &str) -> bool {
        let options = glob::MatchOptions {
            case_sensitive: true,
            require_literal_separator: true,
            require_literal_leading_dot: false,
        };
        self.patterns.iter().any(|pattern| {
            pattern.matches_with(
                if pattern.as_str().contains('/') {
                    relative_path
                } else {
                    name
                },
                options,
            )
        })
    }
}

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

/// Whether `path`'s extension is one of `ext_filter` (case-insensitive). An
/// empty `ext_filter` matches everything (the `--ext` default: no filtering).
pub fn matches_ext_filter(path: &Path, ext_filter: &[String]) -> bool {
    if ext_filter.is_empty() {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ext_filter
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(ext))
        })
}

/// Apply `--ext` to an already-expanded input list, in place. A no-op when
/// `ext_filter` is empty. Errors out if the filter drops every input, so a
/// mistyped extension (or an entry that is 100% subtitles/extras) fails
/// loudly instead of silently posting nothing.
pub fn apply_ext_filter(
    inputs: &mut Vec<InputFile>,
    ext_filter: &[String],
    entry_label: &str,
) -> Result<()> {
    if ext_filter.is_empty() {
        return Ok(());
    }
    inputs.retain(|f| matches_ext_filter(&f.path, ext_filter));
    if inputs.is_empty() {
        anyhow::bail!(
            "no files matching --ext {} found in `{entry_label}`",
            ext_filter.join(",")
        );
    }
    Ok(())
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
    expand_inputs_with_options(paths, &[], false)
}

/// Expand inputs with additional exclusion globs or a complete opt-out.
/// Direct file arguments always bypass exclusions.
pub fn expand_inputs_with_options(
    paths: &[PathBuf],
    exclude: &[String],
    no_exclude: bool,
) -> Result<Vec<InputFile>> {
    expand_inputs_inner(paths, Exclusions::new(exclude, no_exclude)?, false)
}

/// Expand entries discovered under `root`, retaining that root for path globs.
/// Unlike explicit arguments, discovered file and directory entries are filtered.
pub fn expand_inputs_from_root(
    paths: &[PathBuf],
    root: &Path,
    exclude: &[String],
    no_exclude: bool,
) -> Result<Vec<InputFile>> {
    expand_inputs_inner(
        paths,
        Exclusions::new(exclude, no_exclude)?.with_root(root),
        true,
    )
}

fn expand_inputs_inner(
    paths: &[PathBuf],
    exclusions: Exclusions,
    discovered: bool,
) -> Result<Vec<InputFile>> {
    if paths.is_empty() {
        bail!("no files or directories given");
    }

    let mut out = Vec::new();
    for path in paths {
        if discovered {
            let name = base_name(path)?;
            if exclusions.excludes_entry(path, &name, &name) {
                continue;
            }
        }
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
            walk_dir(path, &root, "", &exclusions, &mut out)?;
        } else {
            bail!("`{}` is neither a file nor a directory", path.display());
        }
    }

    if out.is_empty() {
        bail!("no files to post: the given directories were empty or held only skipped entries");
    }

    for file in &mut out {
        file.name = sanitize_published_name(&file.name)?;
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
fn walk_dir(
    dir: &Path,
    prefix: &str,
    relative: &str,
    exclusions: &Exclusions,
    out: &mut Vec<InputFile>,
) -> Result<()> {
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
        let relative_path = if relative.is_empty() {
            name.to_string()
        } else {
            format!("{relative}/{name}")
        };
        if exclusions.excludes_entry(&entry.path(), name, &relative_path) {
            continue;
        }
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
            walk_dir(&entry.path(), &rel, &relative_path, exclusions, out)?;
        } else if file_type.is_file() {
            out.push(InputFile {
                path: entry.path(),
                name: rel,
            });
        }
    }

    Ok(())
}

/// Replace CR, LF, NUL and other C0 controls in a published name so it can
/// never become an NNTP header, a `=ybegin name=` line or raw NZB text.
/// Rejects a name that would be empty after sanitization.
pub fn sanitize_published_name(name: &str) -> Result<String> {
    let out: String = name
        .chars()
        .map(|c| if c.is_control() { '_' } else { c })
        .collect();
    if out.is_empty() {
        bail!("published file name is empty after sanitizing control characters");
    }
    Ok(out)
}

/// The final path component of `path` as a UTF-8 string.
fn base_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("invalid path: `{}`", path.display()))
}

#[cfg(test)]
mod tests;
