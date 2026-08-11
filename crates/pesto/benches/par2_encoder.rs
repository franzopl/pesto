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

use pesto::par2::encoder::{
    altmap_kernel_available, shuffle2x_kernel_available, BenchPath, RecoveryEncoder,
};

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
///
/// Only meaningful where `altmap_kernel_available()` — elsewhere `new_altmap`
/// falls back to the portable layout and this would time that instead, under
/// the ALTMAP heading.
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
///
/// Only meaningful where `shuffle2x_kernel_available()`; see `measure_altmap`.
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

/// What a table column measures.  `Shuffle2x` and `Altmap` are layout
/// constructors rather than forced paths, so they need their own runner.
#[derive(Clone, Copy, PartialEq)]
// Both specialized layouts are x86_64-only, so no column builds them elsewhere.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
enum Kernel {
    Path(BenchPath),
    Shuffle2x,
    Altmap,
}

/// One table column: a heading, whether the kernel behind it actually runs on
/// this CPU, and how to time it.
///
/// `available` is what keeps the table honest. A column whose kernel is absent
/// prints `—` instead of a number: the specialized constructors fall back to the
/// portable layout when their kernel is missing, so timing them anyway would
/// report the portable kernel's throughput under the ALTMAP or Shuffle2x
/// heading. (Before the fallback existed it was worse — the ALTMAP row on GFNI
/// hardware timed a no-op that produced all-zero parity, and reported
/// 1696 MiB/s for it.)
struct Column {
    label: &'static str,
    available: bool,
    kernel: Kernel,
}

/// Column width for every measured cell, and for its heading. Wide enough for
/// `"    1788.9 MiB/s (122.29 GiB/s)"`, the widest cell `cell()` can produce
/// before the GF rate needs four integer digits.
const COL_WIDTH: usize = 31;

fn measure_kernel(kernel: Kernel, input_mib: usize, redundancy_pct: usize) -> (f64, f64) {
    match kernel {
        Kernel::Path(p) => measure(input_mib, redundancy_pct, p),
        Kernel::Shuffle2x => measure_shuffle2x(input_mib, redundancy_pct),
        Kernel::Altmap => measure_altmap(input_mib, redundancy_pct),
    }
}

/// `"  3612.4 MiB/s ( 2.54 GiB/s)"`, padded to `COL_WIDTH`.
fn cell(in_mib_s: f64, gf_gib_s: f64) -> String {
    let gf_str = if gf_gib_s >= 1.0 {
        format!("{gf_gib_s:5.2} GiB/s")
    } else {
        format!("{:5.0} MiB/s", gf_gib_s * 1024.0)
    };
    format!("{in_mib_s:>10.1} MiB/s ({gf_str})")
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

    // Ask the encoder itself which specialized kernels are live rather than
    // re-deriving it from CPU flags here: `new_altmap`/`new_shuffle2x` silently
    // fall back to the portable layout when theirs is missing, and only the
    // encoder knows the exact condition (ALTMAP needs AVX2 *without* GFNI;
    // Shuffle2x runs on GFNI hardware too).
    let has_avx2_altmap = altmap_kernel_available();
    let has_avx2_shuffle2x = shuffle2x_kernel_available();

    println!("PAR2 encoder benchmark — slice {SLICE_SIZE} B — {threads} rayon thread(s)");
    println!(
        "SIMD available: GFNI+AVX512={} | GFNI+AVX2={} | AVX2={} | SSSE3={} | scalar=always",
        yn(has_gfni_512),
        yn(has_gfni_256),
        yn(has_avx2),
        yn(has_ssse3),
    );
    println!(
        "Special kernels: ALTMAP={} (AVX2 without GFNI) | Shuffle2x={} (AVX2)",
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

    #[cfg(target_arch = "x86_64")]
    let columns: &[Column] = &[
        Column {
            label: "GFNI+AVX512",
            available: has_gfni_512,
            kernel: Kernel::Path(BenchPath::Avx512Gfni),
        },
        Column {
            label: "GFNI+AVX2",
            available: has_gfni_256,
            kernel: Kernel::Path(BenchPath::Avx2Gfni),
        },
        Column {
            label: "AVX2(Shuffle2x)",
            available: has_avx2_shuffle2x,
            kernel: Kernel::Shuffle2x,
        },
        Column {
            label: "AVX2",
            available: has_avx2,
            kernel: Kernel::Path(BenchPath::Avx2),
        },
        Column {
            label: "SSSE3",
            available: has_ssse3,
            kernel: Kernel::Path(BenchPath::Ssse3),
        },
        Column {
            label: "scalar",
            available: true,
            kernel: Kernel::Path(BenchPath::Scalar),
        },
        Column {
            label: "AVX2(ALTMAP)",
            available: has_avx2_altmap,
            kernel: Kernel::Altmap,
        },
    ];
    #[cfg(target_arch = "aarch64")]
    let columns: &[Column] = &[
        Column {
            label: "NEON-CLMUL",
            available: true,
            kernel: Kernel::Path(BenchPath::NeonClmul),
        },
        Column {
            label: "scalar",
            available: true,
            kernel: Kernel::Path(BenchPath::Scalar),
        },
    ];
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    let columns: &[Column] = &[Column {
        label: "scalar",
        available: true,
        kernel: Kernel::Path(BenchPath::Scalar),
    }];

    // Table header, derived from the same column list the rows are, so a
    // skipped kernel can never shift the numbers under a neighbouring heading.
    print!("{:<18}", "scenario");
    for c in columns {
        print!("  {:>COL_WIDTH$}", c.label);
    }
    println!();
    println!("{}", "-".repeat(18 + columns.len() * (COL_WIDTH + 2)));

    for s in &scenarios {
        print!("{:<18}", s.label);
        for c in columns {
            if c.available {
                let (in_mib_s, gf_gib_s) = measure_kernel(c.kernel, s.input_mib, s.redundancy_pct);
                print!("  {:>COL_WIDTH$}", cell(in_mib_s, gf_gib_s));
            } else {
                print!("  {:>COL_WIDTH$}", "—");
            }
        }
        println!();
    }

    println!();

    // Speedup vs scalar, for every column that ran above.
    let scalar_column = Kernel::Path(BenchPath::Scalar);
    if columns
        .iter()
        .any(|c| c.available && c.kernel != scalar_column)
    {
        println!("Speedup vs scalar (GF madd rate, 256 MiB @ 10%):");
        let (_, scalar_madd) = measure(256, 10, BenchPath::Scalar);
        for c in columns {
            if c.available && c.kernel != scalar_column {
                let (_, madd) = measure_kernel(c.kernel, 256, 10);
                println!("  {:<15} {:.2}×", c.label, madd / scalar_madd);
            }
        }
        println!();
    }

    // Direct Shuffle2x vs plain AVX2 comparison — the key 28d metric.
    if has_avx2_shuffle2x && has_avx2 {
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
