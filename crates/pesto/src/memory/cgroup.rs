//! cgroup v1/v2 memory limits and PSI (pressure stall information), read
//! directly from `/sys/fs/cgroup` rather than through `sysinfo`.
//!
//! `sysinfo::System::cgroup_limits()` exists and is used elsewhere in this
//! crate (see `poster::producer`'s RAM-limit auto-detection), but it doesn't
//! expose PSI, and it doesn't let this module choose a different haircut per
//! source the way [`super::ceiling`] needs to. Both are cheap enough to read
//! directly: a handful of small files under `/sys/fs/cgroup`.

use std::fs;
use std::path::{Path, PathBuf};

/// This process's cgroup memory limit/usage/pressure, when confined by one.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CgroupMemory {
    /// `memory.max` (v2) / `memory.limit_in_bytes` (v1). `None` if the
    /// cgroup has no limit set (v2 `"max"`, v1's sentinel near-`u64::MAX`
    /// value) or the file couldn't be read.
    pub max: Option<u64>,
    /// `memory.current` (v2) / `memory.usage_in_bytes` (v1).
    pub current: Option<u64>,
    /// PSI `some avg10` for memory, as a percentage (0-100): the share of
    /// the last 10 s in which at least one task stalled on memory. Rises
    /// *before* the OOM killer engages, which is why it matters as an
    /// early-warning signal independent of the byte counters above. `None`
    /// if the kernel doesn't expose PSI (disabled at build time, or too old
    /// a kernel).
    pub psi_avg10: Option<f64>,
}

/// v1's "no limit" sentinel is a huge near-`u64::MAX` byte count (rounded to
/// a page boundary), not a distinct value like v2's `"max"` string. No real
/// host has anywhere near this much memory, so treat anything above it as
/// "unlimited" rather than a real ceiling.
const V1_UNLIMITED_THRESHOLD: u64 = 1 << 62;

/// Read this process's cgroup memory figures from the real filesystem.
pub fn read_cgroup_memory() -> Option<CgroupMemory> {
    let proc_self_cgroup = fs::read_to_string("/proc/self/cgroup").ok()?;
    read_cgroup_memory_at(
        &proc_self_cgroup,
        Path::new("/sys/fs/cgroup"),
        Path::new("/proc/pressure/memory"),
    )
}

/// Core logic, with every filesystem input injectable so tests can exercise
/// the v1/v2 branches and path resolution against synthetic fixtures instead
/// of the real `/proc` and `/sys/fs/cgroup` — including the system-wide PSI
/// fallback, whose real path can't be *absent* deterministically: most test
/// hosts (anything running a recent kernel with PSI compiled in) have it,
/// which would otherwise make this function's result depend on where the
/// tests happen to run.
fn read_cgroup_memory_at(
    proc_self_cgroup: &str,
    sys_fs_cgroup: &Path,
    system_psi_path: &Path,
) -> Option<CgroupMemory> {
    // The unified hierarchy mounts `cgroup.controllers` at its root; no such
    // file exists under a v1 (or hybrid, v1-for-memory) mount. This is the
    // standard way to tell the two apart without parsing mount options.
    let is_v2 = sys_fs_cgroup.join("cgroup.controllers").is_file();

    let (dir, max, current) = if is_v2 {
        let path = cgroup_v2_path(proc_self_cgroup)?;
        let dir = join_relative(sys_fs_cgroup, path);
        (
            dir.clone(),
            read_v2_optional_value(&dir.join("memory.max")),
            read_u64_file(&dir.join("memory.current")),
        )
    } else {
        let path = cgroup_v1_memory_path(proc_self_cgroup)?;
        let dir = join_relative(&sys_fs_cgroup.join("memory"), path);
        (
            dir.clone(),
            read_u64_file(&dir.join("memory.limit_in_bytes"))
                .filter(|&v| v < V1_UNLIMITED_THRESHOLD),
            read_u64_file(&dir.join("memory.usage_in_bytes")),
        )
    };

    let psi_avg10 = read_psi_avg10(&dir, system_psi_path);
    if max.is_none() && current.is_none() && psi_avg10.is_none() {
        return None;
    }
    Some(CgroupMemory {
        max,
        current,
        psi_avg10,
    })
}

/// `/proc/self/cgroup`'s v2 line is `0::<path>` — hierarchy id 0, an empty
/// (unified) controller list.
fn cgroup_v2_path(content: &str) -> Option<&str> {
    content.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next()?;
        let _controllers = fields.next()?;
        let path = fields.next()?;
        (hierarchy == "0").then_some(path)
    })
}

/// `/proc/self/cgroup`'s v1 lines are `<id>:<controllers>:<path>`, one per
/// hierarchy; find the one whose controller list includes `memory`.
fn cgroup_v1_memory_path(content: &str) -> Option<&str> {
    content.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let _hierarchy = fields.next()?;
        let controllers = fields.next()?;
        let path = fields.next()?;
        controllers
            .split(',')
            .any(|c| c == "memory")
            .then_some(path)
    })
}

/// `cgroup_path` from `/proc/self/cgroup` is always absolute (`/` for the
/// root cgroup); `Path::join` treats an absolute second argument as
/// *replacing* the base entirely rather than appending, so the leading `/`
/// must be stripped first.
fn join_relative(base: &Path, cgroup_path: &str) -> PathBuf {
    base.join(cgroup_path.trim_start_matches('/'))
}

fn read_u64_file(path: &Path) -> Option<u64> {
    fs::read_to_string(path).ok()?.trim().parse().ok()
}

/// v2's `memory.max` reads back the literal string `"max"` when unlimited,
/// not a number — that must be distinguished from a real value before
/// falling through to the same `None` a missing/unreadable file would give,
/// so callers can't tell "unlimited" and "unknown" apart, which is fine here
/// since both mean "don't let this source constrain the ceiling".
fn read_v2_optional_value(path: &Path) -> Option<u64> {
    let raw = fs::read_to_string(path).ok()?;
    let raw = raw.trim();
    if raw == "max" {
        None
    } else {
        raw.parse().ok()
    }
}

/// PSI `some avg10` for memory: the cgroup's own `memory.pressure` first
/// (v2, and v1 hosts with PSI mounted per-cgroup), falling back to the
/// system-wide `/proc/pressure/memory` (v1's usual case — PSI is exposed
/// globally even where it isn't per-cgroup).
fn read_psi_avg10(cgroup_dir: &Path, system_psi_path: &Path) -> Option<f64> {
    fs::read_to_string(cgroup_dir.join("memory.pressure"))
        .ok()
        .and_then(|c| parse_psi_avg10(&c))
        .or_else(|| {
            fs::read_to_string(system_psi_path)
                .ok()
                .and_then(|c| parse_psi_avg10(&c))
        })
}

/// Parse the `avg10=` field off PSI's `some` line, e.g.:
/// `some avg10=0.00 avg60=0.00 avg300=0.00 total=0`
fn parse_psi_avg10(content: &str) -> Option<f64> {
    let some_line = content.lines().find(|l| l.starts_with("some "))?;
    some_line
        .split_whitespace()
        .find_map(|tok| tok.strip_prefix("avg10="))?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn parse_psi_avg10_reads_the_some_line() {
        let content = "some avg10=12.34 avg60=5.00 avg300=1.00 total=999\nfull avg10=1.00 avg60=0.50 avg300=0.10 total=1\n";
        assert_eq!(parse_psi_avg10(content), Some(12.34));
    }

    #[test]
    fn parse_psi_avg10_handles_missing_or_malformed_content() {
        assert_eq!(parse_psi_avg10(""), None);
        assert_eq!(parse_psi_avg10("full avg10=1.00\n"), None);
        assert_eq!(parse_psi_avg10("some avg10=notanumber\n"), None);
    }

    #[test]
    fn cgroup_v2_path_finds_the_zero_hierarchy_line() {
        let content = "0::/user.slice/user-1000.slice/session.scope\n";
        assert_eq!(
            cgroup_v2_path(content),
            Some("/user.slice/user-1000.slice/session.scope")
        );
    }

    #[test]
    fn cgroup_v1_memory_path_finds_the_memory_controller_line() {
        let content = "11:cpu,cpuacct:/\n8:memory:/user.slice/user-1000.slice\n1:name=systemd:/\n";
        assert_eq!(
            cgroup_v1_memory_path(content),
            Some("/user.slice/user-1000.slice")
        );
    }

    #[test]
    fn join_relative_strips_the_leading_slash_so_the_base_survives() {
        let base = Path::new("/sys/fs/cgroup");
        assert_eq!(
            join_relative(base, "/user.slice/foo"),
            base.join("user.slice/foo")
        );
        // Root cgroup ("/") must resolve to the base itself, not discard it.
        assert_eq!(join_relative(base, "/"), base);
    }

    #[test]
    fn v2_fixture_reads_max_and_current() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("cgroup.controllers"), "memory cpu io\n").unwrap();
        let cgdir = root.join("user.slice");
        fs::create_dir_all(&cgdir).unwrap();
        fs::write(cgdir.join("memory.max"), "2147483648\n").unwrap();
        fs::write(cgdir.join("memory.current"), "1073741824\n").unwrap();

        let proc_self_cgroup = "0::/user.slice\n";
        let result = read_cgroup_memory_at(
            proc_self_cgroup,
            root,
            // Deliberately nonexistent: the system-wide PSI fallback must
            // stay off for these fixture-driven assertions to be
            // deterministic across test hosts (most real hosts *do* have
            // `/proc/pressure/memory`).
            Path::new("/nonexistent-for-pesto-tests/memory.pressure"),
        )
        .unwrap();
        assert_eq!(result.max, Some(2147483648));
        assert_eq!(result.current, Some(1073741824));
    }

    #[test]
    fn v2_fixture_treats_max_string_as_unlimited() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("cgroup.controllers"), "memory\n").unwrap();
        let cgdir = root.join("user.slice");
        fs::create_dir_all(&cgdir).unwrap();
        fs::write(cgdir.join("memory.max"), "max\n").unwrap();
        fs::write(cgdir.join("memory.current"), "123456\n").unwrap();

        let proc_self_cgroup = "0::/user.slice\n";
        let result = read_cgroup_memory_at(
            proc_self_cgroup,
            root,
            // Deliberately nonexistent: the system-wide PSI fallback must
            // stay off for these fixture-driven assertions to be
            // deterministic across test hosts (most real hosts *do* have
            // `/proc/pressure/memory`).
            Path::new("/nonexistent-for-pesto-tests/memory.pressure"),
        )
        .unwrap();
        assert_eq!(result.max, None);
        assert_eq!(result.current, Some(123456));
    }

    #[test]
    fn v1_fixture_reads_limit_and_usage_from_the_memory_subdir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // No cgroup.controllers file here -> detected as v1.
        let cgdir = root.join("memory").join("user.slice");
        fs::create_dir_all(&cgdir).unwrap();
        fs::write(cgdir.join("memory.limit_in_bytes"), "4294967296\n").unwrap();
        fs::write(cgdir.join("memory.usage_in_bytes"), "2147483648\n").unwrap();

        let proc_self_cgroup = "11:cpu,cpuacct:/\n8:memory:/user.slice\n1:name=systemd:/\n";
        let result = read_cgroup_memory_at(
            proc_self_cgroup,
            root,
            // Deliberately nonexistent: the system-wide PSI fallback must
            // stay off for these fixture-driven assertions to be
            // deterministic across test hosts (most real hosts *do* have
            // `/proc/pressure/memory`).
            Path::new("/nonexistent-for-pesto-tests/memory.pressure"),
        )
        .unwrap();
        assert_eq!(result.max, Some(4294967296));
        assert_eq!(result.current, Some(2147483648));
    }

    #[test]
    fn v1_fixture_treats_the_near_u64_max_sentinel_as_unlimited() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let cgdir = root.join("memory").join("user.slice");
        fs::create_dir_all(&cgdir).unwrap();
        fs::write(cgdir.join("memory.limit_in_bytes"), "9223372036854771712\n").unwrap();
        fs::write(cgdir.join("memory.usage_in_bytes"), "100\n").unwrap();

        let proc_self_cgroup = "8:memory:/user.slice\n";
        let result = read_cgroup_memory_at(
            proc_self_cgroup,
            root,
            // Deliberately nonexistent: the system-wide PSI fallback must
            // stay off for these fixture-driven assertions to be
            // deterministic across test hosts (most real hosts *do* have
            // `/proc/pressure/memory`).
            Path::new("/nonexistent-for-pesto-tests/memory.pressure"),
        )
        .unwrap();
        assert_eq!(result.max, None);
        assert_eq!(result.current, Some(100));
    }

    #[test]
    fn missing_files_and_no_psi_yields_none_overall() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        fs::write(root.join("cgroup.controllers"), "memory\n").unwrap();
        // The cgroup directory itself is never created: every read fails.
        let proc_self_cgroup = "0::/nonexistent\n";
        assert_eq!(
            read_cgroup_memory_at(
                proc_self_cgroup,
                root,
                // Deliberately nonexistent: the system-wide PSI fallback must
                // stay off for these fixture-driven assertions to be
                // deterministic across test hosts (most real hosts *do* have
                // `/proc/pressure/memory`).
                Path::new("/nonexistent-for-pesto-tests/memory.pressure"),
            ),
            None
        );
    }

    #[test]
    fn no_matching_hierarchy_line_yields_none() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        // v1 detection, but no line names the memory controller.
        let proc_self_cgroup = "11:cpu,cpuacct:/\n1:name=systemd:/\n";
        assert_eq!(
            read_cgroup_memory_at(
                proc_self_cgroup,
                root,
                // Deliberately nonexistent: the system-wide PSI fallback must
                // stay off for these fixture-driven assertions to be
                // deterministic across test hosts (most real hosts *do* have
                // `/proc/pressure/memory`).
                Path::new("/nonexistent-for-pesto-tests/memory.pressure"),
            ),
            None
        );
    }
}
