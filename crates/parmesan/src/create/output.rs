use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};

use super::model::{OutputExistsFailure, OutputPolicy};
use crate::layout;

static NEXT_STAGING_DIRECTORY: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
struct StagedOutput {
    final_path: PathBuf,
    staged_path: PathBuf,
    volume: Option<layout::VolumeChunk>,
}

#[derive(Debug)]
pub(super) struct StagedOutputs {
    directory: PathBuf,
    index: Option<StagedOutput>,
    volumes: Vec<StagedOutput>,
}

impl StagedOutputs {
    pub(super) fn prepare(
        output_dir: &Path,
        base_name: &str,
        recovery_count: usize,
        write_index: bool,
        policy: OutputPolicy,
    ) -> Result<Self> {
        let mut final_paths = Vec::new();
        let index_path = write_index.then(|| output_dir.join(layout::index_name(base_name)));
        if let Some(path) = &index_path {
            final_paths.push(path.clone());
        }
        let volumes = layout::plan_volumes(recovery_count as u32);
        final_paths.extend(
            volumes
                .iter()
                .map(|volume| output_dir.join(layout::volume_name(base_name, *volume))),
        );

        if policy == OutputPolicy::FailIfExists {
            if let Some(path) = final_paths.iter().find(|path| path.exists()) {
                return Err(anyhow::Error::new(OutputExistsFailure {
                    path: path.clone(),
                }));
            }
        }

        let directory = create_staging_directory(output_dir)?;
        let index = index_path.map(|final_path| StagedOutput {
            staged_path: directory.join(
                final_path
                    .file_name()
                    .expect("generated index name has a filename"),
            ),
            final_path,
            volume: None,
        });
        let volumes = volumes
            .into_iter()
            .map(|volume| {
                let final_path = output_dir.join(layout::volume_name(base_name, volume));
                StagedOutput {
                    staged_path: directory.join(
                        final_path
                            .file_name()
                            .expect("generated volume name has a filename"),
                    ),
                    final_path,
                    volume: Some(volume),
                }
            })
            .collect();

        Ok(Self {
            directory,
            index,
            volumes,
        })
    }

    pub(super) fn index_path(&self) -> Option<PathBuf> {
        self.index.as_ref().map(|output| output.final_path.clone())
    }

    pub(super) fn volume_paths(&self) -> Vec<PathBuf> {
        self.volumes
            .iter()
            .map(|output| output.final_path.clone())
            .collect()
    }

    pub(super) fn staged_index_path(&self) -> Option<&Path> {
        self.index
            .as_ref()
            .map(|output| output.staged_path.as_path())
    }

    pub(super) fn staged_volume_path(&self, volume: layout::VolumeChunk) -> &Path {
        self.volumes
            .iter()
            .find(|output| output.volume == Some(volume))
            .map(|output| output.staged_path.as_path())
            .expect("a staged path exists for every planned recovery volume")
    }

    pub(super) fn commit(self) -> Result<()> {
        let mut outputs: Vec<&StagedOutput> = self.volumes.iter().collect();
        if let Some(index) = &self.index {
            outputs.push(index);
        }
        for output in outputs {
            std::fs::rename(&output.staged_path, &output.final_path).with_context(|| {
                format!(
                    "publishing staged output `{}` as `{}`",
                    output.staged_path.display(),
                    output.final_path.display()
                )
            })?;
        }
        std::fs::remove_dir(&self.directory).with_context(|| {
            format!("removing staging directory `{}`", self.directory.display())
        })?;
        Ok(())
    }
}

impl Drop for StagedOutputs {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.directory);
    }
}

fn create_staging_directory(output_dir: &Path) -> Result<PathBuf> {
    const MAX_ATTEMPTS: usize = 1024;
    for _ in 0..MAX_ATTEMPTS {
        let sequence = NEXT_STAGING_DIRECTORY.fetch_add(1, Ordering::Relaxed);
        let path = output_dir.join(format!(
            ".parmesan-create-{}-{sequence}",
            std::process::id()
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => return Ok(path),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("creating staging directory `{}`", path.display()))
            }
        }
    }
    anyhow::bail!("could not allocate a unique PAR2 staging directory")
}
