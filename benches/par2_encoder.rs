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

/// Run the ALTMAP encoder for at least `MIN_DURATION` and return
/// (input_mib_per_s, gf_madd_gib_per_s).  Uses `new_altmap` so the entire
/// path (transpose + vpxor kernel + from_altmap) is exercised.
fn measure_altmap(input_mib: usize, redundancy_pct: usize) -> (f64, f64) {
    let input_bytes = input_mib * 1024 * 1024;
    let total_slices = input_bytes.div_ceil(SLICE_SIZE);
    let recovery_count = (total_slices * redundancy_pct) / 100;

    let slices: Vec<Vec<u8>> = (0..total_slices as u64).map(make_slice).collect();

    let mut iters = 0u32;
    let mut total_elapsed = Duration::ZERO;

    loop {
        let start = Instant::now();
        let mut enc = RecoveryEncoder::new_altmap(SLICE_SIZE, total_slices, 0, recovery_count);
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
    let madd_gib = (total_slices as f64 * recovery_count as f64 * SLICE_SIZE as f64) / GIB;
    (in_mib / elapsed, madd_gib / elapsed)
}

/// Run the Shuffle2x encoder for at least `MIN_DURATION` and return
/// (input_mib_per_s, gf_madd_gib_per_s).  Uses `new_shuffle2x` so the entire
/// path (kernel + from_shuffle2x conversion) is exercised.
fn measure_shuffle2x(input_mib: usize, redundancy_pct: usize) -> (f64, f64) {
    let input_bytes = input_mib * 1024 * 1024;
    let total_slices = input_bytes.div_ceil(SLICE_SIZE);
    let recovery_count = (total_slices * redundancy_pct) / 100;

    let slices: Vec<Vec<u8>> = (0..total_slices as u64).map(make_slice).collect();

    let mut iters = 0u32;
    let mut total_elapsed = Duration::ZERO;

    loop {
        let start = Instant::now();
        let mut enc = RecoveryEncoder::new_shuffle2x(SLICE_SIZE, total_slices, 0, recovery_count);
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
    let madd_gib = (total_slices as f64 * recovery_count as f64 * SLICE_SIZE as f64) / GIB;
    (in_mib / elapsed, madd_gib / elapsed)
}

/// Run the encoder with `path` for at least `MIN_DURATION` and return
/// (input_mib_per_s, gf_madd_gib_per_s).
fn measure(input_mib: usize, redundancy_pct: usize, path: BenchPath) -> (f64, f64) {
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
    let madd_gib = (total_slices as f64 * recovery_count as f64 * SLICE_SIZE as f64) / GIB;
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
    let has_gfni_512 = std::is_x86_feature_detected!("avx512f")
        && std::is_x86_feature_detected!("avx512bw")
        && std::is_x86_feature_detected!("gfni");
    #[cfg(not(target_arch = "x86_64"))]
    let has_gfni_512 = false;

    #[cfg(target_arch = "x86_64")]
    let has_gfni_256 =
        std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("gfni");
    #[cfg(not(target_arch = "x86_64"))]
    let has_gfni_256 = false;

    #[cfg(target_arch = "x86_64")]
    let has_avx2 = std::is_x86_feature_detected!("avx2");
    #[cfg(not(target_arch = "x86_64"))]
    let has_avx2 = false;

    #[cfg(target_arch = "x86_64")]
    let has_ssse3 = std::is_x86_feature_detected!("ssse3");
    #[cfg(not(target_arch = "x86_64"))]
    let has_ssse3 = false;

    #[cfg(target_arch = "x86_64")]
    let has_avx2_altmap =
        std::is_x86_feature_detected!("avx2") && !std::is_x86_feature_detected!("gfni");
    #[cfg(not(target_arch = "x86_64"))]
    let has_avx2_altmap = false;

    #[cfg(target_arch = "x86_64")]
    let has_avx2_shuffle2x =
        std::is_x86_feature_detected!("avx2") && !std::is_x86_feature_detected!("gfni");
    #[cfg(not(target_arch = "x86_64"))]
    let has_avx2_shuffle2x = false;

    println!("PAR2 encoder benchmark — slice {SLICE_SIZE} B — {threads} rayon thread(s)");
    println!(
        "SIMD available: GFNI+AVX512={} | GFNI+AVX2={} | AVX2={} | SSSE3={} | scalar=always",
        yn(has_gfni_512),
        yn(has_gfni_256),
        yn(has_avx2),
        yn(has_ssse3),
    );
    println!(
        "Special kernels: ALTMAP={} | Shuffle2x={} (AVX2 without GFNI)",
        yn(has_avx2_altmap),
        yn(has_avx2_shuffle2x)
    );
    println!();

    let scenarios = [
        Scenario {
            label: "64 MiB  @ 10%",
            input_mib: 64,
            redundancy_pct: 10,
        },
        Scenario {
            label: "256 MiB @ 10%",
            input_mib: 256,
            redundancy_pct: 10,
        },
        Scenario {
            label: "256 MiB @ 20%",
            input_mib: 256,
            redundancy_pct: 20,
        },
        Scenario {
            label: "512 MiB @ 10%",
            input_mib: 512,
            redundancy_pct: 10,
        },
    ];

    // Table header
    println!(
        "{:<18}  {:>22}  {:>22}  {:>22}  {:>22}  {:>22}  {:>22}  {:>22}",
        "scenario",
        "GFNI+AVX512",
        "GFNI+AVX2",
        "AVX2(Shuffle2x)",
        "AVX2",
        "SSSE3",
        "scalar",
        "AVX2(ALTMAP)"
    );
    println!("{}", "-".repeat(189));

    #[cfg(target_arch = "x86_64")]
    let paths: &[(BenchPath, bool, &str)] = &[
        (BenchPath::Avx512Gfni, has_gfni_512, "GFNI+AVX512"),
        (BenchPath::Avx2Gfni, has_gfni_256, "GFNI+AVX2"),
        (BenchPath::Avx2, has_avx2, "AVX2"),
        (BenchPath::Ssse3, has_ssse3, "SSSE3"),
        (BenchPath::Scalar, true, "scalar"),
    ];
    #[cfg(target_arch = "aarch64")]
    let paths: &[(BenchPath, bool, &str)] = &[
        (BenchPath::NeonClmul, true, "NEON-CLMUL"),
        (BenchPath::Scalar, true, "scalar"),
    ];
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let paths: &[(BenchPath, bool, &str)] = &[(BenchPath::Scalar, true, "scalar")];

    for s in &scenarios {
        print!("{:<18}", s.label);

        // On GFNI+AVX2 CPUs we skip both GFNI columns (handled in the paths loop)
        // and print "—" for the Shuffle2x/ALTMAP special-kernel columns.
        #[cfg(target_arch = "x86_64")]
        {
            // GFNI paths are printed by the paths loop below; nothing to skip here.
            // Shuffle2x column
            if has_avx2_shuffle2x {
                let (in_mib_s, gf_gib_s) = measure_shuffle2x(s.input_mib, s.redundancy_pct);
                let gf_str = if gf_gib_s >= 1.0 {
                    format!("{gf_gib_s:5.2} GiB/s")
                } else {
                    format!("{:5.0} MiB/s", gf_gib_s * 1024.0)
                };
                // print GFNI+AVX512 and GFNI+AVX2 dashes first
                print!("  {:>22}", "—");
                print!("  {:>22}", "—");
                print!("  {:>10.1} MiB/s ({gf_str})", in_mib_s);
            } else {
                // no shuffle2x here; columns will be printed by the paths loop
                // (or will be "—" if unavailable)
            }
        }

        for (path, available, _label) in paths {
            // On AVX2-only machines, skip GFNI paths (already printed "—" implicitly or above).
            #[cfg(target_arch = "x86_64")]
            if has_avx2_shuffle2x && matches!(path, BenchPath::Avx512Gfni | BenchPath::Avx2Gfni) {
                continue; // "—" already printed above
            }

            if *available {
                let (in_mib_s, gf_gib_s) = measure(s.input_mib, s.redundancy_pct, *path);
                let gf_str = if gf_gib_s >= 1.0 {
                    format!("{gf_gib_s:5.2} GiB/s")
                } else {
                    format!("{:5.0} MiB/s", gf_gib_s * 1024.0)
                };
                print!("  {:>10.1} MiB/s ({gf_str})", in_mib_s);
            } else {
                print!("  {:>22}", "—");
            }
        }

        // ALTMAP column at the end (for comparison reference).
        #[cfg(target_arch = "x86_64")]
        if has_avx2_altmap {
            let (in_mib_s, gf_gib_s) = measure_altmap(s.input_mib, s.redundancy_pct);
            let gf_str = if gf_gib_s >= 1.0 {
                format!("{gf_gib_s:5.2} GiB/s")
            } else {
                format!("{:5.0} MiB/s", gf_gib_s * 1024.0)
            };
            print!("  {:>10.1} MiB/s ({gf_str})", in_mib_s);
        } else {
            print!("  {:>22}", "—");
        }

        println!();
    }

    println!();

    // Speedup vs scalar and Shuffle2x vs AVX2 comparison.
    #[cfg(target_arch = "x86_64")]
    if has_avx2 || has_ssse3 || has_gfni_256 || has_gfni_512 || has_avx2_shuffle2x {
        println!("Speedup vs scalar (GF madd rate, 256 MiB @ 10%):");
        let (_, scalar_madd) = measure(256, 10, BenchPath::Scalar);
        if has_avx2_shuffle2x {
            let (_, s2x_madd) = measure_shuffle2x(256, 10);
            println!("  AVX2(Shuffle2x) {:.2}×", s2x_madd / scalar_madd);
        }
        for (path, available, label) in paths {
            if *available && *path != BenchPath::Scalar {
                let (_, path_madd) = measure(256, 10, *path);
                println!("  {label:<15} {:.2}×", path_madd / scalar_madd);
            }
        }

        // Direct Shuffle2x vs plain AVX2 comparison — the key 28d metric.
        #[cfg(target_arch = "x86_64")]
        if has_avx2_shuffle2x {
            println!();
            println!("Shuffle2x vs plain AVX2 (256 MiB @ 10%):");
            let (_, avx2_madd) = measure(256, 10, BenchPath::Avx2);
            let (_, s2x_madd) = measure_shuffle2x(256, 10);
            let ratio = s2x_madd / avx2_madd;
            let verdict = if ratio >= 1.20 {
                "PASS ≥ 20 %"
            } else if ratio >= 1.0 {
                "marginal (< 20 %)"
            } else {
                "REGRESS"
            };
            println!("  Shuffle2x/AVX2 = {ratio:.3}×  [{verdict}]");
        }
        println!();
    }
}

fn yn(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}
