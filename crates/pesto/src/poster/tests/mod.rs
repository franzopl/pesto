use std::sync::{Arc, Mutex};

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

fn minimal_shared(article_size: usize) -> Arc<Shared> {
    let mut config = dry_run_config();
    config.article_size = article_size;
    let post_group = pick_post_group(&config.groups);
    Arc::new(Shared {
        config,
        servers: Arc::new(vec![]),
        results: Arc::new(Mutex::new(Vec::new())),
        failures: Mutex::new(Vec::new()),
        failed_tasks: Mutex::new(Vec::new()),
        events: None,
        cancelled: Arc::new(AtomicBool::new(false)),
        paused: Arc::new(AtomicBool::new(false)),
        resume: None,
        resume_path: None,
        spool_dir: None,
        pool: Arc::new(Mutex::new(Vec::new())),
        encode_pool: Arc::new(Mutex::new(Vec::new())),
        total_retries: std::sync::atomic::AtomicUsize::new(0),
        post_group,
        release_prefix: None,
        release_from: None,
        run_id: 0,
        total_files: 0,
        release_layout: Arc::new(prepare::ReleaseLayout::from_parts(1, &[(1, 1)]).unwrap()),
        encryption_adapter: None,
        check_tx: Mutex::new(None),
    })
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
        mtime: None,
        release_ordinal: 1,
        file_index: 0,
    }
}

mod dry_run;
mod internals;
mod par2;
mod paths;
mod policy;
mod segment_index;
