/// Benchmark that simulates real posting scenario:
/// - Multiple file segments being encoded in parallel (like posting multiple articles)
/// - Uses rayon thread pool (matches pesto's real posting architecture)
/// - Compares against node-yencode single-threaded
use std::env;
use std::fs::File;
use std::io::Read;
use std::sync::Arc;
use std::time::Instant;

use pesto::yenc::encode_part;
use rayon::prelude::*;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: yenc-bench-posting <file> [line_len] [num_threads]");
        eprintln!("  Simulates posting multiple file segments in parallel");
        eprintln!("  num_threads: rayon thread pool size (0 = auto, uses performance_core_count)");
        std::process::exit(1);
    }

    let path = &args[1];
    let line_len = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(128);
    let num_threads = args.get(3).and_then(|s| s.parse().ok()).unwrap_or(0);

    let mut file = File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    let file_size = data.len() as u64;
    let segment_size = 700_000usize; // ~700KB per article (typical usenet article size)
    let num_segments = (file_size as usize).div_ceil(segment_size);

    // Configure thread pool if num_threads specified
    if num_threads > 0 {
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build_global();
    }

    let current_threads = rayon::current_num_threads();

    // Warm up
    let data_arc = Arc::new(data);
    let segments: Vec<(usize, Vec<u8>)> = (0..num_segments)
        .map(|i| {
            let start = i * segment_size;
            let end = (start + segment_size).min(data_arc.len());
            (i, data_arc[start..end].to_vec())
        })
        .collect();

    for (i, segment) in segments.iter() {
        let spec = pesto::yenc::PartSpec {
            number: (*i + 1) as u32,
            total: num_segments as u32,
            offset: (*i * segment_size) as u64,
        };
        encode_part("file.bin", file_size, spec, segment, line_len, None);
    }

    // Benchmark: encode all segments in parallel, multiple iterations
    let iterations = if file_size < 1024 * 1024 { 3 } else { 1 };

    let start = Instant::now();
    for _ in 0..iterations {
        let _results: Vec<_> = segments
            .par_iter()
            .map(|(i, segment)| {
                let spec = pesto::yenc::PartSpec {
                    number: (*i + 1) as u32,
                    total: num_segments as u32,
                    offset: (*i * segment_size) as u64,
                };
                encode_part("file.bin", file_size, spec, segment, line_len, None);
                segment.len()
            })
            .collect();
    }
    let elapsed = start.elapsed();

    // Total bytes processed: all segments × iterations
    let total_bytes = (file_size as f64) * (iterations as f64);
    let mbps = (total_bytes / 1024.0 / 1024.0) / elapsed.as_secs_f64();

    // Output: format "MBPS THREADS SEGMENTS"
    println!("{:.2} {} {}", mbps, current_threads, num_segments);

    Ok(())
}
