use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use walkdir::WalkDir;

use super::model::{CreateRequest, InvalidRequestFailure, Recovery, SliceStrategy};
use crate::ops::InputFile;

pub(super) fn validate_request(request: &CreateRequest) -> Result<()> {
    if request.input_paths().is_empty() {
        return Err(anyhow::Error::new(InvalidRequestFailure(
            "at least one input path is required",
        )));
    }
    if request.requested_memory_limit() == 0 {
        return Err(anyhow::Error::new(InvalidRequestFailure(
            "memory limit must be greater than zero",
        )));
    }
    match request.recovery_strategy() {
        Recovery::Percentage(0) => {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "recovery percentage must be greater than zero",
            )))
        }
        Recovery::Blocks(0) => {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "recovery block count must be greater than zero",
            )))
        }
        Recovery::Percentage(_) | Recovery::Blocks(_) => {}
    }
    match request.requested_slice_strategy() {
        SliceStrategy::Size(0) => {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "slice size must be greater than zero",
            )))
        }
        SliceStrategy::Count(0) => {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "slice count must be greater than zero",
            )))
        }
        SliceStrategy::Automatic | SliceStrategy::Size(_) | SliceStrategy::Count(_) => {}
    }
    if let Some(base_name) = request.output_base_name() {
        let mut components = Path::new(base_name).components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(anyhow::Error::new(InvalidRequestFailure(
                "output base name must be one normal path component",
            )));
        }
    }
    Ok(())
}

pub(super) fn collect_files(paths: &[PathBuf], recurse: bool) -> Result<Vec<InputFile>> {
    let mut input_files = Vec::new();
    for path in paths {
        let metadata =
            std::fs::metadata(path).with_context(|| format!("stat `{}`", path.display()))?;
        if metadata.is_dir() {
            if !recurse {
                anyhow::bail!(
                    "`{}` is a directory; enable recursive expansion to protect it",
                    path.display()
                );
            }
            for entry in WalkDir::new(path)
                .follow_links(false)
                .sort_by_file_name()
                .into_iter()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_type().is_file())
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
                size: metadata.len(),
            });
        }
    }
    Ok(input_files)
}
