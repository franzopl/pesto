use std::env;
use std::fs::File;
use std::io::Read;
use std::time::Instant;

// We use the yenc module from the pesto crate.
#[cfg(target_arch = "x86_64")]
use pesto::yenc::{encode, encode_avx2, encode_scalar, encode_ssse3};
#[cfg(target_arch = "aarch64")]
use pesto::yenc::{encode, encode_neon, encode_scalar};
#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
use pesto::yenc::{encode, encode_scalar};

type BenchFn = fn(&mut Vec<u8>, &[u8], usize);

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: yenc-bench <file> [line_len] [path]");
        #[cfg(target_arch = "x86_64")]
        eprintln!("Paths: auto (default), scalar, ssse3, avx2");
        #[cfg(target_arch = "aarch64")]
        eprintln!("Paths: auto (default), scalar, neon");
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        eprintln!("Paths: auto (default), scalar");
        std::process::exit(1);
    }

    let path = &args[1];
    let line_len = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(128);
    let path_select = args.get(3).map(|s| s.as_str()).unwrap_or("auto");

    let mut file = File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    let size = data.len();

    // Warm up
    let mut out = Vec::with_capacity(size + size / 32 + 1024);

    let bench_fn: BenchFn = match path_select {
        "scalar" => encode_scalar,
        #[cfg(target_arch = "x86_64")]
        "ssse3" => encode_ssse3,
        #[cfg(target_arch = "x86_64")]
        "avx2" => encode_avx2,
        #[cfg(target_arch = "aarch64")]
        "neon" => encode_neon,
        _ => encode,
    };

    (bench_fn)(&mut out, &data, line_len);

    let iterations = if size < 1024 * 1024 {
        1000
    } else if size < 100 * 1024 * 1024 {
        10
    } else {
        1
    };

    let start = Instant::now();
    for _ in 0..iterations {
        out.clear();
        (bench_fn)(&mut out, &data, line_len);
        // Prevent optimization
        if out.is_empty() && size > 0 {
            panic!("Encoding failed");
        }
    }
    let elapsed = start.elapsed();

    let total_bytes = size as f64 * iterations as f64;
    let mbps = (total_bytes / 1024.0 / 1024.0) / elapsed.as_secs_f64();

    println!("{:.2}", mbps);

    Ok(())
}
