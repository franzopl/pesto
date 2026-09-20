use super::cli::{Command, CreateArgs, RepairArgs, VerifyArgs};
use anyhow::{Context, Result};
use parmesan::create::{
    create_with_options, CreateEvent, CreateRequest, EngineOptions, OutputPolicy, Recovery,
    SliceStrategy,
};
use parmesan::recovery_set::RecoverySet;
use parmesan::repair::{self, RepairOptions};
use parmesan::verify::{self, FileStatus, VerifyReport};
use std::path::PathBuf;

pub(super) async fn run(command: Command) -> Result<()> {
    match command {
        Command::Create(args) => run_create(args).await,
        Command::Verify(args) => run_verify(args),
        Command::Repair(args) => run_repair(args),
    }
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
