//! Upload confirmation panel, live upload progress and catalog recording.

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use pesto::config::Config as PestoConfig;
use tokio_util::sync::CancellationToken;

use crate::catalog::NewUpload;
use crate::events::ProgressUpdate;

use super::{
    collect_nzb_release_keys, compress_label, expand_tilde, obf_label, on_off, App, FileProgress,
    FileStatus, UploadProgress, UploadSettingsSummary, UNSET,
};

impl App {
    pub fn trigger_upload(&mut self) {
        if self.upload_in_progress {
            self.status_bar.set("Upload already running");
            return;
        }
        if self.upload_queue.items.is_empty() {
            self.status_bar
                .set("Queue empty — add files in Browser tab (Enter)");
            return;
        }

        self.upload_in_progress = true;
        self.upload_queue.active = self.upload_queue.items.len();
        self.upload_started_at = Some(Instant::now());

        // Reset per-item state to Pending. The live [▶] badge and Active state
        // are then driven one item at a time by ItemUploadStarted, because
        // uploads run sequentially (one NZB at a time).
        self.queue_status = self
            .upload_queue
            .items
            .iter()
            .map(|p| (p.clone(), FileStatus::Pending))
            .collect();
        self.file_tree
            .set_uploading(std::collections::HashSet::new());

        let token = CancellationToken::new();
        self.current_cancel_token = Some(token.clone());
        self.current_pause_flag = Some(Arc::new(AtomicBool::new(false)));

        // Initialize per-file tracking
        let file_progress: Vec<FileProgress> = self
            .upload_queue
            .items
            .iter()
            .map(|name| FileProgress {
                name: name.clone(),
                total_segments: 0,
                done_segments: 0,
                total_bytes: 0,
                done_bytes: 0,
                status: FileStatus::Pending,
            })
            .collect();

        self.progress = UploadProgress {
            start_time: Some(Instant::now()),
            speed_history: vec![0.0; 5],
            files: file_progress,
            ..Default::default()
        };

        self.status_bar.set(format!(
            "🚀 Upload started ({} files) — streaming real pesto progress (x to cancel)",
            self.upload_queue.items.len()
        ));
        let mode = if self.pesto_config.is_some() {
            "REAL"
        } else {
            "dry-run"
        };
        self.log_panel
            .push(format!("=== Starting pesto::post() [{}] ===", mode));

        // Show effective settings to the user (very important for transparency)
        let settings = self.effective_upload_settings();
        self.log_panel
            .push("--- Effective settings for this upload ---".to_string());
        self.log_panel
            .push(format!("  Obfuscation : {}", settings.obfuscate));
        self.log_panel
            .push(format!("  Compression : {}", settings.compression));
        self.log_panel
            .push(format!("  PAR2        : {}", settings.par2));
        self.log_panel
            .push(format!("  Groups      : {}", settings.groups));
        self.log_panel
            .push("------------------------------------------".to_string());
    }

    /// Live upload state for a queued path (Pending when unknown).
    pub fn item_status(&self, path: &str) -> FileStatus {
        self.queue_status
            .get(path)
            .copied()
            .unwrap_or(FileStatus::Pending)
    }

    /// A single queue item began uploading. Mark it Active and light up its
    /// live [▶] badge in the Browser (only this item, since uploads are
    /// sequential).
    pub fn item_upload_started(&mut self, path: &str) {
        // Each item posts its own NZB and restarts its progress from zero, so
        // clear the previous item's gauges; otherwise the bar stays pinned at
        // the last item's 100% (apply() only grows the counters).
        self.progress.reset_for_item();
        self.queue_status
            .insert(path.to_string(), FileStatus::Active);
        if let Some(fp) = self.progress.files.iter_mut().find(|f| f.name == path) {
            fp.status = FileStatus::Active;
        }
        let basename = std::path::Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|s| s.to_string());
        let mut set = std::collections::HashSet::new();
        if let Some(b) = basename {
            set.insert(b);
        }
        self.file_tree.set_uploading(set);

        // Drop the previous item's rows. Folder modes (PerFile/Season) post each
        // inner file under its own `real_name`, so the per-file panel is seeded
        // from this item's pesto `Started` event (see `register_upload_files`),
        // not from the folder-keyed queue entry.
        self.progress.files.clear();
    }

    /// Seed the per-file rows from a pesto run's work plan. Each tuple is
    /// `(real_name, total_segments, total_bytes)` taken from the `Started`
    /// event; the same `real_name` keys the later `SegmentDone` updates, so the
    /// per-file gauges advance instead of sitting at "waiting…". Within one
    /// queue entry several runs can register files (Season posts each episode as
    /// its own run), so rows accumulate across runs and are matched by name.
    pub fn register_upload_files(&mut self, files: Vec<(String, u64, u64)>) {
        for (name, total_segments, total_bytes) in files {
            if let Some(fp) = self.progress.files.iter_mut().find(|f| f.name == name) {
                fp.total_segments = total_segments.max(fp.total_segments);
                fp.total_bytes = total_bytes.max(fp.total_bytes);
                if fp.status == FileStatus::Pending {
                    fp.status = FileStatus::Active;
                }
            } else {
                self.progress.files.push(FileProgress {
                    name,
                    total_segments,
                    done_segments: 0,
                    total_bytes,
                    done_bytes: 0,
                    status: FileStatus::Active,
                });
            }
        }
    }

    /// A single queue item finished. Record it in the catalog immediately with
    /// the real size and the real NZB path pesto wrote, so a later failure in
    /// the same batch can never erase this success. Failed items are kept in
    /// the queue (marked ✗) for retry.
    pub fn item_upload_done(
        &mut self,
        path: &str,
        success: bool,
        size_bytes: u64,
        nzb_path: Option<PathBuf>,
        duration_s: f64,
        record_catalog: bool,
    ) {
        let status = if success {
            FileStatus::Done
        } else {
            FileStatus::Failed
        };
        self.queue_status.insert(path.to_string(), status);
        if let Some(fp) = self.progress.files.iter_mut().find(|f| f.name == path) {
            fp.status = status;
            if success && fp.total_segments == 0 {
                // No per-file segment stream matched; show a full gauge anyway.
                fp.total_segments = 1;
                fp.done_segments = 1;
            }
        }

        // Per-file / season folder modes record each produced NZB separately via
        // `CatalogRecord`, so the item-level event must not double-record.
        // Failed uploads are also recorded so they appear in History with a ✗ indicator.
        if record_catalog {
            let original_name = std::path::Path::new(path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(path)
                .to_string();
            self.record_catalog_entry(original_name, size_bytes, nzb_path, duration_s, !success);
        }
    }

    /// Record one produced NZB in the catalog and refresh the Browser status.
    pub fn record_catalog_entry(
        &mut self,
        original_name: String,
        size_bytes: u64,
        nzb_path: Option<PathBuf>,
        duration_s: f64,
        had_failures: bool,
    ) {
        if let Some(ref cat) = self.catalog {
            let group = self
                .pesto_config
                .as_ref()
                .and_then(|c| c.groups.first().cloned());
            let server = self.pesto_config.as_ref().map(|c| c.host.clone());

            let mut rec = NewUpload::from_name(original_name);
            rec.size_bytes = (size_bytes > 0).then_some(size_bytes as i64);
            rec.upload_duration_s = Some(duration_s);
            rec.usenet_group = group;
            rec.nntp_server = server;
            rec.nzb_path = nzb_path.map(|p| p.to_string_lossy().into_owned());
            rec.had_failures = had_failures;
            if let Err(e) = cat.record(&rec) {
                self.log_panel.push(format!("catalog record error: {}", e));
            }
        }
        // Reflect the new catalog entry in the Browser's NZB status column.
        self.refresh_nzb_status();
    }

    /// Refresh only the per-file NZB status map used by the Browser (cheaper
    /// than a full history refresh; safe to call after each item).
    pub fn refresh_nzb_status(&mut self) {
        if let Some(ref cat) = self.catalog {
            if let Ok(map) = cat.status_map() {
                self.file_tree.set_nzb_status(map);
            }
        }
    }

    /// Build the on-disk NZB index by scanning the configured `nzb_dir`
    /// recursively for `.nzb` files and keying them by release name. This is
    /// what lets the Browser flag a file as already-uploaded when a matching
    /// NZB exists on disk but is not in the catalog (e.g. uploaded by another
    /// tool, or before this catalog existed). No-op when `nzb_dir` is unset.
    pub fn refresh_nzb_disk_index(&mut self) {
        let Some(dir) = self
            .pesto_config
            .as_ref()
            .and_then(|c| c.nzb_dir.as_deref())
            .map(expand_tilde)
        else {
            return;
        };
        let mut index = std::collections::HashMap::new();
        collect_nzb_release_keys(&dir, &mut index);
        self.file_tree.set_nzb_disk_index(index);
    }

    /// Reload the set of release keys that already went through a hook (e.g. an
    /// indexer upload) from the catalog into the Browser, so those releases get
    /// the "sent" marker.
    pub fn refresh_hooked_index(&mut self) {
        let keys = self
            .catalog
            .as_ref()
            .and_then(|c| c.hooked_release_keys().ok())
            .unwrap_or_default();
        self.file_tree.set_hooked_index(keys);
    }

    /// Record a successful hook run for a release and refresh the Browser marker.
    /// No-op when the release key is empty (early failures) or no catalog exists.
    pub fn record_hook_run(&mut self, release_key: &str, release_name: &str, hook_name: &str) {
        if release_key.is_empty() {
            return;
        }
        if let Some(cat) = self.catalog.as_ref() {
            if let Err(e) = cat.record_hook_run(release_key, release_name, hook_name) {
                self.log_panel
                    .push_error(format!("could not record hook run: {e}"));
            }
        }
        self.refresh_hooked_index();
    }

    pub fn upload_finished(&mut self, success: bool, cancelled: bool) {
        // Catalog records are written per-item in `item_upload_done`, with the
        // real size and NZB path; nothing is recorded here. This finalizes the
        // batch UI and prunes the queue.
        self.upload_started_at.take();

        self.upload_in_progress = false;
        self.upload_queue.active = 0;
        self.progress.is_cancelled = cancelled;
        self.progress.is_paused = false;
        self.current_pause_flag = None;

        // On a non-cancelled batch, successfully uploaded items leave the queue
        // (they now live in History with a ✓ badge); failed items stay queued
        // so the user can fix the problem and press `u` to retry just them.
        let mut failed = 0usize;
        if !cancelled {
            let done: Vec<String> = self
                .upload_queue
                .items
                .iter()
                .filter(|p| self.item_status(p) == FileStatus::Done)
                .cloned()
                .collect();
            failed = self
                .upload_queue
                .items
                .iter()
                .filter(|p| self.item_status(p) == FileStatus::Failed)
                .count();
            for p in &done {
                if let Some(pos) = self.upload_queue.items.iter().position(|q| q == p) {
                    self.upload_queue.items.remove(pos);
                }
                self.queue_meta.remove(p);
                self.queue_status.remove(p);
            }
            let len = self.upload_queue.items.len();
            if self.upload_queue.selected >= len {
                self.upload_queue.selected = len.saturating_sub(1);
            }
            self.sync_queue_badges();
            self.save_queue();
        }

        self.progress.files.clear();
        self.file_tree
            .set_uploading(std::collections::HashSet::new());

        if cancelled {
            self.status_bar.set("Upload cancelled by user");
            self.log_panel.push("=== Upload cancelled ===".to_string());
        } else if success {
            self.status_bar.set("Upload finished successfully");
            self.log_panel.push("=== Upload finished ===".to_string());
        } else {
            self.log_panel
                .push("=== Upload finished with failures — check logs above ===".to_string());
            self.status_bar.set(format!(
                "{} item(s) failed — still queued, press u to retry",
                failed
            ));
        }

        // Refresh the history list if it's currently visible
        self.refresh_history();
    }

    /// Called from the event loop when we receive a human log line
    pub fn handle_progress(&mut self, msg: String) {
        self.log_panel.push(msg);
    }

    /// Called when we receive a structured progress update (preferred path)
    pub fn handle_progress_update(&mut self, update: ProgressUpdate) {
        // Always log a short version
        if let Some(m) = &update.message {
            self.log_panel.push(m.clone());
        }

        self.progress.apply(&update);

        // Apply per-file update if present. The row is normally seeded from the
        // run's `Started` event (register_upload_files); fall back to creating it
        // here so a SegmentDone is never silently dropped.
        if let Some(fu) = &update.file_update {
            let fp = match self.progress.files.iter_mut().find(|f| f.name == fu.name) {
                Some(fp) => fp,
                None => {
                    self.progress.files.push(FileProgress {
                        name: fu.name.clone(),
                        total_segments: 0,
                        done_segments: 0,
                        total_bytes: 0,
                        done_bytes: 0,
                        status: FileStatus::Active,
                    });
                    self.progress.files.last_mut().unwrap()
                }
            };
            fp.done_segments += fu.done_segments;
            fp.done_bytes += fu.done_bytes;
            fp.total_segments = fu.total_segments.max(fp.total_segments);
            fp.total_bytes = fu.total_bytes.max(fp.total_bytes);

            if fu.ok {
                if fp.done_segments >= fp.total_segments && fp.total_segments > 0 {
                    fp.status = FileStatus::Done;
                } else {
                    fp.status = FileStatus::Active;
                }
            } else {
                fp.status = FileStatus::Failed;
            }
        }

        // Update speed using real elapsed time + bytes
        if let Some(start) = self.progress.start_time {
            let elapsed = start.elapsed().as_secs_f64();
            if elapsed > 0.3 && self.progress.done_bytes > 0 {
                let mb = self.progress.done_bytes as f64 / (1024.0 * 1024.0);
                self.progress.last_speed = mb / elapsed;
            }
        }
    }

    pub fn cancel_upload(&mut self) {
        if !self.upload_in_progress {
            return;
        }
        self.progress.is_cancelled = true;
        if let Some(token) = self.current_cancel_token.take() {
            token.cancel();
        }
        self.status_bar.set("Cancelling upload...");
        self.log_panel
            .push("=== Upload cancellation requested ===".to_string());
    }

    /// Flip the shared pause flag for the in-flight upload. Connections stay
    /// open and kept alive while paused (see `post_files_inner`), so toggling
    /// back resumes immediately without a reconnect.
    pub fn toggle_pause_upload(&mut self) {
        if !self.upload_in_progress || self.progress.is_cancelled {
            return;
        }
        let Some(flag) = self.current_pause_flag.as_ref() else {
            return;
        };
        let now_paused = !self.progress.is_paused;
        flag.store(now_paused, std::sync::atomic::Ordering::Relaxed);
        self.progress.is_paused = now_paused;
        if now_paused {
            self.status_bar.set("Pausing... (p to resume)");
            self.log_panel.push("=== Pause requested ===".to_string());
        } else {
            self.status_bar.set("Resuming...");
            self.log_panel.push("=== Resume requested ===".to_string());
        }
    }

    /// Returns a user-friendly summary of the settings that will be used
    /// for the next upload (based on loaded config or dry-run defaults).
    pub fn effective_upload_settings(&self) -> UploadSettingsSummary {
        // Use effective config (with session overrides applied) when available.
        let owned;
        let cfg_ref: Option<&PestoConfig> = if self.pesto_config.is_some() {
            owned = self.effective_config_with_overrides();
            owned.as_ref()
        } else {
            None
        };
        if let Some(cfg) = cfg_ref {
            let obfuscate = obf_label(cfg.obfuscate).to_string();

            let compression = match &cfg.compress_format {
                Some(fmt) if cfg.compress_password.is_some() => {
                    format!("{} + password", compress_label(fmt))
                }
                Some(fmt) => compress_label(fmt),
                None => "Off".to_string(),
            };

            let par2 = format!("{}%", cfg.par2);

            let groups = if cfg.groups.is_empty() {
                UNSET.to_string()
            } else {
                cfg.groups.join(", ")
            };

            let from = if cfg.from.contains('@') {
                cfg.from.clone()
            } else {
                "Random identity".to_string()
            };

            let article = format!("{} KB / {} chars", cfg.article_size / 1024, cfg.line_length);

            let check = on_off(cfg.check).to_string();

            UploadSettingsSummary {
                obfuscate,
                compression,
                par2,
                groups,
                from,
                article_size: article,
                check,
            }
        } else {
            // Dry-run defaults (what we currently use in build_dry_run_config)
            UploadSettingsSummary {
                obfuscate: "None (dry-run)".to_string(),
                compression: "Off (dry-run)".to_string(),
                par2: "5% (dry-run)".to_string(),
                groups: "alt.binaries.test (dry-run)".to_string(),
                from: "upapasta@local (dry-run)".to_string(),
                article_size: "750 KB / 128 chars (dry-run)".to_string(),
                check: "Off (dry-run)".to_string(),
            }
        }
    }
}
