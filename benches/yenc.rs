use std::hint::black_box;

use pesto::yenc::{encode_scalar, encode_ssse3};

fn bench(label: &str, n: usize, f: impl Fn(&[u8], &mut Vec<u8>)) {
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
    println!("{label:>30}  {n:>10} bytes  {ns_per_iter:>8} ns/iter  {throughput_mb:>8.1} MB/s");
}

fn main() {
    let sizes = [512usize, 4 * 1024, 128 * 1024, 750 * 1024];

    for n in sizes {
        bench("encode_scalar", n, |d, o| encode_scalar(o, d, 128));
    }
    println!();
    for n in sizes {
        bench("encode_ssse3 ", n, |d, o| encode_ssse3(o, d, 128));
    }
}
