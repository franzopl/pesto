mod memory;

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use parmesan::create::{
    create_with_options, CreateEvent, CreateRequest, EngineOptions, OutputPolicy, Recovery,
    SliceStrategy,
};
use parmesan::recovery_set::RecoverySet;
use parmesan::repair::{self, RepairOptions};
use parmesan::verify::{self, FileStatus, VerifyReport};
use parmesan::{EncoderLayout, SimdPath};
use std::path::PathBuf;

#[derive(Parser, Debug)]
#[command(
    name = "parmesan",
    version = parmesan::DISPLAY_VERSION,
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

    /// Recovery-buffer layout. `smart` is the production auto path.
    /// `affine512` is the packed AVX-512+GFNI kernel (not auto on SPR yet).
    #[arg(long, value_enum, default_value_t = EncoderLayout::Smart)]
    encoder: EncoderLayout,

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

    /// Emit a machine-readable JSON report instead of human-readable text.
    #[arg(long)]
    json: bool,
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

fn main() -> Result<()> {
    // Must run before any thread exists — see `memory` module docs (#137).
    memory::tune_allocator();
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

    // A hand-built runtime instead of `#[tokio::main]`'s default, which
    // sizes worker threads to `nproc` — see `memory::build_runtime`.
    let rt = memory::build_runtime()?;
    rt.block_on(async move {
        match cli.command {
            Command::Create(args) => run_create(args).await,
            Command::Verify(args) => run_verify(args),
            Command::Repair(args) => run_repair(args),
        }
    })
}

async fn run_create(cli: CreateArgs) -> Result<()> {
    let recovery = match cli.recovery_count {
        Some(count) => Recovery::Blocks(
            u16::try_from(count).context("recovery block count must not exceed 65535")?,
        ),
        None => Recovery::Percentage(cli.recovery_pct),
    };
    let slice_strategy = match (cli.slice_size.as_deref(), cli.slice_count) {
        (Some(size), _) => SliceStrategy::Size(parse_size(size)? as usize),
        (None, Some(count)) => SliceStrategy::Count(count),
        (None, None) => SliceStrategy::Automatic,
    };
    let memory_limit = parse_size(&cli.memory_limit)? as usize;
    let mut request = CreateRequest::from_paths(&cli.files)
        .recovery(recovery)
        .slice_strategy(slice_strategy)
        .memory_limit(memory_limit)
        .output_policy(if cli.overwrite {
            OutputPolicy::ReplaceExisting
        } else {
            OutputPolicy::FailIfExists
        })
        .creator(if cli.comment.is_empty() {
            "parmesan".to_owned()
        } else {
            format!("parmesan | {}", cli.comment.join(" | "))
        })
        .recovery_offset(
            u32::try_from(cli.recovery_offset)
                .context("recovery offset exceeds the PAR2 exponent range")?,
        );
    if let Some(out_dir) = &cli.out_dir {
        request = request.output_dir(out_dir);
    }
    if let Some(base_name) = &cli.base_name {
        request = request.base_name(base_name);
    }
    if let Some(threads) = cli.threads.and_then(std::num::NonZeroUsize::new) {
        request = request.threads(threads);
    }
    if cli.recurse {
        request = request.recurse();
    }

    let quiet = cli.quiet;
    create_with_options(
        request,
        EngineOptions {
            simd: cli.simd,
            layout: cli.encoder,
            write_index: !cli.no_index,
        },
        |event| {
            if quiet {
                return;
            }
            match event {
                CreateEvent::Planned {
                    input_files,
                    geometry,
                    ..
                } => {
                    println!("PAR2 Geometry:");
                    println!("  Input files    : {input_files}");
                    println!("  Input slices   : {}", geometry.input_slices);
                    println!("  Recovery blocks: {}", geometry.recovery_blocks);
                    println!("  Slice size     : {} bytes", geometry.slice_size);
                    if cli.recovery_offset > 0 {
                        println!("  Recovery offset: {}", cli.recovery_offset);
                    }
                    let memory_plan = parmesan::ops::plan_memory_layout(
                        geometry.slice_size,
                        geometry.recovery_blocks,
                        memory_limit,
                    );
                    if memory_plan.slice_chunk < geometry.slice_size {
                        println!(
                            "  Memory plan    : slice-chunk {} bytes × {} recovery (limit {})",
                            memory_plan.slice_chunk, memory_plan.recovery_per_pass, memory_limit
                        );
                    }
                }
                CreateEvent::PassStarted {
                    pass,
                    first_exponent,
                    recovery_blocks,
                } => println!(
                    "\nPass {} (recovery blocks {}-{}):",
                    pass + 1,
                    first_exponent,
                    first_exponent + recovery_blocks as u32 - 1
                ),
                CreateEvent::IndexWritten { path } => println!("Wrote {}", path.display()),
                CreateEvent::VolumeWritten { path } => println!("Finished {}", path.display()),
                CreateEvent::BytesRead { .. } => {}
                _ => {}
            }
        },
    )
    .await?;
    if !quiet {
        println!("\nAll recovery volumes created successfully.");
    }
    Ok(())
}

fn run_verify(args: VerifyArgs) -> Result<()> {
    let set = RecoverySet::load_metadata(&args.index)
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
    let mut set = RecoverySet::load_metadata(&args.index)
        .with_context(|| format!("loading recovery set from `{}`", args.index.display()))?;
    let base_dir = args
        .index
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));

    let report = verify::verify(&set, &base_dir)?;

    if report.is_ok() {
        if args.json {
            println!(
                r#"{{"repaired_files":[],"dry_run":{},"needed_repair":false}}"#,
                args.dry_run
            );
        } else if !args.quiet {
            println!("All files verified OK — nothing to repair.");
        }
        return Ok(());
    }
    if !report.is_repairable() {
        if args.json {
            println!(
                r#"{{"error":"not enough recovery data","bad_slices":{},"available_recovery_blocks":{}}}"#,
                report.total_bad_slices(),
                report.available_recovery_blocks
            );
        }
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
    set.load_recovery_blocks(Some(report.total_bad_slices()))?;
    let plan = repair::repair(&set, &report, &base_dir, &options)?;

    if args.json {
        print_json_repair_plan(&plan);
        return Ok(());
    }

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

fn print_json_repair_plan(plan: &repair::RepairPlan) {
    let mut out = String::from("{\"repaired_files\":[");
    for (i, f) in plan.repaired_files.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&format!(
            "{{\"name\":{},\"path\":{},\"slices_repaired\":{},\"verified\":{}}}",
            json_string(&f.name),
            json_string(&f.path.display().to_string()),
            f.slices_repaired,
            f.verified
        ));
    }
    out.push_str(&format!(
        "],\"dry_run\":{},\"needed_repair\":true}}",
        plan.dry_run
    ));
    println!("{out}");
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
