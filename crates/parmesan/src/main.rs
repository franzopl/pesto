use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use parmesan::ops::{
    calculate_geometry, ingest_files, sort_files_by_file_id, CreateOptions, InputFile,
};
use parmesan::recovery_set::RecoverySet;
use parmesan::repair::{self, RepairOptions};
use parmesan::verify::{self, FileStatus, VerifyReport};
use parmesan::worker::Par2Worker;
use parmesan::{encoder::RecoveryEncoder, layout, packet, SimdPath};
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
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Create a new PAR2 recovery set for the given files (default action).
    Create(CreateArgs),
    /// Verify files against an existing PAR2 recovery set.
    Verify(VerifyArgs),
    /// Repair damaged or missing files using an existing PAR2 recovery set.
    Repair(RepairArgs),
}

#[derive(Args, Debug)]
struct CreateArgs {
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
    #[arg(long)]
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

#[derive(Args, Debug)]
struct VerifyArgs {
    /// Path to the PAR2 index file (e.g. "movie.mkv.par2").
    index: PathBuf,

    /// Suppress the per-file report; only the summary line is printed.
    #[arg(short = 'q', long)]
    quiet: bool,

    /// Emit a machine-readable JSON report instead of human-readable text.
    #[arg(long)]
    json: bool,
}

#[derive(Args, Debug)]
struct RepairArgs {
    /// Path to the PAR2 index file (e.g. "movie.mkv.par2").
    index: PathBuf,

    /// Report what would be repaired without writing any files.
    #[arg(long)]
    dry_run: bool,

    /// Write repaired files under this directory instead of overwriting
    /// damaged/missing originals in place.
    #[arg(short, long)]
    out_dir: Option<PathBuf>,

    /// Suppress the per-file report; only the summary line is printed.
    #[arg(short = 'q', long)]
    quiet: bool,
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

/// Subcommand names that must never be preceded by an implicit `create`.
const KNOWN_FIRST_ARGS: [&str; 8] = [
    "create",
    "verify",
    "repair",
    "help",
    "-h",
    "--help",
    "-V",
    "--version",
];

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    // Bare invocation (`parmesan <files>...`) aliases to `create` for
    // backwards compatibility with versions before subcommands existed.
    let mut args: Vec<String> = std::env::args().collect();
    if let Some(first) = args.get(1) {
        if !KNOWN_FIRST_ARGS.contains(&first.as_str()) {
            args.insert(1, "create".to_string());
        }
    }
    let cli = Cli::parse_from(args);

    match cli.command {
        Command::Create(args) => run_create(args).await,
        Command::Verify(args) => run_verify(args),
        Command::Repair(args) => run_repair(args),
    }
}

async fn run_create(cli: CreateArgs) -> Result<()> {
    let mut input_files = collect_files(&cli.files, cli.recurse)?;
    if input_files.is_empty() {
        anyhow::bail!("no input files found");
    }
    // The default output base name follows the first file as given on the
    // command line — a naming choice, independent of Reed-Solomon ordering.
    // Captured before the sort below so it doesn't start depending on file
    // content.
    let default_base_name = input_files[0].display_name.clone();
    // Reed-Solomon coefficients are assigned by ascending File ID, per the
    // PAR2 spec — not by command-line/directory order. See
    // `ops::sort_files_by_file_id` for why this matters for multi-file sets.
    tokio::task::block_in_place(|| sort_files_by_file_id(&mut input_files))?;

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
        parmesan::performance_core_count()
    };
    let _ = rayon::ThreadPoolBuilder::new()
        .num_threads(rayon_threads)
        .build_global();

    let out_dir = cli.out_dir.clone().unwrap_or_else(|| PathBuf::from("."));
    if !out_dir.exists() {
        std::fs::create_dir_all(&out_dir)?;
    }

    let base_name = cli.base_name.clone().unwrap_or(default_base_name);

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

fn run_verify(args: VerifyArgs) -> Result<()> {
    let set = RecoverySet::load(&args.index)
        .with_context(|| format!("loading recovery set from `{}`", args.index.display()))?;
    let base_dir = args
        .index
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let report = verify::verify(&set, &base_dir)?;

    if args.json {
        print_json_report(&report);
    } else {
        if !args.quiet {
            for f in &report.files {
                let status = match f.status {
                    FileStatus::Ok => "OK",
                    FileStatus::Damaged => "DAMAGED",
                    FileStatus::Missing => "MISSING",
                };
                println!(
                    "{status:<8} {} ({}/{} slices ok)",
                    f.name,
                    f.total_slices - f.bad_slices,
                    f.total_slices
                );
            }
        }
        if report.is_ok() {
            println!("\nAll files verified OK.");
        } else if report.is_repairable() {
            println!(
                "\n{} slice(s) need repair; {} recovery block(s) available — repairable.",
                report.total_bad_slices(),
                report.available_recovery_blocks
            );
        } else {
            println!(
                "\n{} slice(s) need repair; only {} recovery block(s) available — NOT repairable.",
                report.total_bad_slices(),
                report.available_recovery_blocks
            );
        }
    }

    std::process::exit(report.exit_code());
}

fn run_repair(args: RepairArgs) -> Result<()> {
    let set = RecoverySet::load(&args.index)
        .with_context(|| format!("loading recovery set from `{}`", args.index.display()))?;
    let base_dir = args
        .index
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let report = verify::verify(&set, &base_dir)?;

    if report.is_ok() {
        if !args.quiet {
            println!("All files verified OK — nothing to repair.");
        }
        return Ok(());
    }
    if !report.is_repairable() {
        anyhow::bail!(
            "not enough recovery data to repair: {} bad slice(s), only {} recovery block(s) available",
            report.total_bad_slices(),
            report.available_recovery_blocks
        );
    }

    let options = RepairOptions {
        out_dir: args.out_dir.clone(),
        dry_run: args.dry_run,
    };
    let plan = repair::repair(&set, &report, &base_dir, &options)?;

    if !args.quiet {
        let verb = if plan.dry_run {
            "Would repair"
        } else {
            "Repaired"
        };
        for f in &plan.repaired_files {
            println!(
                "{verb:<13} {} ({} slice(s)) -> {}",
                f.name,
                f.slices_repaired,
                f.path.display()
            );
        }
    }

    if plan.dry_run {
        println!(
            "\nDry run: {} file(s) would be repaired.",
            plan.repaired_files.len()
        );
    } else {
        println!(
            "\n{} file(s) repaired successfully.",
            plan.repaired_files.len()
        );
    }

    Ok(())
}

fn print_json_report(report: &VerifyReport) {
    let mut out = String::from("{\"files\":[");
    for (i, f) in report.files.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        let status = match f.status {
            FileStatus::Ok => "ok",
            FileStatus::Damaged => "damaged",
            FileStatus::Missing => "missing",
        };
        out.push_str(&format!(
            "{{\"name\":{},\"status\":\"{}\",\"total_slices\":{},\"bad_slices\":{}}}",
            json_string(&f.name),
            status,
            f.total_slices,
            f.bad_slices
        ));
    }
    out.push_str(&format!(
        "],\"available_recovery_blocks\":{},\"repairable\":{},\"ok\":{}}}",
        report.available_recovery_blocks,
        report.is_repairable(),
        report.is_ok()
    ));
    println!("{out}");
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
