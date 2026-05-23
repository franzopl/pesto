use anyhow::{Context, Result};
use clap::Parser;
use pesto_par2::ops::{calculate_geometry, ingest_files, CreateOptions, InputFile};
use pesto_par2::worker::Par2Worker;
use pesto_par2::{encoder::RecoveryEncoder, layout, packet, SimdPath};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;

#[derive(Parser, Debug)]
#[command(name = "parmesan", version, about = "Fast, standalone PAR2 creation tool")]
struct Cli {
    /// Files to protect.
    #[arg(required = true)]
    files: Vec<PathBuf>,

    /// Percentage of recovery data to generate.
    #[arg(short, long, default_value_t = 10)]
    recovery_pct: u8,

    /// Manual PAR2 slice size, e.g. "1 MiB".
    #[arg(short = 's', long)]
    slice_size: Option<String>,

    /// Target number of input slices.
    #[arg(short = 'n', long)]
    slice_count: Option<usize>,

    /// Exact number of recovery blocks to generate.
    #[arg(short = 'r', long)]
    recovery_count: Option<usize>,

    /// Maximum RAM for recovery buffers, e.g. "1 GiB".
    #[arg(short = 'm', long, default_value = "1 GiB")]
    memory_limit: String,

    /// Number of threads for parallel compute.
    #[arg(short = 't', long)]
    threads: Option<usize>,

    /// Force a specific SIMD multiplication backend.
    #[arg(long, value_enum, default_value_t = SimdPath::Auto)]
    simd: SimdPath,

    /// Output directory for PAR2 files.
    #[arg(short, long)]
    out_dir: Option<PathBuf>,
}

fn parse_size(s: &str) -> Result<u64> {
    let s = s.trim().to_ascii_lowercase();
    let split = s
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(s.len());
    let (num_str, unit) = s.split_at(split);
    let value: f64 = num_str.trim().parse().context("invalid number")?;
    let multiplier: f64 = match unit.trim() {
        "" | "b" => 1.0,
        "k" | "kb" | "kib" => 1024.0,
        "m" | "mb" | "mib" => 1024.0 * 1024.0,
        "g" | "gb" | "gib" => 1024.0 * 1024.0 * 1024.0,
        other => anyhow::bail!("unknown unit `{other}`"),
    };
    Ok((value * multiplier) as u64)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let mut input_files = Vec::new();
    for path in &cli.files {
        let md = std::fs::metadata(path).with_context(|| format!("stat `{}`", path.display()))?;
        if md.is_dir() {
            // Recursive walk for directories? For now, let's keep it simple.
            anyhow::bail!("directories not supported yet in parmesan; provide loose files");
        }
        input_files.push(InputFile {
            path: path.clone(),
            display_name: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            size: md.len(),
        });
    }

    let options = CreateOptions {
        slice_size: cli.slice_size.as_deref().map(parse_size).transpose()?.map(|s| s as usize),
        slice_count: cli.slice_count,
        recovery_count: cli.recovery_count,
        recovery_pct: cli.recovery_pct,
        memory_limit: parse_size(&cli.memory_limit)? as usize,
        threads: cli.threads.unwrap_or(0),
        simd: cli.simd,
    };

    let (slice_size, total_slices, recovery_count) = calculate_geometry(&input_files, &options)?;

    println!("PAR2 Geometry:");
    println!("  Input files    : {}", input_files.len());
    println!("  Input slices   : {total_slices}");
    println!("  Recovery blocks: {recovery_count}");
    println!("  Slice size     : {slice_size} bytes");

    // Configure Rayon
    let rayon_threads = if options.threads > 0 {
        options.threads
    } else {
        pesto_par2::performance_core_count()
    };
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(rayon_threads)
        .build_global();

    let mut all_checksums: Vec<Vec<packet::SliceChecksum>> = vec![Vec::new(); input_files.len()];

    // In parmesan, we only do one pass if possible.
    let slices_per_pass = (options.memory_limit / slice_size).max(1);
    let mut start_exponent = 0;
    
    let mut base_packets = Vec::new();
    let mut rsid = [0u8; 16];

    while start_exponent < recovery_count {
        let count = (recovery_count - start_exponent).min(slices_per_pass);
        let pass_idx = start_exponent / slices_per_pass;
        
        println!("\nPass {} (recovery blocks {}-{}):", pass_idx + 1, start_exponent, start_exponent + count - 1);

        let mut enc = RecoveryEncoder::new(slice_size, total_slices, start_exponent as u32, count);
        if pass_idx == 0 {
            enc = enc.with_checksums();
        }
        enc = enc.with_simd_path(options.simd);
        enc = enc.with_flush_limit((options.memory_limit / 4).clamp(256*1024*1024, 1024*1024*1024));

        let worker = Par2Worker::spawn(enc, pass_idx == 0);
        
        let hashes_opt = ingest_files(&input_files, &worker, slice_size, pass_idx == 0).await?;
        
        let (recovery_slices, slice_checksums, _) = tokio::task::block_in_place(|| worker.finish());

        if pass_idx == 0 {
            // Store hashes and checksums
            all_hashes = hashes_opt.unwrap();
            let mut cs_iter = slice_checksums.into_iter();
            for (idx, f) in input_files.iter().enumerate() {
                let n = (f.size as usize).div_ceil(slice_size);
                all_checksums[idx] = cs_iter.by_ref().take(n).collect();
            }

            // Generate main packets
            let mut file_ids = Vec::new();
            for (idx, f) in input_files.iter().enumerate() {
                file_ids.push(packet::compute_file_id(&all_hashes[idx].md5_16k, f.size, &f.display_name));
            }

            let main_b = packet::main_body(slice_size as u64, &file_ids);
            rsid = packet::recovery_set_id(&main_b);
            base_packets.extend(packet::serialize_packet(&rsid, &packet::TYPE_MAIN, &main_b));
            base_packets.extend(packet::serialize_packet(&rsid, &packet::TYPE_CREATOR, &packet::creator_body("parmesan")));

            for (idx, f) in input_files.iter().enumerate() {
                let fid = &file_ids[idx];
                base_packets.extend(packet::serialize_packet(&rsid, &packet::TYPE_FILE_DESC, &packet::file_description_body(fid, &all_hashes[idx].md5_full, &all_hashes[idx].md5_16k, f.size, &f.display_name)));
                base_packets.extend(packet::serialize_packet(&rsid, &packet::TYPE_IFSC, &packet::ifsc_body(fid, &all_checksums[idx])));
            }

            // Write index file
            let out_dir = cli.out_dir.clone().unwrap_or_else(|| PathBuf::from("."));
            if !out_dir.exists() {
                std::fs::create_dir_all(&out_dir)?;
            }
            let index_name = layout::index_name(&input_files[0].display_name);
            let index_path = out_dir.join(index_name);
            std::fs::write(&index_path, &base_packets)?;
            println!("Wrote {}", index_path.display());
        }

        // Write volumes
        let out_dir = cli.out_dir.clone().unwrap_or_else(|| PathBuf::from("."));
        let volumes = layout::plan_volumes(recovery_count as u32);
        for slice in recovery_slices {
            let vol = volumes.iter().find(|v| slice.exponent >= v.first && slice.exponent < v.first + v.count).unwrap();
            let vol_name = layout::volume_name(&input_files[0].display_name, *vol);
            let vol_path = out_dir.join(&vol_name);

            let mut f = tokio::fs::OpenOptions::new().create(true).append(true).open(&vol_path).await?;
            if slice.exponent == vol.first {
                f.write_all(&base_packets).await?;
            }
            let pkt = packet::serialize_packet(&rsid, &packet::TYPE_RECOVERY, &packet::recovery_body(slice.exponent, &slice.data));
            f.write_all(&pkt).await?;
            
            if slice.exponent == vol.first + vol.count - 1 {
                println!("Finished {}", vol_path.display());
            }
        }

        start_exponent += count;
    }

    println!("\nAll recovery volumes created successfully.");
    Ok(())
}
