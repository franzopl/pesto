use anyhow::{Context, Result};
use clap::Parser;
use pesto_par2::ops::{calculate_geometry, ingest_files, CreateOptions, InputFile};
use pesto_par2::worker::Par2Worker;
use pesto_par2::{encoder::RecoveryEncoder, layout, packet, SimdPath};
use std::path::PathBuf;
use tokio::io::AsyncWriteExt;
use walkdir::WalkDir;

#[derive(Parser, Debug)]
#[command(
    name = "parmesan",
    version,
    about = "Fast, standalone PAR2 creation tool"
)]
struct Cli {
    /// Files or directories to protect.
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

    /// Base name for output PAR2 files (default: first input file's name).
    #[arg(short = 'b', long)]
    base_name: Option<String>,

    /// Suppress all progress and geometry output.
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Overwrite existing PAR2 files instead of failing.
    #[arg(short = 'O', long)]
    overwrite: bool,

    /// Skip generating the index (.par2) file.
    #[arg(long)]
    no_index: bool,

    /// Recurse into directories.
    #[arg(short = 'R', long)]
    recurse: bool,

    /// Embed a comment in the PAR2 Creator packet (repeatable).
    #[arg(short = 'c', long)]
    comment: Vec<String>,

    /// Exponent of the first recovery block (default 0).
    #[arg(short = 'e', long, default_value_t = 0)]
    recovery_offset: usize,
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

fn collect_files(paths: &[PathBuf], recurse: bool) -> Result<Vec<InputFile>> {
    let mut input_files = Vec::new();
    for path in paths {
        let md = std::fs::metadata(path).with_context(|| format!("stat `{}`", path.display()))?;
        if md.is_dir() {
            if !recurse {
                anyhow::bail!(
                    "`{}` is a directory; use --recurse (-R) to expand directories",
                    path.display()
                );
            }
            for entry in WalkDir::new(path)
                .follow_links(false)
                .sort_by_file_name()
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_file())
            {
                let size = entry.metadata()?.len();
                input_files.push(InputFile {
                    path: entry.path().to_path_buf(),
                    display_name: entry.file_name().to_string_lossy().into_owned(),
                    size,
                });
            }
        } else {
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
    }
    Ok(input_files)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    let input_files = collect_files(&cli.files, cli.recurse)?;
    if input_files.is_empty() {
        anyhow::bail!("no input files found");
    }

    let options = CreateOptions {
        slice_size: cli
            .slice_size
            .as_deref()
            .map(parse_size)
            .transpose()?
            .map(|s| s as usize),
        slice_count: cli.slice_count,
        recovery_count: cli.recovery_count,
        recovery_pct: cli.recovery_pct,
        memory_limit: parse_size(&cli.memory_limit)? as usize,
        threads: cli.threads.unwrap_or(0),
        simd: cli.simd,
    };

    let (slice_size, total_slices, recovery_count) = calculate_geometry(&input_files, &options)?;

    if !cli.quiet {
        println!("PAR2 Geometry:");
        println!("  Input files    : {}", input_files.len());
        println!("  Input slices   : {total_slices}");
        println!("  Recovery blocks: {recovery_count}");
        println!("  Slice size     : {slice_size} bytes");
        if cli.recovery_offset > 0 {
            println!("  Recovery offset: {}", cli.recovery_offset);
        }
    }

    // Configure Rayon
    let rayon_threads = if options.threads > 0 {
        options.threads
    } else {
        pesto_par2::performance_core_count()
    };
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(rayon_threads)
        .build_global();

    let out_dir = cli.out_dir.clone().unwrap_or_else(|| PathBuf::from("."));
    if !out_dir.exists() {
        std::fs::create_dir_all(&out_dir)?;
    }

    let base_name = cli
        .base_name
        .clone()
        .unwrap_or_else(|| input_files[0].display_name.clone());

    let creator_string = if cli.comment.is_empty() {
        "parmesan".to_owned()
    } else {
        format!("parmesan | {}", cli.comment.join(" | "))
    };

    let mut all_checksums: Vec<Vec<packet::SliceChecksum>> = vec![Vec::new(); input_files.len()];

    let slices_per_pass = (options.memory_limit / slice_size).max(1);
    // `cursor` tracks our position within the generated recovery set; the
    // on-disk exponent of each block is `recovery_offset + cursor`.
    let mut cursor = 0usize;

    let mut base_packets = Vec::new();
    let mut rsid = [0u8; 16];

    while cursor < recovery_count {
        let count = (recovery_count - cursor).min(slices_per_pass);
        let pass_idx = cursor / slices_per_pass;
        let first_exponent = (cli.recovery_offset + cursor) as u32;

        if !cli.quiet {
            println!(
                "\nPass {} (recovery blocks {}-{}):",
                pass_idx + 1,
                first_exponent,
                first_exponent + count as u32 - 1
            );
        }

        let mut enc = RecoveryEncoder::new_smart(slice_size, total_slices, first_exponent, count);
        if pass_idx == 0 {
            enc = enc.with_checksums();
        }
        enc = enc.with_simd_path(options.simd);
        enc = enc.with_flush_limit(
            (options.memory_limit / 4).clamp(256 * 1024 * 1024, 1024 * 1024 * 1024),
        );

        let worker = Par2Worker::spawn(enc, pass_idx == 0);

        ingest_files(&input_files, &worker, slice_size).await?;

        let (recovery_slices, slice_checksums, hashes) =
            tokio::task::block_in_place(|| worker.finish());

        if pass_idx == 0 {
            let all_hashes = hashes;
            let mut cs_iter = slice_checksums.into_iter();
            for (idx, f) in input_files.iter().enumerate() {
                let n = (f.size as usize).div_ceil(slice_size);
                all_checksums[idx] = cs_iter.by_ref().take(n).collect();
            }

            let mut file_ids = Vec::new();
            for (idx, f) in input_files.iter().enumerate() {
                file_ids.push(packet::compute_file_id(
                    &all_hashes[idx].md5_16k,
                    f.size,
                    &f.display_name,
                ));
            }

            let main_b = packet::main_body(slice_size as u64, &file_ids);
            rsid = packet::recovery_set_id(&main_b);
            base_packets.extend(packet::serialize_packet(&rsid, &packet::TYPE_MAIN, &main_b));
            base_packets.extend(packet::serialize_packet(
                &rsid,
                &packet::TYPE_CREATOR,
                &packet::creator_body(&creator_string),
            ));

            for (idx, f) in input_files.iter().enumerate() {
                let fid = &file_ids[idx];
                base_packets.extend(packet::serialize_packet(
                    &rsid,
                    &packet::TYPE_FILE_DESC,
                    &packet::file_description_body(
                        fid,
                        &all_hashes[idx].md5_full,
                        &all_hashes[idx].md5_16k,
                        f.size,
                        &f.display_name,
                    ),
                ));
                base_packets.extend(packet::serialize_packet(
                    &rsid,
                    &packet::TYPE_IFSC,
                    &packet::ifsc_body(fid, &all_checksums[idx]),
                ));
            }

            if !cli.no_index {
                let index_name = layout::index_name(&base_name);
                let index_path = out_dir.join(&index_name);
                if !cli.overwrite && index_path.exists() {
                    anyhow::bail!(
                        "output file already exists: `{}`; use --overwrite to replace it",
                        index_path.display()
                    );
                }
                std::fs::write(&index_path, &base_packets)?;
                if !cli.quiet {
                    println!("Wrote {}", index_path.display());
                }
            }
        }

        let volumes = layout::plan_volumes(recovery_count as u32);
        for slice in recovery_slices {
            let abs_exp = slice.exponent;
            // Map the absolute exponent back to a position in the layout
            // (which was planned from 0, but our exponents start at recovery_offset).
            let layout_exp = abs_exp - cli.recovery_offset as u32;
            let vol = volumes
                .iter()
                .find(|v| layout_exp >= v.first && layout_exp < v.first + v.count)
                .unwrap();

            let vol_name = layout::volume_name(&base_name, *vol);
            let vol_path = out_dir.join(&vol_name);

            if layout_exp == vol.first {
                // First slice of the volume — check overwrite and open fresh.
                if !cli.overwrite && vol_path.exists() {
                    anyhow::bail!(
                        "output file already exists: `{}`; use --overwrite to replace it",
                        vol_path.display()
                    );
                }
                tokio::fs::write(&vol_path, &base_packets).await?;
            }

            let mut f = tokio::fs::OpenOptions::new()
                .append(true)
                .open(&vol_path)
                .await?;
            let pkt = packet::serialize_packet(
                &rsid,
                &packet::TYPE_RECOVERY,
                &packet::recovery_body(abs_exp, &slice.data),
            );
            f.write_all(&pkt).await?;

            if layout_exp == vol.first + vol.count - 1 && !cli.quiet {
                println!("Finished {}", vol_path.display());
            }
        }

        cursor += count;
    }

    if !cli.quiet {
        println!("\nAll recovery volumes created successfully.");
    }
    Ok(())
}
