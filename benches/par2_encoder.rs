//! Throughput benchmark for the streaming PAR2 Reed-Solomon encoder.
//!
//! Run with `cargo bench`. This is a plain `harness = false` binary — no
//! `criterion` dependency — keeping the dependency tree small as required by
//! `CLAUDE.md`.
//!
//! Two numbers are reported per configuration:
//!
//! - **input MiB/s** — how fast source data is consumed. This is what bounds a
//!   real posting run when the CPU, not the network, is the limit.
//! - **GF madd GiB/s** — the effective GF(2^16) multiply-add rate, i.e.
//!   `input_bytes * recovery_count / time`. This is the implementation-level
//!   metric comparable to other PAR2 creators such as `parpar`, since it is
//!   independent of the chosen redundancy.

use std::time::Instant;

use pesto::par2::encoder::RecoveryEncoder;

/// Slice size used by the benchmark — the default article size.
const SLICE_SIZE: usize = 768_000;
const MIB: f64 = 1024.0 * 1024.0;
const GIB: f64 = MIB * 1024.0;

/// Build one pseudo-random input slice. A cheap xorshift keeps the data
/// non-trivial without pulling in an RNG crate.
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

fn bench(label: &str, input_mib: usize, redundancy_pct: usize) {
    let input_bytes = input_mib * 1024 * 1024;
    let total_slices = input_bytes.div_ceil(SLICE_SIZE);
    let recovery_count = (total_slices * redundancy_pct) / 100;

    // Pre-build the input slices so allocation noise stays out of the timing;
    // `add_slice` only moves the `Vec`, it does not copy.
    let slices: Vec<Vec<u8>> = (0..total_slices as u64).map(make_slice).collect();

    let start = Instant::now();
    let mut enc = RecoveryEncoder::new(SLICE_SIZE, total_slices, 0, recovery_count);
    for slice in slices {
        enc.add_slice(slice);
    }
    let recovery = enc.finish();
    let elapsed = start.elapsed().as_secs_f64();

    assert_eq!(recovery.len(), recovery_count);

    let in_mib = (total_slices * SLICE_SIZE) as f64 / MIB;
    let madd_gib = (total_slices as f64 * recovery_count as f64 * SLICE_SIZE as f64) / GIB;

    println!(
        "{label:<18} {recovery_count:4} rec | {:8.1} MiB/s in | {:6.2} GiB/s GF madd | {elapsed:7.3} s",
        in_mib / elapsed,
        madd_gib / elapsed,
    );
}

fn main() {
    #[cfg(target_arch = "x86_64")]
    let simd = if std::is_x86_feature_detected!("avx2") {
        "AVX2"
    } else {
        "scalar"
    };
    #[cfg(not(target_arch = "x86_64"))]
    let simd = "scalar";

    let threads = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);

    println!(
        "PAR2 encoder benchmark — slice {SLICE_SIZE} B — {simd} path — {threads} thread(s)\n"
    );

    bench("64 MiB @ 10%", 64, 10);
    bench("256 MiB @ 10%", 256, 10);
    bench("256 MiB @ 20%", 256, 20);
}
