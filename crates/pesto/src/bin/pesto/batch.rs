//! Pure input and naming policy for batch and season uploads.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::{Context, Result};
use pesto::config::ObfuscateMode;
use pesto::nntp::pool::ConnectionBroker;
use pesto::nzb::NzbMeta;
use pesto::poster::PostedSegment;
use tracing::info;

use super::output::expand_tilde;
use super::season::post_season_par2_volumes;
use super::upload::{resolve_entry_password, UploadParams};
use super::{add_obfuscation_tag, nfo_metadata_header, run_all_hooks, run_single_upload, HookEnv};

fn is_artifact_entry(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|ext| ext == "nfo" || ext == "nzb")
}

/// Whether `path`'s extension is one of `ext_filter` (case-insensitive). An
/// empty `ext_filter` matches everything (the `--ext` default: no filtering).
fn matches_ext_filter(path: &Path, ext_filter: &[String]) -> bool {
    if ext_filter.is_empty() {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| {
            ext_filter
                .iter()
                .any(|allowed| allowed.eq_ignore_ascii_case(ext))
        })
}

/// Apply `--ext` to an already-expanded input list, in place. A no-op when
/// `ext_filter` is empty. Errors out if the filter drops every input, so a
/// mistyped extension (or an entry that is 100% subtitles/extras) fails
/// loudly instead of silently posting nothing.
pub(super) fn apply_ext_filter(
    inputs: &mut Vec<pesto::walk::InputFile>,
    ext_filter: &[String],
    entry_label: &str,
) -> Result<()> {
    if ext_filter.is_empty() {
        return Ok(());
    }
    inputs.retain(|f| matches_ext_filter(&f.path, ext_filter));
    if inputs.is_empty() {
        anyhow::bail!(
            "no files matching --ext {} found in `{entry_label}`",
            ext_filter.join(",")
        );
    }
    Ok(())
}

/// Enumerate top-level entries of `dir` (files and subdirectories), sorted by
/// name using natural lexical ordering (so `E02` comes before `E10`).
///
/// `ext_filter` (from `--ext`) drops non-matching *files*; subdirectories are
/// always kept regardless of their name, since matching files may live inside
/// them.
pub(super) fn top_level_entries(dir: &Path, ext_filter: &[String]) -> Result<Vec<PathBuf>> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("reading directory `{}`", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| !is_artifact_entry(p))
        .filter(|p| p.is_dir() || matches_ext_filter(p, ext_filter))
        .collect();
    entries.sort_by(|a, b| pesto::walk::natural_cmp(&a.to_string_lossy(), &b.to_string_lossy()));
    Ok(entries)
}

/// Derive the destination of a consolidated `--season` NZB.
pub(super) fn derive_season_nzb_path(
    explicit_out: Option<&Path>,
    entry: &Path,
    nzb_dir: Option<&str>,
) -> PathBuf {
    if let Some(out) = explicit_out {
        return out.to_path_buf();
    }
    // `Path::file_name()` deliberately returns `None` for paths ending in
    // `.` or `..`. Resolve those paths first so `pesto . --season` names the
    // pack after its input directory.
    let name = entry
        .file_name()
        .map(|name| name.to_owned())
        .or_else(|| {
            entry
                .canonicalize()
                .ok()
                .and_then(|resolved| resolved.file_name().map(|name| name.to_owned()))
        })
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "season".to_string());
    let stem = format!("{name}.nzb");
    match nzb_dir {
        Some(dir) => expand_tilde(dir).join(&stem),
        None => PathBuf::from(&stem),
    }
}

/// Build the human-readable entry label used by hooks, banners and history.
pub(super) fn release_label(path: &Path) -> String {
    const STRIP_EXTS: &[&str] = &[
        "mkv", "mp4", "avi", "ts", "m2ts", "mov", "wmv", "flv", "webm", "mpg", "mpeg", "vob",
        "iso", "nzb", "zip", "rar", "7z", "tar", "gz", "bz2", "cbz", "cbr", "pdf", "epub",
    ];
    path.file_name()
        .map(|s| {
            let name = s.to_string_lossy();
            let p = Path::new(s);
            match p.extension().and_then(|e| e.to_str()) {
                Some(ext) if STRIP_EXTS.contains(&ext.to_ascii_lowercase().as_str()) => {
                    p.file_stem().unwrap_or(s).to_string_lossy().into_owned()
                }
                _ => name.into_owned(),
            }
        })
        .unwrap_or_else(|| "entry".to_string())
}

/// Force every episode in a season batch onto one pre-picked newsgroup target.
/// Plain `--each` runs keep their original group pool unchanged.
fn force_season_group(params: Arc<UploadParams>, is_season: bool) -> Arc<UploadParams> {
    if !is_season {
        return params;
    }
    let forced_target = pesto::poster::pick_post_group(&params.config.groups);
    if forced_target.is_empty() {
        return params;
    }
    let mut forced_config = (*params.config).clone();
    forced_config.groups = vec![forced_target.join("+")];
    let mut forced_params = (*params).clone();
    forced_params.config = Arc::new(forced_config);
    Arc::new(forced_params)
}

/// Removes every collected directory on drop.
///
/// A `--season` batch defers each episode's compress-temp cleanup (see
/// `run_single_upload`'s `keep_compress_temp`) so the archive bytes are
/// still on disk when the season's global PAR2 step reads them afterward.
/// Wrapping the collected dirs in this guard means they're still removed —
/// via `Drop` — even if `run_batch` returns early (e.g. the season NZB
/// write's `?`) before reaching the end of the season-merge block.
struct CompressTempCleanup(Vec<PathBuf>);

impl Drop for CompressTempCleanup {
    fn drop(&mut self) {
        for dir in &self.0 {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// Run `--each` / `--season` batch orchestration over the collected entries.
pub(super) async fn run_batch(
    params: Arc<UploadParams>,
    dirs: &[PathBuf],
    jobs: usize,
    season_nzb: Option<PathBuf>,
    cancel: Arc<AtomicBool>,
) -> Result<(Vec<PostedSegment>, bool, bool)> {
    // Collect all entries from every directory argument.
    let mut entries: Vec<PathBuf> = Vec::new();
    for dir in dirs {
        let md = std::fs::metadata(dir).with_context(|| format!("reading `{}`", dir.display()))?;
        if md.is_dir() {
            entries.extend(top_level_entries(dir, &params.ext_filter)?);
        } else {
            // A plain file is its own "entry".
            entries.push(dir.clone());
        }
    }

    if entries.is_empty() {
        anyhow::bail!("no entries found to post");
    }

    // A season batch merges every entry's NZB into one at the end, so they
    // all need the *same* archive password — resolved once, up front, and
    // handed to every entry below. A plain --each batch has no such merge,
    // so leaving this `None` lets each entry resolve (and randomise) its
    // own password independently inside `run_single_upload` (issue #67).
    let season_password: Option<String> = season_nzb
        .is_some()
        .then(|| {
            resolve_entry_password(
                None,
                params.config.compress_password.as_deref(),
                params.archive_password_raw.as_deref(),
            )
        })
        .flatten();

    let params = force_season_group(params, season_nzb.is_some());

    // Whether each episode should keep its compressed archive on disk
    // (instead of deleting it right after posting) so the season's global
    // PAR2 step below can compute recovery data over the *actual posted
    // bytes* rather than the original, never-compressed episode file — see
    // `run_single_upload`'s `keep_compress_temp` doc comment. Mirrors the
    // exact gate `post_season_par2_volumes` is called under further down, so
    // nothing is retained when there's no season PAR2 step to read it.
    let keep_compress_temp = season_nzb.is_some() && params.config.par2 > 0 && entries.len() > 1;

    let effective_jobs = if jobs == 0 {
        parmesan::performance_core_count()
    } else {
        jobs
    };

    let semaphore = Arc::new(tokio::sync::Semaphore::new(effective_jobs));

    // One connection broker for the whole batch: every episode below checks
    // out already-authenticated connections instead of paying a fresh
    // TLS+AUTH handshake per episode (see ROADMAP.new.md Phase 2). Sized to
    // the configured total connection budget and shared (via the broker's
    // internal semaphore) across concurrently running episodes under
    // `--jobs N`, so real concurrent sockets never exceed that budget.
    let (broker, broker_keepalive) = ConnectionBroker::new(
        Arc::new(params.config.all_servers().collect()),
        params.config.total_connections(),
        params.config.keepalive_interval,
    );

    let mut all_segments: Vec<PostedSegment> = Vec::new();
    let mut all_groups: Vec<String> = Vec::new();
    let mut any_cancelled = false;
    let mut any_failures = false;
    let mut posted_episode_paths: Vec<PathBuf> = Vec::new();
    let mut compress_temp_cleanup = CompressTempCleanup(Vec::new());

    let total_entries = entries.len();
    let mut handles = Vec::new();
    for (entry_idx, entry) in entries.iter().enumerate() {
        // Acquire the permit before spawning so uploads start in the sorted
        // order. With the permit inside the task, the scheduler decided which
        // upload ran first, making --each non-deterministic.
        let permit = Arc::clone(&semaphore)
            .acquire_owned()
            .await
            .expect("semaphore closed");
        let entry = entry.clone();
        let params = Arc::clone(&params);
        let task_cancel = cancel.clone();
        let task_password = season_password.clone();
        let task_broker = broker.clone();
        let label = release_label(&entry);

        info!(
            entry = entry_idx + 1,
            total = total_entries,
            name = %label,
            "--each entry"
        );

        let handle = tokio::spawn(async move {
            let _permit = permit;
            if !params.json_mode {
                println!("\n── {} ──", label);
            }
            run_single_upload(
                &params,
                &[entry],
                &label,
                Some(&task_cancel),
                task_password.as_deref(),
                keep_compress_temp,
                Some(task_broker),
            )
            .await
        });
        handles.push(handle);
    }

    for handle in handles {
        match handle.await {
            Ok(Ok(result)) => {
                all_segments.extend(result.segments);
                for g in result.groups {
                    if !all_groups.contains(&g) {
                        all_groups.push(g);
                    }
                }
                if result.cancelled {
                    any_cancelled = true;
                }
                // Pack completeness is independent of --allow-incomplete-nzb
                // (that flag is per-episode). had_failures already covers
                // MissingConfirmed; inconclusive is OR'd so Inconclusive
                // still blocks if the two ever diverge.
                if result.had_failures || !result.inconclusive.is_empty() {
                    any_failures = true;
                }
                posted_episode_paths.extend(result.posted_paths);
                if let Some(dir) = result.compress_temp_dir {
                    compress_temp_cleanup.0.push(dir);
                }
            }
            Ok(Err(e)) => {
                eprintln!("upload error: {e:#}");
                any_failures = true;
            }
            Err(e) => {
                eprintln!("upload task panicked: {e}");
                any_failures = true;
            }
        }
    }

    // Every episode has checked its connections back in by now — close them
    // for real and stop the keepalive task.
    broker.shutdown().await;
    broker_keepalive.abort();

    info!(entries = total_entries, "--each complete");

    // Write consolidated season NZB (and matching .nfo + hooks) when requested.
    // The pack is a distinct artefact: --allow-incomplete-nzb never unlocks it.
    if let Some(season_path) = season_nzb {
        if pesto::poster::should_write_season_nzb(
            any_cancelled,
            any_failures,
            all_segments.is_empty(),
        ) {
            info!(entries = total_entries, path = %season_path.display(), "season merge starting");
            let config = &params.config;
            let season_name = config.nzb_title.clone().or_else(|| {
                season_path
                    .file_stem()
                    .map(|s| s.to_string_lossy().into_owned())
            });

            // Generate and post global PAR2 for the entire season (Phase 47b).
            // This produces a single, coherent recovery set covering all episodes
            // instead of multiple independent rsids for each episode.
            //
            // Uses `posted_episode_paths` — the files each episode actually put
            // on the wire (an archive, under `--compress`/`--password`) — not
            // the original `entries`. The two can differ in content, name, and
            // even count (one archive can split into several `--compress-
            // volume-size` volumes); computing recovery data against the
            // original, never-posted file would produce a PAR2 set that
            // doesn't describe anything actually on Usenet (`keep_compress_temp`
            // above is what keeps these archives alive long enough to read here).
            let mut season_par2_segments = Vec::new();
            if config.par2 > 0 && posted_episode_paths.len() > 1 {
                match post_season_par2_volumes(
                    &posted_episode_paths,
                    &season_name.clone().unwrap_or_else(|| "season".to_string()),
                    &params,
                    &cancel,
                )
                .await
                {
                    Ok(par2_segments) => {
                        if !par2_segments.is_empty() {
                            info!(
                                par2_segments = par2_segments.len(),
                                "season PAR2 volumes posted successfully"
                            );
                            season_par2_segments = par2_segments;
                        }
                    }
                    Err(e) => {
                        eprintln!("✗ season PAR2 posting failed: {e:#}");
                        eprintln!("  (continuing with per-episode PAR2 sets)");
                        // Non-fatal; continue with season consolidation without global PAR2.
                    }
                }
            }

            // Filter segments for the season NZB:
            // - Keep: episode data files (no .par2 in name)
            // - Remove: per-episode PAR2 sets (have .par2 in name)
            // - Add: global season PAR2 (replaces individual sets with single coherent rsid)
            let season_segments: Vec<PostedSegment> = if !season_par2_segments.is_empty() {
                let data_segments: Vec<_> = all_segments
                    .iter()
                    .filter(|s| !s.file_name.ends_with(".par2"))
                    .cloned()
                    .collect();
                info!(
                    total_segments = all_segments.len(),
                    data_segments_count = data_segments.len(),
                    par2_segments_count = season_par2_segments.len(),
                    "season NZB consolidation: filtering segments"
                );
                if data_segments.is_empty() {
                    eprintln!("⚠ WARNING: No episode data segments found! Season NZB will contain only PAR2.");
                }
                let mut combined = data_segments;
                combined.extend(season_par2_segments);
                combined
            } else {
                // If season PAR2 generation failed, use all segments (with per-episode PAR2 sets)
                info!(
                    total_segments = all_segments.len(),
                    "season NZB consolidation: using all segments (no global PAR2)"
                );
                all_segments.clone()
            };

            let mut nzb_tags = config.nzb_tags.clone();
            add_obfuscation_tag(&mut nzb_tags, &config.obfuscate);
            let nzb_meta = NzbMeta {
                name: season_name,
                password: config
                    .nzb_password
                    .clone()
                    .or_else(|| season_password.clone()),
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
            let xml =
                pesto::nzb::generate(&all_groups, &season_segments, &nzb_meta, config.obfuscate);
            tokio::fs::write(&season_path, &xml)
                .await
                .with_context(|| format!("writing season nzb `{}`", season_path.display()))?;
            if !params.json_mode {
                println!("\nwrote season nzb: {}", season_path.display());
            } else {
                let path_esc = season_path
                    .display()
                    .to_string()
                    .replace('\\', "\\\\")
                    .replace('"', "\\\"");
                println!(r#"{{"type":"nzb_written","path":"{path_esc}","season":true}}"#);
            }

            // Generate season .nfo (mediainfo of first episode) next to the NZB.
            let nfo_path: Option<PathBuf> = if config.nfo {
                let nfo_out = season_path.with_extension("nfo");
                match pesto::nfo::generate_season(dirs) {
                    Some(content) => match pesto::nfo::write(
                        &nfo_out,
                        &format!("{}{content}", nfo_metadata_header(config)),
                    ) {
                        Ok(()) => {
                            println!("wrote nfo:  {}", nfo_out.display());
                            Some(nfo_out)
                        }
                        Err(e) => {
                            eprintln!("season nfo write failed: {e}");
                            None
                        }
                    },
                    None => None,
                }
            } else {
                None
            };

            // Run post-upload hooks — same as a regular upload.
            let season_label = season_path
                .file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| "season".to_string());
            let total_bytes: u64 = all_segments.iter().map(|s| s.bytes).sum();
            let effective_password = config
                .nzb_password
                .clone()
                .or_else(|| season_password.clone());
            let season_obfuscate = match config.obfuscate {
                ObfuscateMode::None => "none",
                ObfuscateMode::Full => "full",
                ObfuscateMode::Light => "light",
                ObfuscateMode::FullShared => "full-shared",
                ObfuscateMode::Article => "article",
            };
            // The group(s) actually used across every episode in the season
            // — every episode is now forced onto the same pre-picked target
            // (see the `pick_post_group` override above `run_batch`'s entry
            // loop), so `all_groups` is just that one shared target rather
            // than a union of independently-random picks.
            let season_groups_str = all_groups.join(":");
            // Same reasoning for the server(s): the union of servers that
            // actually accepted an article across every episode, derived
            // from each segment's `server_idx`, not the static config.
            let season_server_list: Vec<_> = config.all_servers().collect();
            let mut season_server_idxs: Vec<usize> =
                all_segments.iter().map(|s| s.server_idx).collect();
            season_server_idxs.sort_unstable();
            season_server_idxs.dedup();
            let season_servers_str = season_server_idxs
                .into_iter()
                .filter_map(|idx| season_server_list.get(idx))
                .map(|s| s.host.as_str())
                .collect::<Vec<_>>()
                .join(":");
            let season_tags_str = config.nzb_tags.join(" ");
            let hook_env = HookEnv {
                nzb_path: Some(&season_path),
                nfo_path: nfo_path.as_deref(),
                name: &season_label,
                total_bytes,
                input_paths: "",
                group: all_groups.first().map(String::as_str),
                groups: &season_groups_str,
                password: effective_password.as_deref(),
                server: season_servers_str.split(':').next().unwrap_or(&config.host),
                servers: &season_servers_str,
                category: config.nzb_category.as_deref(),
                nzb_title: config.nzb_title.as_deref(),
                obfuscate: season_obfuscate,
                par2: config.par2,
                tags: &season_tags_str,
                tmdb_id: config.tmdb_id.as_deref(),
                imdb_id: config.imdb_id.as_deref(),
                tvdb_id: config.tvdb_id.as_deref(),
                mal_id: config.mal_id.as_deref(),
                incomplete: false,
            };
            // Skip hooks for --dry-run / --par2-only: no real upload happened.
            if !config.dry_run && !config.par2_only {
                run_all_hooks(config, &hook_env);
            }
        } else if any_cancelled {
            eprintln!("interrupted — skipping season nzb output");
        } else if any_failures {
            eprintln!("error: season pack was not created due to earlier upload failures");
        } else {
            eprintln!("error: season pack was not created (no valid segments were uploaded)");
        }
    }

    Ok((all_segments, any_cancelled, any_failures))
}

#[cfg(test)]
mod release_label_tests {
    use super::release_label;
    use std::path::Path;

    #[test]
    fn strips_mkv_from_season_episode_file() {
        assert_eq!(
            release_label(Path::new("/tv/Show.S01E01.1080p.mkv")),
            "Show.S01E01.1080p"
        );
    }

    #[test]
    fn keeps_scene_name_without_media_extension() {
        assert_eq!(
            release_label(Path::new("/tv/Show.S01E01.720p.BluRay-Group")),
            "Show.S01E01.720p.BluRay-Group"
        );
    }

    #[test]
    fn strips_extension_case_insensitively() {
        assert_eq!(release_label(Path::new("Movie.MKV")), "Movie");
    }

    #[test]
    fn keeps_directory_like_names() {
        assert_eq!(release_label(Path::new("/season/Episode01")), "Episode01");
    }
}

#[cfg(test)]
mod tests;
