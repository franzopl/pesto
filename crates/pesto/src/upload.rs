//! Full upload pipeline: compress → post → NZB → history → notifications →
//! hooks.
//!
//! [`run_upload`] is the single entry point for embedding callers (upapasta).
//! The `pesto` CLI has its own equivalent in `bin/pesto.rs`; the two will
//! converge over time.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;

use crate::compress::ArchiveFormat;
use crate::config::{Config, ObfuscateMode};
use crate::poster::PostedSegment;
use crate::progress::ProgressSender;

/// The result of a completed upload pipeline.
pub struct UploadOutcome {
    pub segments: Vec<PostedSegment>,
    pub groups: Vec<String>,
    pub cancelled: bool,
    pub had_failures: bool,
    pub nzb_path: Option<PathBuf>,
    pub total_bytes: u64,
}

/// Run the complete upload pipeline.
///
/// `entry_paths` are the original user-specified paths; they are used to derive
/// the NZB output stem and are passed to the hooks as-is.
/// `entry_label` is the display name written to history and passed to hooks.
/// `write_history` controls whether a record is appended to the shared
/// pesto history file after a successful upload.
pub async fn run_upload(
    config: &Config,
    entry_paths: &[PathBuf],
    entry_label: &str,
    progress_tx: Option<ProgressSender>,
    cancel: Option<Arc<AtomicBool>>,
    nzb_out_override: Option<PathBuf>,
    write_history: bool,
) -> anyhow::Result<UploadOutcome> {
    let upload_start = std::time::Instant::now();
    let mut inputs = crate::walk::expand_inputs(entry_paths)?;
    let total_bytes: u64 = inputs
        .iter()
        .filter_map(|f| std::fs::metadata(&f.path).ok())
        .map(|m| m.len())
        .sum();

    // ── Compression ──────────────────────────────────────────────────────────
    let compress_format_str: Option<String> = config.compress_format.clone().or_else(|| {
        if config.compress_password.is_some() {
            Some("7z".to_string())
        } else {
            None
        }
    });
    let effective_password: Option<String> = config.compress_password.clone();
    let compress_temp_dir: Option<PathBuf>;

    if let Some(fmt_str) = &compress_format_str {
        let format = ArchiveFormat::parse(fmt_str).ok_or_else(|| {
            anyhow::anyhow!("unknown compression format `{fmt_str}`; supported: 7z, zip, rar")
        })?;

        let archive_stem = upload_root(&inputs)
            .or_else(|| {
                inputs.first().map(|f| {
                    PathBuf::from(&f.name)
                        .file_stem()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned()
                })
            })
            .unwrap_or_else(|| "archive".to_string());

        let archive_stem = if config.obfuscate != ObfuscateMode::None {
            crate::article::obfuscated_name()
        } else {
            archive_stem
        };

        let tmp_dir = std::env::temp_dir().join(format!(
            "pesto_compress_{}_{}",
            std::process::id(),
            entry_label
        ));
        compress_temp_dir = Some(tmp_dir.clone());

        let fs_paths: Vec<PathBuf> = collect_compress_roots(&inputs);
        let compress_input_bytes: u64 = fs_paths.iter().map(|p| dir_or_file_size(p)).sum();

        emit(
            &progress_tx,
            crate::progress::ProgressEvent::CompressStarted {
                total_bytes: compress_input_bytes,
            },
        );

        // Poll archive size every 200 ms for a live progress bar.
        let poll_tx = progress_tx.clone();
        let poll_path = tmp_dir.join(format!("{}.{}", archive_stem, format.extension()));
        let poll_handle = tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_millis(200));
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                if let Ok(meta) = tokio::fs::metadata(&poll_path).await {
                    if let Some(ref tx) = poll_tx {
                        let _ = tx.send(crate::progress::ProgressEvent::CompressProgress {
                            bytes_written: meta.len(),
                        });
                    }
                }
            }
        });

        let compress_inputs = fs_paths;
        let compress_stem = archive_stem;
        let compress_dest = tmp_dir;
        let compress_pass = effective_password.clone();
        let result = tokio::task::spawn_blocking(move || {
            crate::compress::compress(
                &compress_inputs,
                &compress_stem,
                &compress_dest,
                format,
                compress_pass.as_deref(),
            )
        })
        .await
        .context("compressor task panicked")??;

        poll_handle.abort();
        emit(&progress_tx, crate::progress::ProgressEvent::CompressDone);

        let archive_name = result
            .path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        inputs = vec![crate::walk::InputFile {
            path: result.path,
            name: archive_name,
        }];
    } else {
        compress_temp_dir = None;
    }
    // ─────────────────────────────────────────────────────────────────────────

    // Derive NZB output path (override > nzb_dir/stem.nzb > ./stem.nzb).
    // Always derive the stem from the original entry_paths so compression or
    // obfuscation does not leak randomised archive names into the filename.
    let nzb_base: Option<PathBuf> = nzb_out_override.or_else(|| {
        let stem = entry_paths
            .first()
            .and_then(|p| {
                p.file_name().map(|s| {
                    // Release directories use the full folder name as the NZB
                    // stem — calling file_stem() would strip codec tags like
                    // "264" from "H.264" or "0" from "AAC2.0".
                    if p.is_dir() {
                        s.to_string_lossy().into_owned()
                    } else {
                        Path::new(s)
                            .file_stem()
                            .unwrap_or(s)
                            .to_string_lossy()
                            .into_owned()
                    }
                })
            })
            .or_else(|| upload_root(&inputs))
            .or_else(|| {
                inputs.first().map(|f| {
                    let top = f.name.split('/').next().unwrap_or(&f.name);
                    // When the name has a slash, top is a directory component —
                    // use it as-is to avoid stripping codec tags.
                    if f.name.contains('/') {
                        top.to_owned()
                    } else {
                        PathBuf::from(top)
                            .file_stem()
                            .unwrap_or_default()
                            .to_string_lossy()
                            .into_owned()
                    }
                })
            })?;
        let base = if let Some(dir) = &config.nzb_dir {
            expand_tilde(dir).join(&stem)
        } else {
            PathBuf::from(&stem)
        };
        let mut s = base.into_os_string();
        s.push(".nzb");
        Some(PathBuf::from(s))
    });

    let resume_path = nzb_base.as_ref().map(|p| p.with_extension("pesto-state"));

    // ── Post ─────────────────────────────────────────────────────────────────
    let post_tx = progress_tx.clone();
    let outcome = if let Some(ref cancel_flag) = cancel {
        crate::poster::post_files_with_progress_and_cancel(
            config,
            &inputs,
            post_tx,
            resume_path.as_deref(),
            Some(cancel_flag.clone()),
            Some(entry_label),
        )
        .await?
    } else {
        crate::poster::post_files_with_progress(
            config,
            &inputs,
            post_tx,
            resume_path.as_deref(),
            Some(entry_label),
        )
        .await?
    };
    // ─────────────────────────────────────────────────────────────────────────

    let has_post_failures = !outcome.failures.is_empty();
    // Set when the streaming check still can't confirm some articles after
    // every repost attempt. Kept separate from `has_post_failures` because
    // `--allow-incomplete-nzb` only opts back into publishing past *this*
    // kind of gap, not a genuine POST failure.
    let has_confirmed_missing = !outcome.still_missing.is_empty();
    let cancelled = outcome.cancelled || cancel.as_ref().is_some_and(|f| f.load(Ordering::Relaxed));

    if has_confirmed_missing {
        for id in &outcome.still_missing {
            emit_status(&progress_tx, format!("  missing: {id}"));
        }
        tracing::error!(
            count = outcome.still_missing.len(),
            ids = ?outcome.still_missing,
            "check: articles still missing after every repost attempt"
        );
    }

    // If some articles are still confirmed missing after every repost round,
    // refuse to write the NZB (and skip NFO/hooks below) unless the caller
    // explicitly opted into publishing anyway via `allow_incomplete_nzb`
    // (e.g. relying on PAR2 recovery). A genuine POST failure always blocks,
    // regardless of the flag.
    let write_blocked =
        has_post_failures || (has_confirmed_missing && !config.allow_incomplete_nzb);

    // ── Write NZB ────────────────────────────────────────────────────────────
    let nzb_path: Option<PathBuf> = if (cancelled && !config.resume)
        || outcome.segments.is_empty()
        || config.dry_run
        || config.par2_only
        || write_blocked
    {
        None
    } else if let Some(base) = nzb_base {
        let out = versioned_nzb_path(&base).await;
        let nzb_meta = crate::nzb::NzbMeta {
            name: config.nzb_name.clone().or_else(|| {
                entry_paths
                    .first()
                    .and_then(|p| p.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
            }),
            password: config
                .nzb_password
                .clone()
                .or_else(|| effective_password.clone()),
            category: config.nzb_category.clone(),
            tags: config.nzb_tags.clone(),
        };
        let xml = crate::nzb::generate(&outcome.groups, &outcome.segments, &nzb_meta);
        match tokio::fs::write(&out, &xml).await {
            Ok(()) => {
                emit_status(&progress_tx, format!("wrote nzb: {}", out.display()));

                if write_history && !config.dry_run {
                    let par2_str;
                    let par2_pct = if config.par2 > 0 {
                        par2_str = format!("{}%", config.par2);
                        Some(par2_str.as_str())
                    } else {
                        None
                    };
                    crate::history::record_upload(
                        &crate::history::UploadRecord {
                            name: entry_label,
                            obfuscated_name: if config.obfuscate != ObfuscateMode::None {
                                Some(entry_label)
                            } else {
                                None
                            },
                            password: effective_password.as_deref(),
                            total_bytes,
                            group: config.groups.first().map(String::as_str),
                            server: Some(config.host.as_str()),
                            par2_redundancy: par2_pct,
                            duration_secs: upload_start.elapsed().as_secs_f64(),
                            nzb_path: Some(&out.display().to_string()),
                            subject: config.nzb_name.as_deref().or(Some(entry_label)),
                        },
                        config.history_dir.as_deref(),
                    );
                }

                Some(out)
            }
            Err(e) => {
                emit_status(&progress_tx, format!("failed to write nzb: {e}"));
                None
            }
        }
    } else {
        None
    };
    // ─────────────────────────────────────────────────────────────────────────

    // ── Notifications ────────────────────────────────────────────────────────
    let notify_enabled = config.notify.unwrap_or(true)
        && (config.notify_webhook.is_some() || config.notify_ntfy.is_some());
    if notify_enabled && !config.par2_only && !config.dry_run && !cancelled {
        crate::notify::send_all(&crate::notify::NotifyConfig {
            webhook_url: config.notify_webhook.as_deref(),
            ntfy_topic: config.notify_ntfy.as_deref(),
            name: entry_label,
            total_bytes,
            group: config.groups.first().map(String::as_str),
            category: config.nzb_category.as_deref(),
            // Reflects true completeness, independent of `allow_incomplete_nzb`
            // — the notification should say "not fully ok" even when the
            // caller chose to publish anyway.
            ok: !(has_post_failures || has_confirmed_missing),
        })
        .await;
    }
    // ─────────────────────────────────────────────────────────────────────────

    // ── NFO + post-upload hooks ──────────────────────────────────────────────
    if !cancelled && !write_blocked && !config.par2_only && !config.dry_run {
        // Generate .nfo next to the .nzb (or next to the source files).
        let nfo_path: Option<PathBuf> = if config.nfo {
            let base = nzb_path
                .as_ref()
                .map(|p| p.with_extension("nfo"))
                .or_else(|| {
                    entry_paths
                        .first()
                        .and_then(|p| p.parent())
                        .map(|d| d.join(format!("{entry_label}.nfo")))
                });
            if let Some(ref nfo_out) = base {
                match crate::nfo::generate(entry_paths) {
                    Some(content) => match crate::nfo::write(nfo_out, &content) {
                        Ok(()) => {
                            emit_status(&progress_tx, format!("wrote nfo:  {}", nfo_out.display()));
                            Some(nfo_out.clone())
                        }
                        Err(e) => {
                            emit_status(&progress_tx, format!("nfo write failed: {e}"));
                            None
                        }
                    },
                    None => None,
                }
            } else {
                None
            }
        } else {
            None
        };

        let hook_ctx = crate::hooks::HookContext {
            name: entry_label.to_string(),
            total_bytes,
            input_paths: entry_paths
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join(":"),
            server: config.host.clone(),
            group: config.groups.first().cloned().unwrap_or_default(),
            groups: config.groups.join(":"),
            password: config
                .nzb_password
                .as_deref()
                .or(config.compress_password.as_deref())
                .unwrap_or("")
                .to_string(),
            category: config.nzb_category.clone().unwrap_or_default(),
            nzb_name: config.nzb_name.clone().unwrap_or_default(),
            obfuscate: match config.obfuscate {
                ObfuscateMode::None => "none",
                ObfuscateMode::Full => "full",
                ObfuscateMode::Paranoid => "paranoid",
            }
            .to_string(),
            par2: config.par2,
            tags: config.nzb_tags.join(" "),
            nzb_path: nzb_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            nfo_path: nfo_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
        };
        let hook_cfg = config.clone();
        let log_lines =
            tokio::task::spawn_blocking(move || crate::hooks::run_hooks(&hook_cfg, &hook_ctx))
                .await
                .unwrap_or_else(|e| vec![format!("hook task panicked: {e}")]);
        for line in log_lines {
            emit_status(&progress_tx, format!("[hook] {}", line));
        }
    }
    // ─────────────────────────────────────────────────────────────────────────

    if let Some(dir) = compress_temp_dir {
        let _ = tokio::fs::remove_dir_all(dir).await;
    }
    // Only now — after `post_files_with_progress_and_cancel` has fully
    // drained its internal streaming check/repost queue, which may need to
    // re-read a PAR2 file's bytes — is it safe to remove the PAR2 temp dir.
    // See `poster::par2_temp_dir`'s doc comment for why this used to happen
    // too early.
    if !config.par2_only {
        let _ = tokio::fs::remove_dir_all(crate::poster::par2_temp_dir()).await;
    }

    Ok(UploadOutcome {
        segments: outcome.segments,
        groups: outcome.groups,
        cancelled,
        // True completeness, independent of `allow_incomplete_nzb` — the
        // caller (e.g. upapasta's catalog) should still be able to tell an
        // upload with confirmed-missing articles apart from a clean one.
        had_failures: has_post_failures || has_confirmed_missing,
        nzb_path,
        total_bytes,
    })
}

fn emit(tx: &Option<ProgressSender>, event: crate::progress::ProgressEvent) {
    if let Some(ref tx) = tx {
        let _ = tx.send(event);
    }
}

fn emit_status(tx: &Option<ProgressSender>, text: impl Into<String>) {
    emit(
        tx,
        crate::progress::ProgressEvent::Status { text: text.into() },
    );
}

fn collect_compress_roots(inputs: &[crate::walk::InputFile]) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    for input in inputs {
        let depth = input.name.split('/').count();
        let root = if depth <= 1 {
            input.path.clone()
        } else {
            input
                .path
                .ancestors()
                .nth(depth)
                .filter(|p| !p.as_os_str().is_empty())
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| input.path.clone())
        };
        if !roots.contains(&root) {
            roots.push(root);
        }
    }
    if roots.is_empty() {
        inputs.iter().map(|f| f.path.clone()).collect()
    } else {
        roots
    }
}

fn upload_root(inputs: &[crate::walk::InputFile]) -> Option<String> {
    let mut root: Option<&str> = None;
    for input in inputs {
        let (candidate, _) = input.name.split_once('/')?;
        match root {
            Some(existing) if existing != candidate => return None,
            _ => root = Some(candidate),
        }
    }
    root.map(str::to_string)
}

fn dir_or_file_size(path: &Path) -> u64 {
    match std::fs::metadata(path) {
        Err(_) => 0,
        Ok(m) if m.is_file() => m.len(),
        Ok(_) => {
            let mut total = 0u64;
            if let Ok(rd) = std::fs::read_dir(path) {
                for entry in rd.flatten() {
                    total += dir_or_file_size(&entry.path());
                }
            }
            total
        }
    }
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(rest);
        }
    } else if path == "~" {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home);
        }
    }
    PathBuf::from(path)
}

/// Return a unique path for the NZB using `O_CREAT|O_EXCL` (atomic create).
///
/// Tries `base.nzb`, then `base.v2.nzb`, `base.v3.nzb`, … until it can
/// exclusively create the file. No stat/exists calls.
async fn versioned_nzb_path(base: &Path) -> PathBuf {
    let bare = base.with_extension("");
    let dir = bare.parent().unwrap_or(Path::new("."));
    let stem = bare.file_name().unwrap_or_default().to_string_lossy();

    let try_create = |path: PathBuf| async move {
        tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
            .map(|_| path)
    };

    if let Ok(p) = try_create(dir.join(format!("{stem}.nzb"))).await {
        return p;
    }
    let mut n = 2u32;
    loop {
        let candidate = dir.join(format!("{stem}.v{n}.nzb"));
        if let Ok(p) = try_create(candidate).await {
            return p;
        }
        n += 1;
        if n > 999 {
            return dir.join(format!("{stem}.nzb"));
        }
    }
}
