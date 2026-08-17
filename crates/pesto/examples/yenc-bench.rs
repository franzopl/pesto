//! yEnc microbenchmark driver — the machine-readable one the `bench/` suite
//! consumes.
//!
//! Measures the encoder (and, with `--decode`, the decoder) in isolation:
//! data is generated once in memory, so nothing here touches the disk, the
//! network or the article/NNTP layers. That is the point — it is the bottom
//! layer of the suite, where a number can be attributed to the SIMD kernel
//! and nothing else.
//!
//! Built as an example, not a `#[bench]`, so the binary lands at a stable
//! path (`target/release/examples/yenc-bench`) the shell harness can call
//! directly without parsing `cargo` output.
//!
//! Usage:
//!   yenc-bench --json [--sizes 4096,131072,768000] [--line-lens 128,256]
//!              [--paths auto,scalar,ssse3,avx2] [--min-time 1.0] [--decode]
//!   yenc-bench <file> [line_len] [path]     # legacy: one number on stdout
//!
//! Sizes are given in bytes; `768000` is the default article size, and is the
//! only one that reflects what the poster actually calls the encoder with.
//! The smaller and larger sizes are there to expose cache effects and
//! per-call overhead.

use std::time::{Duration, Instant};

#[cfg(target_arch = "aarch64")]
use pesto::yenc::encode_neon;
use pesto::yenc::{encode, encode_scalar};
#[cfg(target_arch = "x86_64")]
use pesto::yenc::{encode_avx2, encode_ssse3};

type EncodeFn = fn(&mut Vec<u8>, &[u8], usize);

/// Default sweep: two cache-resident sizes, the real article size, and one
/// size well past L2 so the memory path shows up.
const DEFAULT_SIZES: &[usize] = &[4096, 131_072, 768_000, 8 * 1024 * 1024];
const DEFAULT_LINE_LENS: &[usize] = &[128];
const MIB: f64 = 1024.0 * 1024.0;

/// Deterministic filler — same generator family as `bench-gen`, so a
/// microbenchmark and an end-to-end run are encoding statistically identical
/// data. yEnc's cost depends on how many bytes need escaping, so this must
/// not be zeroes: zeroes are the *worst* case (NUL always escapes) and would
/// understate throughput by a wide margin.
fn make_data(len: usize, seed: u64) -> Vec<u8> {
    let mut out = vec![0u8; len];
    let mut x = seed | 1;
    for chunk in out.chunks_mut(8) {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        let bytes = x.to_le_bytes();
        chunk.copy_from_slice(&bytes[..chunk.len()]);
    }
    out
}

fn resolve_path(name: &str) -> Option<EncodeFn> {
    match name {
        "auto" => Some(encode as EncodeFn),
        "scalar" => Some(encode_scalar as EncodeFn),
        #[cfg(target_arch = "x86_64")]
        "ssse3" => std::arch::is_x86_feature_detected!("ssse3").then_some(encode_ssse3 as EncodeFn),
        #[cfg(target_arch = "x86_64")]
        "avx2" => std::arch::is_x86_feature_detected!("avx2").then_some(encode_avx2 as EncodeFn),
        #[cfg(target_arch = "aarch64")]
        "neon" => Some(encode_neon as EncodeFn),
        _ => None,
    }
}

/// Every path worth measuring on this CPU, in ascending order of expected
/// speed. Unavailable paths are skipped rather than reported as zero.
fn available_paths() -> Vec<&'static str> {
    let mut v = vec!["scalar"];
    #[cfg(target_arch = "x86_64")]
    {
        if std::arch::is_x86_feature_detected!("ssse3") {
            v.push("ssse3");
        }
        if std::arch::is_x86_feature_detected!("avx2") {
            v.push("avx2");
        }
    }
    #[cfg(target_arch = "aarch64")]
    v.push("neon");
    v.push("auto");
    v
}

struct Measurement {
    iters: u64,
    elapsed: Duration,
}

impl Measurement {
    fn mibps(&self, bytes_per_iter: usize) -> f64 {
        let total = (self.iters as f64) * (bytes_per_iter as f64);
        (total / MIB) / self.elapsed.as_secs_f64()
    }
    fn ns_per_iter(&self) -> f64 {
        self.elapsed.as_nanos() as f64 / self.iters as f64
    }
}

/// Run `f` until at least `min_time` has elapsed, then report the rate.
///
/// Iteration count is doubled rather than fixed so a 4 KiB input and an 8 MiB
/// input both get a statistically meaningful sample without the small one
/// taking a fixed, wasteful number of seconds.
fn measure(min_time: Duration, mut f: impl FnMut()) -> Measurement {
    // Warmup: first call pays page-fault and branch-predictor costs that
    // belong to neither the encoder nor the comparison.
    f();
    let mut iters = 16u64;
    loop {
        let start = Instant::now();
        for _ in 0..iters {
            f();
        }
        let elapsed = start.elapsed();
        if elapsed >= min_time || iters >= 1 << 30 {
            return Measurement { iters, elapsed };
        }
        iters *= 2;
    }
}

struct Args {
    json: bool,
    sizes: Vec<usize>,
    line_lens: Vec<usize>,
    paths: Vec<String>,
    min_time: Duration,
    decode: bool,
}

fn parse_list<T: std::str::FromStr>(s: &str) -> Vec<T> {
    s.split(',').filter_map(|p| p.trim().parse().ok()).collect()
}

fn parse_args(argv: &[String]) -> Args {
    let mut a = Args {
        json: false,
        sizes: DEFAULT_SIZES.to_vec(),
        line_lens: DEFAULT_LINE_LENS.to_vec(),
        paths: available_paths().iter().map(|s| s.to_string()).collect(),
        min_time: Duration::from_secs_f64(1.0),
        decode: false,
    };
    let value = |i: usize| argv.get(i + 1).cloned().unwrap_or_default();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--json" => a.json = true,
            "--decode" => a.decode = true,
            "--sizes" => {
                a.sizes = parse_list(&value(i));
                i += 1;
            }
            "--line-lens" => {
                a.line_lens = parse_list(&value(i));
                i += 1;
            }
            "--paths" => {
                a.paths = value(i).split(',').map(|s| s.trim().to_string()).collect();
                i += 1;
            }
            "--min-time" => {
                a.min_time = Duration::from_secs_f64(value(i).parse().unwrap_or(1.0));
                i += 1;
            }
            _ => {}
        }
        i += 1;
    }
    a
}

fn emit(json: bool, op: &str, path: &str, line_len: usize, size: usize, m: &Measurement) {
    let mibps = m.mibps(size);
    if json {
        println!(
            r#"{{"op":"{op}","simd_path":"{path}","line_len":{line_len},"size":{size},"iters":{},"elapsed_ms":{:.3},"ns_per_iter":{:.1},"mibps":{mibps:.1}}}"#,
            m.iters,
            m.elapsed.as_secs_f64() * 1000.0,
            m.ns_per_iter(),
        );
    } else {
        println!(
            "{op:>7}  {path:>8}  ll={line_len:<4} {size:>9} B  {:>9.0} ns/iter  {mibps:>8.1} MiB/s",
            m.ns_per_iter()
        );
    }
}

/// The original `yenc-bench <file> [line_len] [path]` form: prints a single
/// MiB/s figure. Kept because `bench/yenc.sh` and any pasted command line
/// from an older README still call it that way.
fn legacy_mode(argv: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let data = std::fs::read(&argv[0])?;
    let line_len = argv.get(1).and_then(|s| s.parse().ok()).unwrap_or(128);
    let name = argv.get(2).map(|s| s.as_str()).unwrap_or("auto");
    let f = resolve_path(name).ok_or_else(|| format!("unknown or unsupported path `{name}`"))?;

    let mut out = Vec::with_capacity(data.len() + data.len() / 32 + 1024);
    let m = measure(Duration::from_secs(1), || {
        out.clear();
        f(&mut out, &data, line_len);
    });
    println!("{:.2}", m.mibps(data.len()));
    Ok(())
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    if let Some(first) = argv.first() {
        if !first.starts_with("--") {
            return legacy_mode(&argv);
        }
    }
    let args = parse_args(&argv);

    if !args.json {
        println!("yEnc microbenchmark — min-time {:?}\n", args.min_time);
    }

    for &size in &args.sizes {
        let data = make_data(size, 0x2545_F491_4F6C_DD1D);
        for &line_len in &args.line_lens {
            for name in &args.paths {
                let Some(f) = resolve_path(name) else {
                    continue;
                };
                let mut out = Vec::with_capacity(size + size / 32 + 1024);
                let m = measure(args.min_time, || {
                    out.clear();
                    f(&mut out, &data, line_len);
                });
                emit(args.json, "encode", name, line_len, size, &m);
            }

            if args.decode {
                // Decode is a single portable implementation — there are no
                // SIMD variants to sweep, so it is reported once per
                // (size, line_len) under the `auto` label.
                let spec = pesto::yenc::PartSpec {
                    number: 1,
                    total: 2,
                    offset: 0,
                };
                let part = pesto::yenc::encode_part(
                    "bench.bin",
                    size as u64 * 2,
                    spec,
                    &data,
                    line_len,
                    None,
                );
                let body = part.body;
                let m = measure(args.min_time, || {
                    let decoded = pesto::yenc::decode_part(&body).expect("decode");
                    std::hint::black_box(decoded.data.len());
                });
                emit(args.json, "decode", "auto", line_len, size, &m);
            }
        }
    }
    Ok(())
}
