use std::env;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use rayon::prelude::*;
use pesto::yenc::encode;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: yenc-bench-parallel <file> [line_len] [num_threads]");
        eprintln!("  num_threads: number of parallel encoding tasks (0 = auto)");
        std::process::exit(1);
    }

    let path = &args[1];
    let line_len = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(128);
    let num_threads = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);

    let mut file = File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    let size = data.len();

    // Configure thread pool if num_threads specified
    if num_threads > 0 {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build_global();
    }

    // Warm up
    let mut out = Vec::with_capacity(size + size / 32 + 1024);
    encode(&mut out, &data, line_len);

    // Determine iterations based on file size
    let iterations = if size < 1024 * 1024 {
        100
    } else if size < 100 * 1024 * 1024 {
        5
    } else {
        1
    };

    // Parallel benchmark: encode the same data multiple times in parallel
    let data = Arc::new(data);
    let line_len_arc = Arc::new(line_len);

    let start = Instant::now();
    for _ in 0..iterations {
        let results: Vec<_> = (0..rayon::current_num_threads())
            .into_par_iter()
            .map(|_| {
                let mut out = Vec::with_capacity(size + size / 32 + 1024);
                encode(&mut out, &data, *line_len_arc);
                out.len()
            })
            .collect();

        // Prevent optimization
        if results.is_empty() {
            panic!("Encoding failed");
        }
    }
    let elapsed = start.elapsed();

    // Total bytes encoded: each iteration encodes num_threads copies
    let num_threads_used = rayon::current_num_threads();
    let total_bytes = size as f64 * iterations as f64 * num_threads_used as f64;
    let mbps = (total_bytes / 1024.0 / 1024.0) / elapsed.as_secs_f64();

    println!("{:.2}", mbps);

    Ok(())
}
