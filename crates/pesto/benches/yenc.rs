use std::hint::black_box;

#[cfg(target_arch = "x86_64")]
use pesto::yenc::{encode, encode_avx2, encode_scalar, encode_ssse3};
#[cfg(not(target_arch = "x86_64"))]
use pesto::yenc::{encode, encode_scalar};

// nyuu 0.4.2 (node-yencode C++ addon, AVX2) — documented throughput used as
// reference target. Source: https://github.com/animetosho/node-yencode
//
//   line_len=128: ~1200 MB/s
//   line_len=256: ~2400 MB/s  (estimated; nyuu defaults to 128)
//
// Values printed at the end of each section so every run shows the target.

const NYUU_MBPS_128: f64 = 1200.0;
const NYUU_MBPS_256: f64 = 2400.0;

fn bench(label: &str, n: usize, line_len: usize, f: impl Fn(&[u8], &mut Vec<u8>)) -> f64 {
    let data: Vec<u8> = (0u8..=255).cycle().take(n).collect();
    let mut out = Vec::with_capacity(n + n / 16 + 128);
    let iters = (2_000_000 / n.max(1)).max(10);

    let t0 = std::time::Instant::now();
    for _ in 0..iters {
        out.clear();
        f(black_box(&data), black_box(&mut out));
    }
    let elapsed = t0.elapsed();

    let ns_per_iter = elapsed.as_nanos() / iters as u128;
    let throughput_mb = (n as f64 / (ns_per_iter as f64 / 1e9)) / 1_048_576.0;
    println!(
        "{label:>30}  ll={line_len:<3}  {n:>10} bytes  {ns_per_iter:>8} ns/iter  {throughput_mb:>8.1} MB/s"
    );
    throughput_mb
}

fn section(
    title: &str,
    line_len: usize,
    nyuu_ref: f64,
    sizes: &[usize],
    f: impl Fn(&[u8], &mut Vec<u8>) + Copy,
    label: &str,
) {
    println!("\n── {title} (line_len={line_len}) ──");
    let mut best = 0f64;
    for &n in sizes {
        let mb = bench(label, n, line_len, f);
        if mb > best {
            best = mb;
        }
    }
    let ratio = best / nyuu_ref;
    let marker = if ratio >= 1.0 {
        "✓ beats nyuu"
    } else {
        "✗ below nyuu target"
    };
    println!("  best={best:.0} MB/s  nyuu={nyuu_ref:.0} MB/s  ratio={ratio:.2}×  {marker}");
}

fn main() {
    let sizes = [512usize, 4 * 1024, 128 * 1024, 750 * 1024];

    // --- line_len = 128 (current default, nyuu default) ---
    section(
        "encode_scalar",
        128,
        NYUU_MBPS_128,
        &sizes,
        |d, o| encode_scalar(o, d, 128),
        "encode_scalar  ll=128",
    );
    #[cfg(target_arch = "x86_64")]
    {
        section(
            "encode_ssse3 ",
            128,
            NYUU_MBPS_128,
            &sizes,
            |d, o| encode_ssse3(o, d, 128),
            "encode_ssse3   ll=128",
        );
        section(
            "encode_avx2  ",
            128,
            NYUU_MBPS_128,
            &sizes,
            |d, o| encode_avx2(o, d, 128),
            "encode_avx2    ll=128",
        );
    }
    section(
        "encode (disp)",
        128,
        NYUU_MBPS_128,
        &sizes,
        |d, o| encode(o, d, 128),
        "encode(disp)   ll=128",
    );

    // --- line_len = 256 (wider safe zone — phase 27c target) ---
    section(
        "encode_scalar",
        256,
        NYUU_MBPS_256,
        &sizes,
        |d, o| encode_scalar(o, d, 256),
        "encode_scalar  ll=256",
    );
    #[cfg(target_arch = "x86_64")]
    {
        section(
            "encode_ssse3 ",
            256,
            NYUU_MBPS_256,
            &sizes,
            |d, o| encode_ssse3(o, d, 256),
            "encode_ssse3   ll=256",
        );
        section(
            "encode_avx2  ",
            256,
            NYUU_MBPS_256,
            &sizes,
            |d, o| encode_avx2(o, d, 256),
            "encode_avx2    ll=256",
        );
    }
    section(
        "encode (disp)",
        256,
        NYUU_MBPS_256,
        &sizes,
        |d, o| encode(o, d, 256),
        "encode(disp)   ll=256",
    );
}
