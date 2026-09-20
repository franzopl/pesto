use super::*;
use crate::config::{Config, FileConfig, Overrides};
use crate::walk::InputFile;
use tempfile::TempDir;

fn dry_run_config() -> Config {
    let mut file = FileConfig::default();
    file.posting.groups = Some(vec!["alt.test".into()]);
    Config::resolve(
        file,
        Overrides {
            dry_run: Some(true),
            par2: Some(0),
            ..Default::default()
        },
    )
    .unwrap()
}

fn meta_with_name(path: &std::path::Path, name: &str) -> FileMeta {
    FileMeta {
        path: path.to_path_buf(),
        real_name: name.into(),
        client_path: name.into(),
        subject_name: name.into(),
        yenc_name: name.into(),
        from: String::new(),
        date: (None, None),
        size: 0,
        file_index: 0,
    }
}

mod dry_run;
mod internals;
mod par2;
mod paths;
mod policy;
