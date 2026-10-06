use std::process;

use anyhow::{Context, Result};

use super::cli::{Cli, Command, MediaInfoFormat};
use super::{check, download, info};

/// Exit codes for the download command.
pub(super) const EXIT_COMPLETE: i32 = 0;
pub(super) const EXIT_REPAIRED: i32 = 1;
pub(super) const EXIT_INCOMPLETE: i32 = 2;
pub(super) const EXIT_FATAL: i32 = 3;

pub(super) async fn run(cli: Cli) -> Result<()> {
    pesto::logging::init(cli.verbose, cli.log_file.as_deref(), None)?;

    // `penne --config` with no value: launch the interactive setup wizard,
    // regardless of whether a subcommand was also given.
    if matches!(cli.config, Some(None)) {
        return penne::wizard::run();
    }

    match cli.command {
        Some(Command::Info { nzb }) => info::run(&nzb),
        Some(Command::MediaInfo {
            nzb,
            max_bytes,
            file,
            password,
            format,
            output,
            server,
        }) => {
            let config_path = cli
                .config
                .flatten()
                .or_else(penne::config::default_config_path)
                .ok_or_else(|| anyhow::anyhow!("cannot locate config; use --config <FILE>"))?;
            let config_toml = std::fs::read_to_string(&config_path).map_err(|e| {
                anyhow::anyhow!(
                    "reading {}: {e}; run `penne --config` to configure servers",
                    config_path.display()
                )
            })?;
            let config = penne::config::RawConfig::parse(&config_toml)?
                .select(&server)?
                .resolve()?;
            let parsed = penne::nzb::load(&nzb)?;
            let password = password.or(parsed.meta.password.clone());
            let queue = penne::queue::build(&parsed);
            let report = penne::mediainfo::inspect(
                &queue,
                &config,
                &penne::mediainfo::Options {
                    max_bytes,
                    file,
                    text: matches!(format, MediaInfoFormat::Text),
                    password,
                },
            )
            .await?;
            eprintln!(
                "MediaInfo: {} bytes in {} article(s); {} sampled bytes of {}{}",
                report.downloaded_bytes,
                report.fetched_articles,
                report.sampled_bytes,
                report.file_size,
                if report.partial {
                    " (partial metadata)"
                } else {
                    ""
                }
            );
            let rendered = match format {
                MediaInfoFormat::Json => serde_json::to_string_pretty(&report)?,
                MediaInfoFormat::Text => report.text.context("MediaInfo text report is missing")?,
            };
            let rendered = format!("{}\n", rendered.trim_end());
            if let Some(path) = output {
                std::fs::write(&path, &rendered)
                    .with_context(|| format!("saving MediaInfo report to {}", path.display()))?;
            }
            print!("{rendered}");
            Ok(())
        }
        Some(Command::Download {
            nzb,
            out_dir,
            password,
            stat,
            sample,
            server,
            mode,
            quiet,
        }) => {
            let stat = stat.map(|inner| inner.unwrap_or_default());
            let multi = nzb.len() > 1;
            let mut worst_exit_code = EXIT_COMPLETE;
            for (i, path) in nzb.iter().enumerate() {
                if multi {
                    println!("=== [{}/{}] {} ===", i + 1, nzb.len(), path.display());
                }
                // Beyond the first release, each gets its own subdirectory
                // (named after its .nzb's stem) under the shared destination
                // so same-named files across releases can never collide. A
                // single .nzb keeps the old flat destination unchanged.
                let subdir = multi.then(|| {
                    path.file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("release")
                        .to_string()
                });
                let exit_code = download::run(
                    path,
                    out_dir.clone(),
                    cli.config.clone().flatten(),
                    password.clone(),
                    stat,
                    sample,
                    &server,
                    mode,
                    quiet,
                    subdir.as_deref(),
                )
                .await
                .unwrap_or_else(|e| {
                    eprintln!("error: {e:#}");
                    EXIT_FATAL
                });
                worst_exit_code = worst_exit_code.max(exit_code);
            }
            process::exit(worst_exit_code);
        }
        Some(Command::Check {
            nzb,
            method,
            sample,
            pipeline_depth,
            fail_fast,
            json,
            quiet,
            server,
            independent_servers,
        }) => {
            let exit_code = check::run(
                &nzb,
                method,
                sample,
                pipeline_depth,
                fail_fast,
                json,
                quiet,
                cli.config.flatten(),
                &server,
                independent_servers,
            )
            .await
            .unwrap_or_else(|e| {
                eprintln!("error: {e:#}");
                2
            });
            process::exit(exit_code);
        }
        None => {
            println!(
                "penne — fast NZB downloader.\n\n\
                 Run `penne --help` for usage, or `penne --config` to set up your servers."
            );
            Ok(())
        }
    }
}
