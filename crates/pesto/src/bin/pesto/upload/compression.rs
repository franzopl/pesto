//! Compression execution for one upload entry.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pesto::compress::{compress, existing_archive, ArchiveFormat};
use pesto::config::{Config, ObfuscateMode};
use tracing::info;

use super::{collect_compress_roots, reuse_or_generate_archive_stem, upload_root};

pub(crate) struct CompressionOutcome {
    pub(crate) inputs: Vec<pesto::walk::InputFile>,
    pub(crate) temp_dir: Option<PathBuf>,
    pub(crate) light_prefix: Option<String>,
    pub(crate) elapsed_ms: Option<u128>,
}

pub(crate) async fn run(
    config: &Config,
    inputs: Vec<pesto::walk::InputFile>,
    entry_label: &str,
    resume_path: Option<&Path>,
    effective_password: Option<&str>,
    password_was_generated: bool,
    progress_tx: &pesto::progress::ProgressSender,
) -> Result<CompressionOutcome> {
    let format_name = config
        .compress_format
        .as_deref()
        .or_else(|| effective_password.map(|_| "7z"));
    let Some(format_name) = format_name else {
        return Ok(CompressionOutcome {
            inputs,
            temp_dir: None,
            light_prefix: None,
            elapsed_ms: None,
        });
    };

    let format = ArchiveFormat::parse(format_name).ok_or_else(|| {
        anyhow::anyhow!("unknown compression format `{format_name}`; supported: 7z, zip, rar")
    })?;
    if format == ArchiveFormat::Rar && pesto::compress::find_binary("rar").is_none() {
        eprintln!("note: rar password protection requires the `rar` binary in PATH");
    }

    let client_stem = upload_root(&inputs)
        .or_else(|| {
            inputs.first().map(|input| {
                PathBuf::from(&input.name)
                    .file_stem()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .into_owned()
            })
        })
        .unwrap_or_else(|| "archive".to_string());
    let client_stem = pesto::compress::portable_archive_stem(&client_stem);
    let archive_stem = if config.obfuscate != ObfuscateMode::None {
        reuse_or_generate_archive_stem(resume_path, config)
    } else {
        client_stem.clone()
    };
    let light_prefix = (config.obfuscate == ObfuscateMode::Light).then(|| archive_stem.clone());

    let temp_base = config
        .compress_temp_dir
        .clone()
        .unwrap_or_else(std::env::temp_dir);
    let temp_dir = if config.resume {
        temp_base.join(format!("pesto_compress_{archive_stem}"))
    } else {
        temp_base.join(format!(
            "pesto_compress_{}_{}",
            std::process::id(),
            entry_label
        ))
    };

    let filesystem_inputs = collect_compress_roots(&inputs);
    let input_bytes = filesystem_inputs.iter().map(|path| path_size(path)).sum();
    let started = std::time::Instant::now();
    let _ = progress_tx.send(pesto::progress::ProgressEvent::CompressStarted {
        total_bytes: input_bytes,
    });

    let poll_tx = progress_tx.clone();
    let poll_dir = temp_dir.clone();
    let poll_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            let bytes_written = path_size(&poll_dir);
            let _ =
                poll_tx.send(pesto::progress::ProgressEvent::CompressProgress { bytes_written });
        }
    });

    let volume_size = config.compress_volume_size.clone();
    let existing = config
        .resume
        .then(|| existing_archive(&temp_dir, &archive_stem, format, volume_size.as_deref()));
    let archive = if let Some(reused) = existing.flatten() {
        eprintln!(
            "resume: reusing existing archive `{}`",
            reused.path.display()
        );
        reused
    } else {
        let compress_dest = temp_dir.clone();
        let compress_stem = archive_stem.clone();
        let compress_password = effective_password.map(str::to_string);
        tokio::task::spawn_blocking(move || {
            compress(
                &filesystem_inputs,
                &compress_stem,
                &compress_dest,
                format,
                compress_password.as_deref(),
                volume_size.as_deref(),
            )
        })
        .await
        .context("compressor task panicked")??
    };

    poll_handle.abort();
    let _ = progress_tx.send(pesto::progress::ProgressEvent::CompressDone);
    let elapsed_ms = started.elapsed().as_millis();
    info!(elapsed_ms, phase = "compress", "phase done");

    let inputs = std::iter::once(archive.path)
        .chain(archive.extra_paths)
        .map(|path| {
            let published_stem = if config.obfuscate == ObfuscateMode::Light {
                &archive_stem
            } else {
                &client_stem
            };
            let name = pesto::compress::client_archive_name(&path, &archive_stem, published_stem);
            pesto::walk::InputFile { path, name }
        })
        .collect();

    if password_was_generated {
        if let Some(password) = effective_password {
            println!("archive password: {password}");
        }
    }

    Ok(CompressionOutcome {
        inputs,
        temp_dir: Some(temp_dir),
        light_prefix,
        elapsed_ms: Some(elapsed_ms),
    })
}

fn path_size(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Err(_) => 0,
        Ok(metadata) if metadata.is_file() => metadata.len(),
        Ok(_) => std::fs::read_dir(path)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| path_size(&entry.path()))
            .sum(),
    }
}
