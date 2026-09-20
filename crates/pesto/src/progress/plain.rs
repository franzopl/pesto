//! Plain-text upload presentation used by the CLI.

/// Human-readable byte size with binary (IEC) units.
pub fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Print a `tree`-style breakdown of the upload payload to stderr.
/// Upload settings shown below the file tree before posting starts.
pub struct UploadFlags<'a> {
    pub obfuscate: &'a str,
    /// `Some(fmt)` when compression is enabled, e.g. `"7z"`.
    pub compress: Option<&'a str>,
    /// `Some(pw)` when an archive password was set.
    pub password: Option<&'a str>,
    pub par2: u8,
    pub resume: bool,
    pub check: bool,
}

/// Print a compact settings block after the file tree.
///
/// Only non-default lines are shown so the output stays noise-free for the
/// common case (no obfuscation, no compression, par2=10, resume=off).
pub fn print_upload_flags(flags: &UploadFlags<'_>) {
    let mut lines: Vec<(&str, String)> = Vec::new();

    if flags.obfuscate != "none" {
        lines.push(("obfuscate", flags.obfuscate.to_string()));
    }
    if let Some(fmt) = flags.compress {
        lines.push(("compress", fmt.to_string()));
    }
    if let Some(pw) = flags.password {
        let preview = if pw.len() > 6 {
            format!("{}…", &pw[..6])
        } else {
            "*".repeat(pw.len())
        };
        lines.push(("password", preview));
    }
    if flags.par2 != 10 {
        lines.push(("par2", format!("{}%", flags.par2)));
    }
    if flags.resume {
        lines.push(("resume", "on".to_string()));
    }
    if !flags.check {
        lines.push(("check", "off".to_string()));
    }

    if lines.is_empty() {
        return;
    }

    let col = lines.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, val) in &lines {
        eprintln!("  {:<width$}  {}", key, val, width = col);
    }
    eprintln!();
}

pub fn print_tree(files: &[crate::walk::InputFile]) {
    use std::collections::BTreeMap;

    if files.is_empty() {
        return;
    }

    struct Leaf {
        filename: String,
        size: u64,
    }
    let mut tree: BTreeMap<String, Vec<Leaf>> = BTreeMap::new();
    let mut total_bytes: u64 = 0;

    for f in files {
        let size = std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0);
        total_bytes += size;
        let parts: Vec<&str> = f.name.splitn(2, '/').collect();
        let (dir, filename) = if parts.len() == 2 {
            (parts[0].to_string(), parts[1].to_string())
        } else {
            (String::new(), f.name.clone())
        };
        tree.entry(dir).or_default().push(Leaf { filename, size });
    }

    let dirs: Vec<_> = tree.keys().cloned().collect();
    let dir_count = dirs.len();

    for (di, dir) in dirs.iter().enumerate() {
        let leaves = &tree[dir];
        let is_last_dir = di == dir_count - 1;
        let dir_connector = if is_last_dir {
            "└──"
        } else {
            "├──"
        };

        if dir.is_empty() {
            for (li, leaf) in leaves.iter().enumerate() {
                let is_last = li == leaves.len() - 1;
                let conn = if is_last { "└──" } else { "├──" };
                eprintln!("{conn} {} ({})", leaf.filename, format_size(leaf.size));
            }
        } else {
            eprintln!("{dir_connector} {dir}/");
            let prefix = if is_last_dir { "    " } else { "│   " };
            for (li, leaf) in leaves.iter().enumerate() {
                let is_last = li == leaves.len() - 1;
                let conn = if is_last { "└──" } else { "├──" };
                eprintln!(
                    "{prefix}{conn} {} ({})",
                    leaf.filename,
                    format_size(leaf.size)
                );
            }
        }
    }

    eprintln!();
    eprintln!(
        "  {} file{} · {}",
        files.len(),
        if files.len() == 1 { "" } else { "s" },
        format_size(total_bytes)
    );
    eprintln!();
}
