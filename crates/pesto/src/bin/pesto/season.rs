//! Season-wide PAR2 generation and upload.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use pesto::poster::PostedSegment;
use tracing::info;

use super::UploadParams;

/// Generate and post the PAR2 volumes that protect an entire season batch.
pub(super) async fn post_season_par2_volumes(
    episode_paths: &[PathBuf],
    release_name: &str,
    params: &Arc<UploadParams>,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
) -> Result<Vec<PostedSegment>> {
    if episode_paths.is_empty() || params.config.par2 == 0 {
        return Ok(Vec::new());
    }

    let (progress_tx, renderer) = if params.json_mode {
        pesto::progress::spawn_json_emitter()
    } else {
        pesto::ui::terminal::spawn_renderer_with(params.renderer_opts.clone())
    };
    let result = post_season_par2_volumes_with_progress(
        episode_paths,
        release_name,
        params,
        cancel,
        progress_tx,
    )
    .await;
    let _ = renderer.await;
    result
}

async fn post_season_par2_volumes_with_progress(
    episode_paths: &[PathBuf],
    release_name: &str,
    params: &Arc<UploadParams>,
    cancel: &Arc<std::sync::atomic::AtomicBool>,
    progress_tx: pesto::progress::ProgressSender,
) -> Result<Vec<PostedSegment>> {
    if episode_paths.is_empty() || params.config.par2 == 0 {
        let _ = progress_tx.send(pesto::progress::ProgressEvent::Finished);
        return Ok(Vec::new());
    }

    let par2_output_dir =
        tempfile::tempdir().context("creating directory for season PAR2 volumes")?;
    let par2_dir_path = par2_output_dir.path().to_path_buf();

    let generation = pesto::poster::generate_and_write_season_par2_with_progress(
        episode_paths,
        release_name,
        &par2_dir_path,
        &params.config,
        Some(&progress_tx),
    )
    .await;
    if let Err(error) = generation {
        let _ = progress_tx.send(pesto::progress::ProgressEvent::Failed {
            description: format!("season PAR2 generation failed: {error:#}"),
        });
        let _ = progress_tx.send(pesto::progress::ProgressEvent::Finished);
        return Err(error);
    }

    let par2_files: Vec<PathBuf> = std::fs::read_dir(&par2_dir_path)?
        .filter_map(|entry| {
            entry.ok().and_then(|entry| {
                let path = entry.path();
                if path
                    .extension()
                    .is_some_and(|extension| extension == "par2")
                {
                    Some(path)
                } else {
                    None
                }
            })
        })
        .collect();

    if par2_files.is_empty() {
        let _ = progress_tx.send(pesto::progress::ProgressEvent::Finished);
        return Ok(Vec::new());
    }

    let _ = progress_tx.send(pesto::progress::ProgressEvent::Status {
        text: format!("Uploading {} season recovery volume(s)", par2_files.len()),
    });

    // Prevent recursively generating recovery data for the recovery volumes.
    let mut par2_config = (*params.config).clone();
    par2_config.par2 = 0;

    // `run_upload` derives compression from the config again. Leaving the
    // season compression settings enabled would encrypt each generated PAR2
    // volume and make the recovery set unusable by downloaders.
    par2_config.compress_format = None;
    par2_config.compress_password = None;
    par2_config.compress_volume_size = None;

    // `no_hooks` suppresses directory hooks, but explicit hooks still run.
    // Disable both paths so the internal PAR2-only NZB is never submitted to
    // an indexer ahead of the real season NZB.
    par2_config.no_hooks = true;
    par2_config.post_hooks = Vec::new();

    // Keep the internal NZB in the TempDir. `run_upload` may version the path,
    // and placing every possible sibling here guarantees cleanup on return.
    let par2_nzb_path = par2_dir_path.join("season-par2.nzb");

    let outcome = pesto::upload::run_upload(
        &par2_config,
        &par2_files,
        "season-par2",
        Some(progress_tx),
        Some(cancel.clone()),
        Some(par2_nzb_path),
        false,
        None,
    )
    .await?;

    info!(
        par2_segments = outcome.segments.len(),
        par2_volumes = par2_files.len(),
        "season PAR2 upload complete"
    );

    Ok(outcome.segments)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicBool;

    use pesto::config::{Config, FileConfig, ObfuscateMode, Overrides};

    use super::*;
    use crate::cleanup::CleanupMode;

    fn test_config() -> Config {
        let mut file = FileConfig::default();
        file.server.host = Some("news.example.com".into());
        file.posting.groups = Some(vec!["alt.test".into()]);
        let mut config = Config::resolve(
            file,
            Overrides {
                article_size: Some(64 * 1024),
                obfuscate: Some(ObfuscateMode::None),
                par2: Some(10),
                ..Default::default()
            },
        )
        .unwrap();
        config.dry_run = true;
        config.check = false;
        config
    }

    fn test_upload_params() -> Arc<UploadParams> {
        Arc::new(UploadParams {
            config: Arc::new(test_config()),
            archive_password_raw: None,
            nzb_default: None,
            json_mode: true,
            out: None,
            write_history: false,
            renderer_opts: pesto::progress::RendererOptions::default(),
            ext_filter: Vec::new(),
            cleanup_mode: CleanupMode::Leave,
        })
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn reports_generation_and_volume_upload_progress() {
        let dir = tempfile::tempdir().unwrap();
        let episode_a = dir.path().join("S01E01.bin");
        let episode_b = dir.path().join("S01E02.bin");
        std::fs::write(&episode_a, vec![0x11; 1024 * 1024]).unwrap();
        std::fs::write(&episode_b, vec![0x22; 1024 * 1024]).unwrap();

        let params = test_upload_params();
        let cancel = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        let segments = post_season_par2_volumes_with_progress(
            &[episode_a, episode_b],
            "Season01",
            &params,
            &cancel,
            tx,
        )
        .await
        .unwrap();
        assert!(
            !segments.is_empty(),
            "dry-run volume upload should produce segments"
        );

        let mut events = Vec::new();
        while let Ok(event) = rx.try_recv() {
            events.push(event);
        }
        assert!(events.iter().any(|event| matches!(
            event,
            pesto::progress::ProgressEvent::Par2EncodeStarted { .. }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            pesto::progress::ProgressEvent::Par2PassStarted { .. }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            pesto::progress::ProgressEvent::Par2InputProgress { .. }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            pesto::progress::ProgressEvent::Par2ComputeStarted { .. }
        )));
        assert!(events
            .iter()
            .any(|event| matches!(event, pesto::progress::ProgressEvent::Par2SliceWritten)));
        assert!(
            events.iter().any(|event| matches!(
                event,
                pesto::progress::ProgressEvent::Started {
                    mode: pesto::progress::RunMode::DryRun,
                    ..
                }
            )),
            "the generated volume upload must start the normal rich progress path"
        );
        assert!(events
            .iter()
            .any(|event| matches!(event, pesto::progress::ProgressEvent::SegmentDone { .. })));
        assert!(events
            .iter()
            .any(|event| matches!(event, pesto::progress::ProgressEvent::Finished)));
    }
}
