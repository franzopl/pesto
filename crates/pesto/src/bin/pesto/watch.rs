//! Watch-directory stability, retry and upload orchestration.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;

use super::batch::{derive_season_nzb_path, release_label, run_batch, top_level_entries};
use super::cleanup::apply_watch_cleanup;
use super::run_single_upload;
use super::upload::UploadParams;

/// How many consecutive failed attempts before giving up on an entry.
const WATCH_MAX_RETRIES: u32 = 3;

/// Recursively sum the byte size of a path (file or directory).
fn entry_size(path: &Path) -> u64 {
    if let Ok(md) = std::fs::metadata(path) {
        if md.is_file() {
            return md.len();
        }
    }
    let Ok(rd) = std::fs::read_dir(path) else {
        return 0;
    };
    rd.filter_map(|e| e.ok())
        .map(|e| entry_size(&e.path()))
        .sum()
}

/// `--each`/`--season` options that also apply to directories detected by
/// `--watch` (see `run_watch`).
pub(super) struct WatchBatchOpts {
    pub(super) each: bool,
    pub(super) season: bool,
    pub(super) explicit_out: Option<PathBuf>,
}

/// Run `--watch DIR`: poll for new entries and post each one automatically.
///
/// New entries are held in a "pending" state until their total byte size is
/// stable across two consecutive polls (settle check), preventing premature
/// uploads of directories that are still being populated.  Failed uploads are
/// retried up to `WATCH_MAX_RETRIES` times before being abandoned.
///
/// Exits cleanly on SIGTERM or Ctrl-C after finishing any in-progress upload.
pub(super) async fn run_watch(
    params: Arc<UploadParams>,
    watch_dir: &Path,
    watch_done: Option<&Path>,
    poll_interval: u64,
    jobs: usize,
    batch_opts: WatchBatchOpts,
    cancel: Arc<AtomicBool>,
) -> Result<bool> {
    let WatchBatchOpts {
        each,
        season,
        explicit_out,
    } = batch_opts;
    use tokio::sync::mpsc;

    eprintln!(
        "watching {} (poll every {}s)",
        watch_dir.display(),
        poll_interval
    );

    // `done`: entries that have been successfully uploaded (or permanently failed).
    let mut done: HashSet<PathBuf> = HashSet::new();
    // Pre-populate done with whatever is already present so we don't re-post on startup.
    if let Ok(existing) = top_level_entries(watch_dir, &params.ext_filter) {
        for e in existing {
            done.insert(e);
        }
    }

    // `pending`: entries seen but not yet stable.  Value is the size snapshot
    // from the previous poll; once two consecutive polls agree the entry is
    // dispatched for upload.
    let mut pending: HashMap<PathBuf, u64> = HashMap::new();

    // `retry_counts`: number of failed attempts per entry.
    let mut retry_counts: HashMap<PathBuf, u32> = HashMap::new();

    let effective_jobs = if jobs == 0 {
        parmesan::performance_core_count()
    } else {
        jobs
    };
    let semaphore = Arc::new(tokio::sync::Semaphore::new(effective_jobs));

    // Channel for completed tasks to report back (path, success, cancelled).
    let (result_tx, mut result_rx) = mpsc::unbounded_channel::<(PathBuf, bool, bool)>();

    let mut any_cancelled = false;

    loop {
        // Check for shutdown between polls.
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_secs(poll_interval)) => {}
            _ = async {
                while !cancel.load(Ordering::Relaxed) {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
            } => {
                eprintln!("\nshutdown requested — finishing in-progress uploads");
                break;
            }
        }

        // Drain completed-task notifications before scanning for new entries.
        while let Ok((entry, success, task_cancelled)) = result_rx.try_recv() {
            if task_cancelled {
                any_cancelled = true;
                eprintln!("watch: upload of `{}` was cancelled", entry.display());
            } else if success {
                done.insert(entry);
            } else {
                let attempts = retry_counts.entry(entry.clone()).or_insert(0);
                *attempts += 1;
                if *attempts >= WATCH_MAX_RETRIES {
                    eprintln!(
                        "watch: giving up on `{}` after {WATCH_MAX_RETRIES} failed attempts",
                        entry.display()
                    );
                    done.insert(entry);
                } else {
                    eprintln!(
                        "watch: will retry `{}` (attempt {}/{})",
                        entry.display(),
                        attempts,
                        WATCH_MAX_RETRIES
                    );
                    // Remove from pending so it goes through the settle check again.
                    pending.remove(&entry);
                }
            }
        }

        let entries = match top_level_entries(watch_dir, &params.ext_filter) {
            Ok(e) => e,
            Err(e) => {
                eprintln!("watch: error reading {}: {e}", watch_dir.display());
                continue;
            }
        };

        for entry in entries {
            if done.contains(&entry) {
                continue;
            }

            let current_size = entry_size(&entry);

            match pending.get(&entry).copied() {
                None => {
                    // First time we see this entry — record its size and wait.
                    pending.insert(entry.clone(), current_size);
                    let label = entry
                        .file_name()
                        .map(|n| n.to_string_lossy().into_owned())
                        .unwrap_or_else(|| "entry".to_string());
                    eprintln!("watch: detected `{label}` — waiting for it to stabilise");
                }
                Some(prev_size) if prev_size != current_size => {
                    // Still changing — update snapshot and keep waiting.
                    pending.insert(entry.clone(), current_size);
                }
                Some(_) => {
                    // Size unchanged since last poll: entry is stable, dispatch it.
                    pending.remove(&entry);
                    // Acquire the permit before spawning so uploads start in the
                    // sorted order returned by top_level_entries().
                    let permit = Arc::clone(&semaphore)
                        .acquire_owned()
                        .await
                        .expect("semaphore closed");
                    // Mark as done immediately so a second poll won't re-queue it
                    // while the upload task holds the semaphore permit.
                    done.insert(entry.clone());

                    let params = Arc::clone(&params);
                    let watch_done = watch_done.map(PathBuf::from);
                    let tx = result_tx.clone();
                    let label = release_label(&entry);
                    let task_cancel = cancel.clone();
                    let explicit_out = explicit_out.clone();

                    tokio::spawn(async move {
                        let _permit = permit;
                        if !params.json_mode {
                            println!("\n── watch: {} ──", label);
                        }
                        // Directories are posted as one combined NZB by default. With
                        // --each/--season, split per top-level entry instead, reusing
                        // the same batch machinery --each/--season use outside --watch.
                        // run_single_upload (the `else` branch below) already
                        // applies `params.cleanup_mode` to `entry` itself once
                        // its upload succeeds — see its own `should_cleanup`
                        // block. run_batch does not. Only the run_batch path
                        // needs the outer cleanup below, or a --cleanup/
                        // --cleanup-to entry gets deleted/moved twice: once
                        // here succeeding silently, once more failing with
                        // "No such file or directory" (issue #92).
                        let used_run_batch = (each || season) && entry.is_dir();
                        let (success, task_cancelled) = if used_run_batch {
                            let season_nzb = season.then(|| {
                                derive_season_nzb_path(
                                    explicit_out.as_deref(),
                                    &entry,
                                    params.config.nzb_dir.as_deref(),
                                )
                            });
                            match run_batch(
                                Arc::clone(&params),
                                std::slice::from_ref(&entry),
                                jobs,
                                season_nzb,
                                task_cancel.clone(),
                            )
                            .await
                            {
                                Ok((_segments, any_cancelled, any_failures)) => {
                                    (!any_cancelled && !any_failures, any_cancelled)
                                }
                                Err(e) => {
                                    eprintln!(
                                        "watch: upload failed for `{}`: {e:#}",
                                        entry.display()
                                    );
                                    (false, false)
                                }
                            }
                        } else {
                            match run_single_upload(
                                &params,
                                std::slice::from_ref(&entry),
                                &label,
                                Some(&task_cancel),
                                None,
                                false,
                                None,
                            )
                            .await
                            {
                                Ok(result) if result.cancelled => (false, true),
                                Ok(_) => (true, false),
                                Err(e) => {
                                    eprintln!(
                                        "watch: upload failed for `{}`: {e:#}",
                                        entry.display()
                                    );
                                    (false, false)
                                }
                            }
                        };
                        if success {
                            if let Err(e) = apply_watch_cleanup(
                                &params.cleanup_mode,
                                &entry,
                                watch_done.as_deref(),
                                used_run_batch,
                            ) {
                                eprintln!("watch: {e:#}");
                            }
                        }
                        // Report outcome; if the channel is closed we're shutting down.
                        let _ = tx.send((entry, success, task_cancelled));
                    });
                }
            }
        }
    }

    // Wait for all in-progress uploads (drain the semaphore).
    let effective_jobs = if jobs == 0 {
        parmesan::performance_core_count()
    } else {
        jobs
    };
    let _ = semaphore.acquire_many(effective_jobs as u32).await;
    eprintln!("watch: all uploads finished, exiting");
    Ok(any_cancelled)
}
