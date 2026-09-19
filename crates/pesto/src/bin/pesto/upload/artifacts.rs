//! NZB persistence and upload-history recording.

use std::path::PathBuf;

use anyhow::{Context, Result};
use pesto::nzb::NzbMeta;

use super::super::add_obfuscation_tag;
use super::super::output::{nzb_archive_path, resolve_nzb_dest};
use super::UploadParams;

pub(crate) struct ArtifactRequest<'a> {
    pub(crate) params: &'a UploadParams,
    pub(crate) nzb_out_path: Option<String>,
    pub(crate) nzb_user_dest: Option<PathBuf>,
    pub(crate) has_unrecoverable_failures: bool,
    pub(crate) effective_password: Option<&'a str>,
    pub(crate) outcome: &'a pesto::poster::PostOutcome,
    pub(crate) entry_label: &'a str,
    pub(crate) total_bytes: u64,
    pub(crate) duration_secs: f64,
}

pub(crate) async fn write(request: ArtifactRequest<'_>) -> Result<Option<PathBuf>> {
    let ArtifactRequest {
        params,
        nzb_out_path,
        nzb_user_dest,
        has_unrecoverable_failures,
        effective_password,
        outcome,
        entry_label,
        total_bytes,
        duration_secs,
    } = request;
    let Some(stem) = nzb_out_path else {
        return Ok(nzb_user_dest);
    };
    let archive_path = nzb_archive_path(&stem).await;
    let mut reported_path = nzb_user_dest.clone().or_else(|| Some(archive_path.clone()));

    if params.config.par2_only {
        return Ok(reported_path);
    }
    if has_unrecoverable_failures {
        eprintln!("skipping nzb output — upload incomplete");
        return Ok(None);
    }
    if outcome.segments.is_empty() {
        eprintln!("no segments posted — skipping nzb output");
        return Ok(None);
    }

    let config = &params.config;
    let mut nzb_tags = config.nzb_tags.clone();
    add_obfuscation_tag(&mut nzb_tags, &config.obfuscate);
    let metadata = NzbMeta {
        name: config.nzb_title.clone(),
        password: config
            .nzb_password
            .clone()
            .or_else(|| effective_password.map(str::to_string)),
        category: config.nzb_category.clone(),
        tmdb_id: config.tmdb_id.clone(),
        imdb_id: config.imdb_id.clone(),
        tvdb_id: config.tvdb_id.as_deref().map(|id| {
            format!(
                "{}/{id}",
                config
                    .tvdb_kind
                    .unwrap_or(pesto::nzb::TvdbKind::Series)
                    .as_str()
            )
        }),
        mal_id: config.mal_id.clone(),
        tags: nzb_tags,
    };
    let xml = pesto::nzb::generate(
        &outcome.groups,
        &outcome.segments,
        &metadata,
        config.obfuscate,
    );
    tokio::fs::write(&archive_path, &xml)
        .await
        .with_context(|| format!("writing nzb file `{}`", archive_path.display()))?;

    if let Some(destination) = &nzb_user_dest {
        if let Some(parent) = destination.parent() {
            let _ = tokio::fs::create_dir_all(parent).await;
        }
        let effective_destination = resolve_nzb_dest(destination, config.nzb_conflict).await?;
        if std::fs::hard_link(&archive_path, &effective_destination).is_err() {
            std::fs::copy(&archive_path, &effective_destination)
                .with_context(|| format!("copying nzb to `{}`", effective_destination.display()))?;
        }
        reported_path = Some(effective_destination);
    }

    let reported = reported_path.as_deref().unwrap_or(&archive_path);
    if params.json_mode {
        let escaped = reported
            .display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"");
        println!(r#"{{"type":"nzb_written","path":"{escaped}"}}"#);
    } else {
        println!("wrote nzb: {}", reported.display());
    }

    if params.write_history && !config.dry_run {
        record_history(
            config,
            outcome,
            entry_label,
            effective_password,
            total_bytes,
            duration_secs,
            reported,
        );
    }

    Ok(reported_path)
}

fn record_history(
    config: &pesto::config::Config,
    outcome: &pesto::poster::PostOutcome,
    entry_label: &str,
    effective_password: Option<&str>,
    total_bytes: u64,
    duration_secs: f64,
    nzb_path: &std::path::Path,
) {
    let obfuscated_name =
        (config.obfuscate != pesto::config::ObfuscateMode::None).then_some(entry_label);
    let par2_redundancy = (config.par2 > 0).then(|| format!("{}%", config.par2));
    let servers = outcome.servers.join(", ");
    let wire_subjects = pesto::nzb::wire_subjects(&outcome.segments);
    pesto::history::record_upload(
        &pesto::history::UploadRecord {
            name: entry_label,
            obfuscated_name,
            password: effective_password,
            total_bytes,
            group: outcome.groups.first().map(String::as_str),
            server: (!servers.is_empty()).then_some(servers.as_str()),
            par2_redundancy: par2_redundancy.as_deref(),
            duration_secs,
            nzb_path: Some(&nzb_path.display().to_string()),
            subject: config.nzb_title.as_deref().or(Some(entry_label)),
            wire_subjects: &wire_subjects,
        },
        config.history_dir.as_deref(),
    );
}
