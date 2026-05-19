//! Throughput benchmark for the streaming PAR2 Reed-Solomon encoder.
//!
//! Run with:
//!   cargo bench --features bench-internals
//!
//! Two numbers are reported per row:
//!   - **input MiB/s** — source data throughput; bounds a real posting run when
//!     the CPU (not the network) is the bottleneck.
//!   - **GF madd GiB/s** — effective GF(2^16) multiply-add rate, i.e.
//!     `input_bytes × recovery_count / time`. This is the implementation-level
//!     metric comparable to other PAR2 creators (e.g. parpar) and is independent
//!     of the chosen redundancy.
//!
//! Each available SIMD path is benchmarked with the same workload so the
//! relative speedup of GFNI vs AVX2 vs SSSE3 vs scalar can be measured on
//! the same machine.

use std::time::{Duration, Instant};

use pesto::par2::encoder::{BenchPath, RecoveryEncoder};

const SLICE_SIZE: usize = 768_000;
const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = MIB * 1024.0;
/// Minimum wall-clock time per measurement to reduce noise.
const MIN_DURATION: Duration = Duration::from_secs(2);

fn make_slice(seed: u64) -> Vec<u8> {
    let mut s = vec![0u8; SLICE_SIZE];
    let mut x = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for chunk in s.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    s
}

/// Run the encoder with `path` for at least `MIN_DURATION` and return
/// (input_mib_per_s, gf_madd_gib_per_s).
fn measure(
    input_mib: usize,
    redundancy_pct: usize,
    path: BenchPath,
) -> (f64, f64) {
    let input_bytes = input_mib * 1024 * 1024;
    let total_slices = input_bytes.div_ceil(SLICE_SIZE);
    let recovery_count = (total_slices * redundancy_pct) / 100;

    let slices: Vec<Vec<u8>> = (0..total_slices as u64).map(make_slice).collect();

    let mut iters = 0u32;
    let mut total_elapsed = Duration::ZERO;

    loop {
        let start = Instant::now();
        let mut enc = RecoveryEncoder::new(SLICE_SIZE, total_slices, 0, recovery_count)
            .with_forced_path(path);
        for slice in slices.iter().cloned() {
            enc.add_slice(slice);
        }
        let (recovery, _) = enc.finish();
        assert_eq!(recovery.len(), recovery_count);
        total_elapsed += start.elapsed();
        iters += 1;

        if total_elapsed >= MIN_DURATION {
            break;
        }
    }

    let elapsed = total_elapsed.as_secs_f64() / iters as f64;
    let in_mib = (total_slices * SLICE_SIZE) as f64 / MIB;
    let madd_gib =
        (total_slices as f64 * recovery_count as f64 * SLICE_SIZE as f64) / GIB;
    (in_mib / elapsed, madd_gib / elapsed)
}

struct Scenario {
    label: &'static str,
    input_mib: usize,
    redundancy_pct: usize,
}

fn main() {
    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    // Detect which paths are available at runtime.
    #[cfg(target_arch = "x86_64")]
    let has_gfni = std::is_x86_feature_detected!("avx512f")
        && std::is_x86_feature_detected!("avx512bw")
        && std::is_x86_feature_detected!("gfni");
    #[cfg(not(target_arch = "x86_64"))]
    let has_gfni = false;

    #[cfg(target_arch = "x86_64")]
    let has_avx2 = std::is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let has_avx2 = false;

    #[cfg(target_arch = "x86_64")]
    let has_ssse3 = std::is_x86_feature_detected!("ssse3");
    #[cfg(not(target_arch = "x86_64"))]
    let has_ssse3 = false;

    println!("PAR2 encoder benchmark — slice {SLICE_SIZE} B — {threads} rayon thread(s)");
    println!(
        "SIMD available: GFNI+AVX512={} | AVX2={} | SSSE3={} | scalar=always",
        yn(has_gfni), yn(has_avx2), yn(has_ssse3),
    );
    println!();

    let scenarios = [
        Scenario { label: "64 MiB  @ 10%", input_mib: 64,  redundancy_pct: 10 },
        Scenario { label: "256 MiB @ 10%", input_mib: 256, redundancy_pct: 10 },
        Scenario { label: "256 MiB @ 20%", input_mib: 256, redundancy_pct: 20 },
        Scenario { label: "512 MiB @ 10%", input_mib: 512, redundancy_pct: 10 },
    ];

    // Table header
    println!(
        "{:<18}  {:>22}  {:>22}  {:>22}  {:>22}",
        "scenario", "GFNI+AVX512", "AVX2", "SSSE3", "scalar"
    );
    println!("{}", "-".repeat(114));

    #[cfg(target_arch = "x86_64")]
    let paths: &[(BenchPath, bool, &str)] = &[
        (BenchPath::Avx512Gfni, has_gfni, "GFNI+AVX512"),
        (BenchPath::Avx2,       has_avx2, "AVX2"),
        (BenchPath::Ssse3,      has_ssse3, "SSSE3"),
        (BenchPath::Scalar,     true,      "scalar"),
    ];
    #[cfg(not(target_arch = "x86_64"))]
    let paths: &[(BenchPath, bool, &str)] = &[
        (BenchPath::Scalar, true, "scalar"),
    ];

    for s in &scenarios {
        print!("{:<18}", s.label);
        for (path, available, _label) in paths {
            if *available {
                let (in_mib_s, gf_gib_s) = measure(s.input_mib, s.redundancy_pct, *path);
                print!("  {:>10.1} MiB/s", in_mib_s);
                // Show GF madd rate compactly
                let gf_str = if gf_gib_s >= 1.0 {
                    format!("{gf_gib_s:5.2} GiB/s")
                } else {
                    format!("{:5.0} MiB/s", gf_gib_s * 1024.0)
                };
                print!(" ({gf_str})");
            } else {
                print!("  {:>22}", "—");
            }
        }
        println!();
    }

    println!();

    // Speedup table vs scalar baseline (only makes sense when multiple paths available).
    #[cfg(target_arch = "x86_64")]
    if has_avx2 || has_ssse3 || has_gfni {
        println!("Speedup vs scalar (GF madd rate, 256 MiB @ 10%):");
        let (_, scalar_madd) =
            measure(256, 10, BenchPath::Scalar);
        for (path, available, label) in paths {
            if *available && *path != BenchPath::Scalar {
                let (_, path_madd) = measure(256, 10, *path);
                println!("  {label:<14} {:.2}×", path_madd / scalar_madd);
            }
        }
        println!();
    }
}

fn yn(b: bool) -> &'static str {
    if b { "yes" } else { "no" }
}
