//! Deterministic test-corpus generator for the benchmark suite.
//!
//! Benchmarks need input that is (a) reproducible on any machine, so two
//! people comparing numbers are encoding the same bytes, and (b) realistic in
//! entropy, because both PAR2 and archive compression behave completely
//! differently on zeroes than on real media. A sparse file of zeroes — what
//! `bench/` used before — makes `7z` compress a 5 GB workload to nothing and
//! removes all read I/O from the measurement.
//!
//! So: a seeded PRNG, not `/dev/urandom` (unreproducible) and not zeroes.
//! Given the same `--seed`, `--size` and `--entropy`, this writes the same
//! bytes on every machine and every run, and prints the CRC-32 to prove it.
//!
//! Usage:
//!   bench-gen --out FILE --size 8G [--seed 1] [--entropy 100]
//!   bench-gen --out-dir DIR --count 2000 --template 'part-%04d.bin' --size 256K
//!   bench-gen --check FILE                       # print size + CRC-32 only
//!
//! `--entropy N` is the percentage of each 4 KiB chunk filled with random
//! bytes; the remainder is a repeating motif. 100 (the default) is
//! incompressible, which is what real video is and therefore the right
//! default for PAR2 and posting benchmarks. Lower values give an archive
//! something to actually compress, for the `--compress` workloads.
//!
//! Output is one JSON object per generated file on stdout, so the shell
//! harness can record exactly what it produced in the run manifest.

use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use rayon::prelude::*;

/// Generation block. Large enough to amortise per-block setup, small enough
/// that a batch of them across all cores still fits comfortably in cache-warm
/// memory rather than ballooning RSS on a 64-core box.
const BLOCK: usize = 4 * 1024 * 1024;
/// Granularity at which `--entropy` mixes random and repeating bytes.
const CHUNK: usize = 4096;

/// SplitMix64 — used only to expand a small seed into PRNG state.
fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// xoshiro256++ — fast, well-distributed, and trivially reproducible across
/// platforms because it is pure integer arithmetic with no floating point.
struct Rng([u64; 4]);

impl Rng {
    /// Seeded per block index, not per file, so blocks can be generated out
    /// of order (in parallel) and still produce byte-identical output.
    fn for_block(seed: u64, block: u64) -> Self {
        let mut s = seed ^ block.wrapping_mul(0x9E37_79B9_7F4A_7C15);
        Self([
            splitmix64(&mut s),
            splitmix64(&mut s),
            splitmix64(&mut s),
            splitmix64(&mut s),
        ])
    }

    #[inline]
    fn next_u64(&mut self) -> u64 {
        let s = &mut self.0;
        let result = s[0].wrapping_add(s[3]).rotate_left(23).wrapping_add(s[0]);
        let t = s[1] << 17;
        s[2] ^= s[0];
        s[3] ^= s[1];
        s[1] ^= s[2];
        s[0] ^= s[3];
        s[2] ^= t;
        s[3] = s[3].rotate_left(45);
        result
    }
}

/// Fill `buf` with the deterministic content of block `index`.
fn fill_block(buf: &mut [u8], seed: u64, index: u64, entropy: u8) {
    let mut rng = Rng::for_block(seed, index);
    let random_bytes = (CHUNK * entropy as usize) / 100;

    for chunk in buf.chunks_mut(CHUNK) {
        let split = random_bytes.min(chunk.len());
        let (rand_part, motif_part) = chunk.split_at_mut(split);

        for out in rand_part.chunks_mut(8) {
            let bytes = rng.next_u64().to_le_bytes();
            out.copy_from_slice(&bytes[..out.len()]);
        }
        // The low-entropy remainder: a short repeating motif, which is what
        // gives an archiver something to find without making the whole file
        // trivially compressible.
        if !motif_part.is_empty() {
            let motif = rng.next_u64().to_le_bytes();
            for (i, b) in motif_part.iter_mut().enumerate() {
                *b = motif[i % motif.len()];
            }
        }
    }
}

fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s.find(|c: char| !c.is_ascii_digit() && c != '.')?;
    let (num, unit) = if split == 0 {
        (s.as_str(), "")
    } else {
        s.split_at(split)
    };
    let value: f64 = num.parse().ok()?;
    let mult: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        "t" | "tb" | "tib" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((value * mult) as u64)
}

/// Accepts both a bare number ("2048") and a suffixed one ("2G").
fn parse_size_or_plain(s: &str) -> Option<u64> {
    s.trim().parse::<u64>().ok().or_else(|| parse_size(s))
}

struct Args {
    out: Option<PathBuf>,
    out_dir: Option<PathBuf>,
    template: String,
    count: usize,
    size: u64,
    seed: u64,
    entropy: u8,
    sparse: bool,
    check: Option<PathBuf>,
    force: bool,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            out: None,
            out_dir: None,
            template: "file-%04d.bin".to_string(),
            count: 1,
            size: 0,
            seed: 1,
            entropy: 100,
            sparse: false,
            check: None,
            force: false,
        }
    }
}

fn usage() -> ! {
    eprintln!(
        "usage: bench-gen --out FILE --size 8G [--seed N] [--entropy 0..100] [--sparse]\n\
         \x20      bench-gen --out-dir DIR --count N --template 'part-%04d.bin' --size 256K\n\
         \x20      bench-gen --check FILE"
    );
    std::process::exit(2)
}

fn parse_args() -> Args {
    let mut a = Args::default();
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let value = |i: usize| argv.get(i + 1).cloned().unwrap_or_default();
    let mut i = 0;
    while i < argv.len() {
        match argv[i].as_str() {
            "--out" => {
                a.out = Some(PathBuf::from(value(i)));
                i += 1;
            }
            "--out-dir" => {
                a.out_dir = Some(PathBuf::from(value(i)));
                i += 1;
            }
            "--template" => {
                a.template = value(i);
                i += 1;
            }
            "--count" => {
                a.count = value(i).parse().unwrap_or(1);
                i += 1;
            }
            "--size" => {
                a.size = parse_size_or_plain(&value(i)).unwrap_or_else(|| usage());
                i += 1;
            }
            "--seed" => {
                a.seed = value(i).parse().unwrap_or(1);
                i += 1;
            }
            "--entropy" => {
                a.entropy = value(i).parse::<u8>().unwrap_or(100).min(100);
                i += 1;
            }
            "--check" => {
                a.check = Some(PathBuf::from(value(i)));
                i += 1;
            }
            "--sparse" => a.sparse = true,
            "--force" => a.force = true,
            "-h" | "--help" => usage(),
            other => {
                eprintln!("unknown argument `{other}`");
                usage()
            }
        }
        i += 1;
    }
    a
}

/// Substitute a `%0Nd`/`%d` placeholder in a filename template.
fn render_template(template: &str, index: usize) -> String {
    let Some(start) = template.find('%') else {
        return format!("{template}{index}");
    };
    let Some(rel_end) = template[start..].find('d') else {
        return format!("{template}{index}");
    };
    let end = start + rel_end;
    let width: usize = template[start + 1..end]
        .trim_start_matches('0')
        .parse()
        .unwrap_or(0);
    format!(
        "{}{:0width$}{}",
        &template[..start],
        index,
        &template[end + 1..],
        width = width
    )
}

fn crc_of_file(path: &Path) -> std::io::Result<(u64, u32)> {
    let mut f = File::open(path)?;
    let mut hasher = crc32fast::Hasher::new();
    let mut buf = vec![0u8; BLOCK];
    let mut total = 0u64;
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        total += n as u64;
    }
    Ok((total, hasher.finalize()))
}

fn emit(path: &Path, size: u64, seed: u64, entropy: u8, sparse: bool, crc: u32, ms: u128) {
    println!(
        r#"{{"path":"{}","size":{size},"seed":{seed},"entropy":{entropy},"sparse":{sparse},"crc32":"{crc:08x}","elapsed_ms":{ms}}}"#,
        path.display().to_string().replace('"', "\\\"")
    );
}

fn generate(path: &Path, a: &Args, seed: u64) -> std::io::Result<()> {
    if path.exists() && !a.force {
        // Reuse: regenerating tens of GB on every run would dominate the
        // benchmark's own wall time. The caller checks the manifest CRC.
        let (size, crc) = crc_of_file(path)?;
        emit(path, size, seed, a.entropy, a.sparse, crc, 0);
        return Ok(());
    }

    let started = std::time::Instant::now();
    let file = File::create(path)?;

    if a.sparse {
        file.set_len(a.size)?;
        emit(
            path,
            a.size,
            seed,
            a.entropy,
            true,
            0,
            started.elapsed().as_millis(),
        );
        return Ok(());
    }

    let mut writer = BufWriter::with_capacity(BLOCK, file);
    let mut hasher = crc32fast::Hasher::new();
    let total_blocks = a.size.div_ceil(BLOCK as u64);
    let batch = rayon::current_num_threads().max(1);
    let mut buffers: Vec<Vec<u8>> = vec![vec![0u8; BLOCK]; batch];

    let mut block = 0u64;
    while block < total_blocks {
        let this_batch = batch.min((total_blocks - block) as usize);
        buffers[..this_batch]
            .par_iter_mut()
            .enumerate()
            .for_each(|(k, buf)| {
                let index = block + k as u64;
                let remaining = a.size - index * BLOCK as u64;
                let len = (remaining as usize).min(BLOCK);
                buf.truncate(len);
                buf.resize(len, 0);
                fill_block(buf, seed, index, a.entropy);
            });
        for buf in buffers[..this_batch].iter_mut() {
            hasher.update(buf);
            writer.write_all(buf)?;
            buf.resize(BLOCK, 0);
        }
        block += this_batch as u64;
    }
    writer.flush()?;
    emit(
        path,
        a.size,
        seed,
        a.entropy,
        false,
        hasher.finalize(),
        started.elapsed().as_millis(),
    );
    Ok(())
}

fn main() -> std::io::Result<()> {
    let a = parse_args();

    if let Some(path) = &a.check {
        let (size, crc) = crc_of_file(path)?;
        emit(path, size, 0, 0, false, crc, 0);
        return Ok(());
    }

    match (&a.out, &a.out_dir) {
        (Some(path), None) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            generate(path, &a, a.seed)?;
        }
        (None, Some(dir)) => {
            std::fs::create_dir_all(dir)?;
            for index in 0..a.count {
                let path = dir.join(render_template(&a.template, index));
                // Each file gets its own derived seed so two files of the
                // same size in one corpus are never byte-identical (which
                // would let an archiver dedupe them and skew --compress).
                generate(&path, &a, a.seed.wrapping_add(index as u64 * 0x9E37_79B9))?;
            }
        }
        _ => usage(),
    }
    Ok(())
}
