//! Completion notifications, NFO generation and post-upload hooks.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use pesto::config::ObfuscateMode;

use super::super::{nfo_metadata_header, run_all_hooks, HookEnv};
use super::UploadParams;

pub(crate) struct CompletionRequest<'a> {
    pub(crate) params: &'a UploadParams,
    pub(crate) entry_paths: &'a [PathBuf],
    pub(crate) entry_label: &'a str,
    pub(crate) original_inputs: &'a [pesto::walk::InputFile],
    pub(crate) effective_password: Option<&'a str>,
    pub(crate) outcome: &'a pesto::poster::PostOutcome,
    pub(crate) nzb_reported_path: Option<&'a Path>,
    pub(crate) cancelled: bool,
    pub(crate) has_post_failures: bool,
    pub(crate) has_confirmed_missing: bool,
    pub(crate) has_inconclusive: bool,
    pub(crate) has_unrecoverable_failures: bool,
    pub(crate) total_bytes: u64,
}

pub(crate) async fn run(request: CompletionRequest<'_>) -> Result<()> {
    let CompletionRequest {
        params,
        entry_paths,
        entry_label,
        original_inputs,
        effective_password,
        outcome,
        nzb_reported_path,
        cancelled,
        has_post_failures,
        has_confirmed_missing,
        has_inconclusive,
        has_unrecoverable_failures,
        total_bytes,
    } = request;
    let config = &params.config;

    let notify_enabled = config.notify.unwrap_or(true)
        && (config.notify_webhook.is_some() || config.notify_ntfy.is_some());
    if notify_enabled && !config.par2_only && !config.dry_run && !cancelled {
        let had_failures = !outcome.failures.is_empty()
            || has_post_failures
            || has_confirmed_missing
            || has_inconclusive;
        pesto::notify::send_all(&pesto::notify::NotifyConfig {
            webhook_url: config.notify_webhook.as_deref(),
            ntfy_topic: config.notify_ntfy.as_deref(),
            name: entry_label,
            total_bytes,
            group: outcome.groups.first().map(String::as_str),
            category: config.nzb_category.as_deref(),
            ok: !had_failures,
        })
        .await;
    }

    let upload_ok = !cancelled && outcome.failures.is_empty() && !has_unrecoverable_failures;
    let nfo_path = generate_nfo(
        config,
        entry_paths,
        entry_label,
        nzb_reported_path,
        upload_ok,
    )
    .await?;

    if upload_ok && !config.par2_only && !config.dry_run {
        let input_paths = original_inputs
            .iter()
            .map(|input| input.path.to_string_lossy().into_owned())
            .collect::<Vec<_>>()
            .join(":");
        let obfuscate = match config.obfuscate {
            ObfuscateMode::None => "none",
            ObfuscateMode::Full => "full",
            ObfuscateMode::Light => "light",
            ObfuscateMode::FullShared => "full-shared",
            ObfuscateMode::Article => "article",
        };
        let groups = outcome.groups.join(":");
        let servers = outcome.servers.join(":");
        let tags = config.nzb_tags.join(" ");
        let hook_env = HookEnv {
            nzb_path: nzb_reported_path,
            nfo_path: nfo_path.as_deref(),
            name: entry_label,
            total_bytes,
            input_paths: &input_paths,
            group: outcome.groups.first().map(String::as_str),
            groups: &groups,
            password: effective_password,
            server: servers.split(':').next().unwrap_or(&config.host),
            servers: &servers,
            category: config.nzb_category.as_deref(),
            nzb_title: config.nzb_title.as_deref(),
            obfuscate,
            par2: config.par2,
            tags: &tags,
            tmdb_id: config.tmdb_id.as_deref(),
            imdb_id: config.imdb_id.as_deref(),
            tvdb_id: config.tvdb_id.as_deref(),
            mal_id: config.mal_id.as_deref(),
            incomplete: has_confirmed_missing,
        };
        run_all_hooks(config, &hook_env);
    }

    Ok(())
}

async fn generate_nfo(
    config: &pesto::config::Config,
    entry_paths: &[PathBuf],
    entry_label: &str,
    nzb_reported_path: Option<&Path>,
    upload_ok: bool,
) -> Result<Option<PathBuf>> {
    if !config.nfo || !upload_ok || config.par2_only {
        return Ok(None);
    }
    let Some(output) = nzb_reported_path
        .map(|path| path.with_extension("nfo"))
        .or_else(|| {
            entry_paths
                .first()
                .and_then(|path| path.parent())
                .map(|directory| directory.join(format!("{entry_label}.nfo")))
        })
    else {
        return Ok(None);
    };

    if pesto::nfo::looks_like_bluray(entry_paths) {
        println!(
            "generating nfo (running bdinfo — this can take a while on large Blu-ray discs)..."
        );
    } else {
        println!("generating nfo...");
    }
    pesto::memory::set_phase(pesto::memory::Phase::Nfo);
    let paths = entry_paths.to_vec();
    let handle = tokio::task::spawn_blocking(move || pesto::nfo::generate(&paths));
    tokio::pin!(handle);
    let content = loop {
        tokio::select! {
            result = &mut handle => break result.context("nfo generation task panicked")?,
            _ = tokio::time::sleep(std::time::Duration::from_secs(10)) => {
                println!("... still generating nfo, please wait");
            }
        }
    };

    match content {
        Some(content) => match pesto::nfo::write(
            &output,
            &format!("{}{content}", nfo_metadata_header(config)),
        ) {
            Ok(()) => {
                println!("wrote nfo:  {}", output.display());
                Ok(Some(output))
            }
            Err(error) => {
                eprintln!("nfo write failed: {error}");
                Ok(None)
            }
        },
        None => {
            eprintln!("nfo: no content generated for the given paths");
            Ok(None)
        }
    }
}
