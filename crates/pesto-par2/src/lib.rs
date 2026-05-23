pub mod altmap;
pub mod encoder;
pub mod gf16;
pub mod layout;
pub mod packet;
pub mod shuffle2x;
pub mod yenc;

pub use encoder::{altmap_buffer_size, shuffle2x_buffer_size};

/// Returns the name of the SIMD path that the PAR2 encoder will use at runtime.
pub fn detect_simd() -> &'static str {
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx512f")
        && std::is_x86_feature_detected!("avx512bw")
        && std::is_x86_feature_detected!("gfni")
    {
        return "AVX-512/GFNI";
    }
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        return "AVX2";
    }
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("ssse3") {
        return "SSSE3";
    }
    #[cfg(target_arch = "aarch64")]
    {
        return "NEON";
    }
    #[allow(unreachable_code)]
    "scalar"
}

/// Number of physical CPU cores, derived from `/proc/cpuinfo` by counting
/// distinct `(physical id, core id)` pairs. Falls back to the logical CPU
/// count when that information is unavailable.
pub fn physical_core_count() -> usize {
    #[cfg(target_os = "linux")]
    {
        use std::collections::HashSet;
        if let Ok(info) = std::fs::read_to_string("/proc/cpuinfo") {
            let mut seen: HashSet<(String, String)> = HashSet::new();
            let (mut phys, mut core): (Option<String>, Option<String>) = (None, None);
            for line in info.lines() {
                if line.trim().is_empty() {
                    if let (Some(p), Some(c)) = (phys.take(), core.take()) {
                        seen.insert((p, c));
                    }
                } else if let Some((key, val)) = line.split_once(':') {
                    match key.trim() {
                        "physical id" => phys = Some(val.trim().to_string()),
                        "core id" => core = Some(val.trim().to_string()),
                        _ => {}
                    }
                }
            }
            if !seen.is_empty() {
                return seen.len();
            }
        }
    }
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Number of performance-class cores. On hybrid CPUs (Intel 12th gen and later)
/// Detects hybrid layout via Linux topology: P-cores expose two
/// `thread_siblings_list` entries (HT pair), E-cores stand alone.
/// On hybrid CPUs (P + E mix) return all physical cores (paired leaders + solo),
/// one rayon thread per physical core. On non-hybrid CPUs fall back to
/// [`physical_core_count`]. Hyperthreads are always excluded — they contend for
/// the same execution ports and add noise on pure SIMD/ALU workloads.
pub fn performance_core_count() -> usize {
    #[cfg(target_os = "linux")]
    {
        use std::collections::HashSet;

        let Ok(entries) = std::fs::read_dir("/sys/devices/system/cpu") else {
            return physical_core_count();
        };

        let mut paired_leaders: HashSet<usize> = HashSet::new();
        let mut solo: HashSet<usize> = HashSet::new();

        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_s = name.to_string_lossy();
            let Some(cpu_num) = name_s
                .strip_prefix("cpu")
                .and_then(|s| s.parse::<usize>().ok())
            else {
                continue;
            };
            let sib_path = entry.path().join("topology/thread_siblings_list");
            let Ok(sib) = std::fs::read_to_string(sib_path) else {
                continue;
            };
            let leader = sib
                .trim()
                .split([',', '-'])
                .next()
                .and_then(|s| s.parse::<usize>().ok())
                .unwrap_or(cpu_num);
            let count = sib.trim().split([',', '-']).count();
            if count >= 2 {
                paired_leaders.insert(leader);
            } else {
                solo.insert(cpu_num);
            }
        }

        if !paired_leaders.is_empty() && !solo.is_empty() {
            // Hybrid CPU: include both P-cores and E-cores (all physical, no HT).
            return paired_leaders.len() + solo.len();
        }
    }
    physical_core_count()
}
