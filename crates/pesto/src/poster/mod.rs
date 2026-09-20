//! Parallel posting: the orchestration that ties together file reading, yEnc
//! encoding, article assembly and the NNTP client.
//!
//! Files are read sequentially by a producer. yEnc runs on a small encode
//! pool (nyuu: one encoder filling a ready-article queue). NNTP workers only
//! POST. If PAR2 recovery exceeds a memory limit, the producer re-reads.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{bail, Context, Result};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tracing::{debug, error, info, warn};

use crate::article::{
    default_subject, generate_message_id, obfuscated_name, obfuscated_name_with_prefix,
    random_from, Article,
};
use crate::config::{types::MAX_AUTO_PIPELINE_DEPTH, Config, ObfuscateMode};
use crate::nntp::pool::{ConnectionBroker, ConnectionSlot};
use crate::progress::{FileEntry, ProgressEvent, ProgressSender, RunMode};
use crate::resume::{resume_action, ResumeAction, ResumeState, SegmentRecord};
use crate::walk::{natural_cmp, InputFile};
use crate::yenc;
use parmesan::encoder::FileHasher;
use parmesan::layout;
use parmesan::packet;

mod check;
use check::spawn_check_coordinator;
mod connections;
use connections::{release_slots, split_connections, take_slots};
mod par2;
use par2::par2_geometry;
pub use par2::{generate_and_write_season_par2, generate_and_write_season_par2_with_progress};
mod identity;
pub use identity::pick_post_group;
use identity::{
    normalize_client_path, obfuscated_yenc_name, par2_release_base, persisted_identity,
    resolve_date,
};
mod outcome;
pub use outcome::{
    nzb_write_decision, should_write_season_nzb, FailedTask, NzbWriteDecision, PostOutcome,
    PostedSegment,
};
mod producer;
use producer::producer;
mod shared;
use shared::Shared;
mod task;
use task::{PostTask, ReadyArticle, TaskDispatcher};

#[derive(Debug, Clone)]
struct FileMeta {
    path: PathBuf,
    real_name: String,
    client_path: String,
    subject_name: String,
    yenc_name: String,
    /// Poster identity for this file. In obfuscate mode a fresh random
    /// identity is generated per file so segments cannot be correlated
    /// across files by the From header.
    from: String,
    /// Date header resolved once per file: `(rfc_string, unix_timestamp)`.
    /// Fixed dates have `Some` RFC but `None` timestamp.
    date: (Option<String>, Option<u64>),
    size: u64,
    /// This file's 1-based position among every file in the release (data
    /// files, then the PAR2 index, then the PAR2 volumes) — used for the
    /// `--file-counter` `[filenum/total]` subject prefix. Meaningless
    /// (left as `0`) when `Shared::total_files` is `0`, i.e. the flag is off.
    file_index: u32,
}

/// How many dedicated yEnc workers fill the ready-article queue.
///
/// Encode is off the POST path, so this is not one-SIMD-per-connection.
/// Cap at performance cores. A single encoder on c7i (4c) left post-only
/// movie at 0.85× nyuu (`20260820T091733Z`); `min(cores, conns)` is the
/// fill rate the queue needs at 0 ms mock.
fn encode_concurrency(perf_cores: usize, connections: usize) -> usize {
    perf_cores.min(connections.max(1)).max(1)
}

/// Nyuu `articleQueueBuffer`: `min(round(conns*0.5)+2, 25)`.
fn ready_queue_depth(connections: usize) -> usize {
    let n = connections.max(1);
    let half = n / 2 + n % 2; // round(n*0.5) for integers
    (half + 2).clamp(4, 25)
}

/// Post every file in `files` to the groups configured in `config`.
///
/// This is the silent entry point; use [`post_files_with_progress`] to observe
/// the run through a [`ProgressEvent`] channel. Build the [`InputFile`] list
/// with [`crate::walk::expand_inputs`], which also expands directories.
pub async fn post_files(config: &Config, files: &[InputFile]) -> Result<PostOutcome> {
    post_files_with_progress(config, files, None, None, None).await
}

/// Post every file in `files`, emitting [`ProgressEvent`]s on `events`.
///
/// `resume_state_path` is the path of the `.pesto-state` sidecar file.
/// Progress is tracked in memory whenever this path is given, regardless of
/// `config.resume` — that flag only controls whether a *prior* run's
/// on-disk state at this path is loaded and used to skip already-posted
/// segments. At the end of the run, the state is written to disk once if
/// the run ended incomplete (so a later `--resume` has something to load),
/// or deleted if it ended complete (nothing left to resume).
///
/// Passing `None` for `events` keeps the poster silent (library default).
pub async fn post_files_with_progress(
    config: &Config,
    files: &[InputFile],
    events: Option<ProgressSender>,
    resume_state_path: Option<&Path>,
    entry_label: Option<&str>,
) -> Result<PostOutcome> {
    post_files_with_progress_and_cancel(config, files, events, resume_state_path, None, entry_label)
        .await
}

/// Like [`post_files_with_progress`] but accepts an external cancel flag.
///
/// Setting `external_cancel` to `true` causes the run to stop at the next
/// segment boundary, exactly as if the user had pressed Ctrl-C. This is the
/// integration point for embedding applications such as `upapasta`.
pub async fn post_files_with_progress_and_cancel(
    config: &Config,
    files: &[InputFile],
    events: Option<ProgressSender>,
    resume_state_path: Option<&Path>,
    external_cancel: Option<Arc<AtomicBool>>,
    entry_label: Option<&str>,
) -> Result<PostOutcome> {
    post_files_inner(
        config,
        files,
        events,
        resume_state_path,
        external_cancel,
        entry_label,
        None,
        None,
    )
    .await
}

/// Like [`post_files_with_progress_and_cancel`], but lets the caller supply a
/// [`ConnectionBroker`] whose already-authenticated connections are checked
/// out for this run and checked back in (instead of disconnected) when done,
/// so a later call sharing the same broker reuses them without paying a
/// fresh TLS+AUTH handshake, and/or an `external_pause` flag: setting it to
/// `true` suspends every posting worker at the next segment-batch boundary
/// (connections stay open and kept alive) and setting it back to `false`
/// resumes immediately, without paying a reconnect. Only the posting phase
/// is pausable — PAR2 generation, compression and the final check/repost
/// passes run to completion regardless, the same phase scoping `cancel`
/// already has.
///
/// This is CLI-internal plumbing for `--each`/`--season` batching (see
/// `run_batch` in `bin/pesto.rs`) — embedders should use
/// [`post_files_with_progress_and_cancel`], `post`, `post_cancelable` or
/// `post_pausable`, which always build and tear down their own pool per
/// call and remain unaffected by the `broker` parameter (`broker: None`).
#[allow(clippy::too_many_arguments)]
pub async fn post_files_inner(
    config: &Config,
    files: &[InputFile],
    events: Option<ProgressSender>,
    resume_state_path: Option<&Path>,
    external_cancel: Option<Arc<AtomicBool>>,
    entry_label: Option<&str>,
    broker: Option<Arc<ConnectionBroker>>,
    external_pause: Option<Arc<AtomicBool>>,
) -> Result<PostOutcome> {
    post_files_inner_with_release_prefix(
        config,
        files,
        events,
        resume_state_path,
        external_cancel,
        entry_label,
        broker,
        external_pause,
        None,
    )
    .await
}

/// Variant of [`post_files_inner`] used by the upload pipelines to supply an
/// opaque shared identity created before posting (currently a compressed
/// `light` archive stem). External callers should use [`post_files_inner`].
#[doc(hidden)]
#[allow(clippy::too_many_arguments)]
pub async fn post_files_inner_with_release_prefix(
    config: &Config,
    files: &[InputFile],
    events: Option<ProgressSender>,
    resume_state_path: Option<&Path>,
    external_cancel: Option<Arc<AtomicBool>>,
    entry_label: Option<&str>,
    broker: Option<Arc<ConnectionBroker>>,
    external_pause: Option<Arc<AtomicBool>>,
    release_prefix_override: Option<&str>,
) -> Result<PostOutcome> {
    configure_rayon(config.threads);
    if config.file_counter && !config.obfuscate.policy().allow_file_counter {
        bail!(
            "file_counter=true contradicts private obfuscation mode {:?}",
            config.obfuscate
        );
    }
    if let Some(domain) = &config.message_id_domain {
        if !crate::article::valid_message_id_domain(domain) {
            bail!("invalid message_id_domain `{domain}`");
        }
    }

    // Resume state is tracked in memory for *every* run that could plausibly
    // need it (not gated behind --resume), so a run that ends incomplete
    // always has something to persist for a later retry — without
    // `--resume`, deciding you need it only happens *after* a failure, which
    // is too late if nothing was ever recorded (see issue #18). Only
    // *loading* a prior run's on-disk state (to skip already-posted
    // segments) stays gated behind --resume: silently trusting whatever
    // `.pesto-state` file happens to already sit next to the target, without
    // being asked to, is exactly the "stale state reused blindly" hazard
    // issue #18 warns about.
    let (resume_arc, resume_path_owned) = if !config.dry_run && !config.par2_only {
        if let Some(rp) = resume_state_path {
            let state = if config.resume {
                ResumeState::load(rp)?
            } else {
                ResumeState::default()
            };
            (Some(Arc::new(Mutex::new(state))), Some(rp.to_path_buf()))
        } else {
            (None, None)
        }
    } else {
        (None, None)
    };

    // Type-1 spool: only when --resume was actually passed (see the field
    // doc on `Shared::spool_dir` for why this is a stricter condition than
    // `resume_arc` itself).
    let spool_dir_owned = if config.resume {
        resume_path_owned.as_deref().map(crate::spool::spool_dir)
    } else {
        None
    };

    // Posting parameters that change how the whole input is chunked or
    // named — compared against whatever fingerprint a loaded state was
    // recorded under. A mismatch (e.g. this run's --article-size differs
    // from the run that originally populated the state) means every
    // recorded Message-ID could reference the wrong byte range, so the
    // *entire* state is discarded rather than trusted partially — see
    // `resume::RunFingerprint` and GitHub issue #18.
    let run_fingerprint = crate::resume::RunFingerprint::from_config(config);
    if let Some(resume) = &resume_arc {
        let mut state = resume.lock().unwrap();
        let had_segments = !state.is_empty();
        if !state.validate_run(&run_fingerprint) {
            eprintln!(
                "resume: posting parameters changed since the saved state was recorded \
                 (--article-size/--obfuscate/--compress/--par2/--file-counter) — ignoring it \
                 and starting fresh"
            );
        } else if had_segments
            && config.obfuscate != ObfuscateMode::None
            && state.has_legacy_wire_identities()
        {
            bail!(
                "resume state predates persisted wire identities; this obfuscated upload cannot safely append or repost segments — finish it with the original Pesto version or start a new upload"
            );
        } else if had_segments {
            eprintln!(
                "resuming: {} segment(s) already posted, skipping",
                state.len()
            );
        }
    }

    // Generated once per run (not per file) so every file posted under
    // `FullShared`/`Light` — archive parts and PAR2 volumes alike — shares
    // the same wire name prefix and sender identity. See
    // `ObfuscateMode::FullShared` and `ObfuscateMode::Light`. Randomly
    // generated fresh by default, which would otherwise make a `--resume`
    // run's segments unmatchable against a prior run's (its wire identity,
    // though not the resume key itself, would differ) — a compatible prior
    // state (see `validate_run` above) reuses the same identity instead of
    // generating a new one; see issue #18's resume follow-up discussion.
    let (release_prefix, release_from) = if matches!(
        config.obfuscate,
        ObfuscateMode::FullShared | ObfuscateMode::Light
    ) {
        let reused = resume_arc.as_ref().and_then(|r| {
            r.lock()
                .unwrap()
                .release_identity()
                .map(|(p, f)| (p.to_string(), f.to_string()))
        });
        // A compressed `light` upload can supply its already-random archive
        // stem here. That makes the archive filename, the wire Subject/yEnc
        // name, the NZB subject and the PAR2 FileDesc one shareable identity
        // instead of creating a second random wire-only token. A persisted
        // identity always wins for an interrupted pre-change upload: changing
        // its wire identity would make its already-posted segments unusable.
        let (prefix, from) = reused.unwrap_or_else(|| {
            (
                release_prefix_override
                    .filter(|prefix| !prefix.is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(obfuscated_name),
                random_from(),
            )
        });
        if let Some(resume) = &resume_arc {
            resume
                .lock()
                .unwrap()
                .set_release_identity(prefix.clone(), from.clone());
        }
        (Some(prefix), Some(from))
    } else {
        (None, None)
    };

    let common_release_root = files
        .first()
        .and_then(|file| file.name.split_once('/').map(|(root, _)| root))
        .filter(|root| {
            files.iter().all(|file| {
                file.name
                    .split_once('/')
                    .is_some_and(|(candidate, _)| candidate == *root)
            })
        });
    let mut metas = Vec::with_capacity(files.len());
    let mut client_paths = std::collections::HashSet::with_capacity(files.len());
    for (idx, input) in files.iter().enumerate() {
        let path = &input.path;
        let md = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("reading metadata of `{}`", path.display()))?;
        if !md.is_file() {
            bail!("`{}` is not a regular file", path.display());
        }
        // `real_name` is the published name: a relative path like
        // `season01/ep01.mkv` for files found inside a directory argument.
        let real_name = input.name.clone();
        let client_path = normalize_client_path(&real_name, common_release_root)?.to_owned();
        if !client_paths.insert(client_path.clone()) {
            bail!("multiple inputs normalize to the same client path `{client_path}`");
        }
        let size = md.len();

        if let Some(resume) = &resume_arc {
            let mtime = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs());
            let file_fp = crate::resume::FileFingerprint { size, mtime };
            let mut state = resume.lock().unwrap();
            if !state.file_matches(&real_name, &file_fp) {
                if config.par2 > 0 {
                    // PAR2 recovery blocks are computed over the whole
                    // recovery set together, not per file — one file's
                    // content changing invalidates every volume's segments
                    // too, not just this file's own (see
                    // `forget_all_segments`'s doc comment). PAR2 volumes
                    // never go through this per-file check themselves
                    // (they're generated later, straight into the posting
                    // queue — see `push_par2_file`), so this is the only
                    // place that can catch it.
                    eprintln!(
                        "resume: `{real_name}` changed size or modification time since the \
                         saved state was recorded — ignoring all saved segments, including \
                         PAR2 volumes, since recovery data no longer matches this file"
                    );
                    state.forget_all_segments();
                } else {
                    eprintln!(
                        "resume: `{real_name}` changed size or modification time since the \
                         saved state was recorded — ignoring its saved segments and \
                         re-posting it"
                    );
                    state.forget_file(&real_name);
                }
            }
            state.record_file(&real_name, file_fp);
        }
        let (subject_name, yenc_name, from) = match config.obfuscate {
            ObfuscateMode::None => {
                let wn = client_path.clone();
                (wn.clone(), wn, config.from.clone())
            }
            ObfuscateMode::Full | ObfuscateMode::Article => (
                obfuscated_name(),
                obfuscated_yenc_name(&real_name),
                random_from(),
            ),
            ObfuscateMode::Light | ObfuscateMode::FullShared => {
                let from = release_from.clone().unwrap_or_default();
                let prefix = release_prefix.as_deref().unwrap_or_default();
                // A `--compress-volume-size` archive part carries a
                // volume suffix (`.partNN.rar`, `.7z.NNN`) that indexers
                // key their "same release" grouping off of — preserve it
                // verbatim instead of the generic numbered suffix below,
                // or the release fails to group under full-shared/light
                // obfuscation (issue #68).
                let name = if let Some(suffix) = crate::compress::volume_suffix(&real_name) {
                    format!("{prefix}{suffix}")
                } else {
                    let ext = Path::new(&real_name)
                        .extension()
                        .map(|e| format!(".{}", e.to_string_lossy()))
                        .unwrap_or_default();
                    // A single-file release (the common case: one archive,
                    // or one loose file) keeps a bare `prefix.ext`;
                    // multiple unrelated files use a `.partNN` marker
                    // ahead of the extension instead of a bare `-NN`
                    // suffix. Indexer subject-cleaning regexes (e.g.
                    // nZEDb's `CollectionsCleaning::generic()`) strip a
                    // known `\.part\d*(\.rar)?` prefix together with the
                    // trailing extension as one unit — the same way they
                    // already strip `.volNNN+NNN.par2` — so every file
                    // collapses back to the same collection key. A bare
                    // `-NN` before the extension isn't part of that
                    // pattern and survives cleaning, giving each file its
                    // own key and defeating the grouping `full-shared`/
                    // `light` exist for (confirmed empirically: real
                    // upload's `.par2`/`.volNNN+NNN.par2` set grouped on
                    // binsearch, its loose `-NN.mkv` files did not).
                    if files.len() == 1 {
                        format!("{prefix}{ext}")
                    } else {
                        format!("{prefix}.part{:02}{ext}", idx + 1)
                    }
                };
                // The shared prefix stays on the subject — that's what
                // indexers actually key "same release" grouping off of
                // (issue #58/#68, both subject-based). Under `light`,
                // the yEnc body name= is that same string verbatim
                // (issue #106's "option 1" — restores full-shared's
                // pre-0.6.1 behavior for indexers that key grouping off
                // an exact Subject/yEnc-name match). Under `full-shared`,
                // the yEnc name= starts with that same prefix but adds
                // its own random suffix instead: an indexer that can
                // only see the yEnc body still recognises the article as
                // part of the release, while the random suffix avoids
                // an exact Subject/yEnc match.
                let yenc_name = if config.obfuscate == ObfuscateMode::Light {
                    name.clone()
                } else {
                    obfuscated_name_with_prefix(prefix)
                };
                (name, yenc_name, from)
            }
        };
        let date = resolve_date(config.date.as_deref());
        metas.push(Arc::new(FileMeta {
            path: path.clone(),
            real_name,
            client_path,
            subject_name,
            yenc_name,
            from,
            date,
            size: md.len(),
            // Assigned below, once `metas`' final posting order is settled —
            // see the `config.file_counter` pass after the File-ID sort.
            file_index: 0,
        }));
    }

    // PAR2 numbers its input blocks by walking the recovery-set files in
    // File-ID order (par2 spec, Main packet). The producer feeds slices to the
    // encoder in `metas` order, so for a multi-file set to be repairable
    // `metas` must already be sorted by File ID. A single-file set is
    // trivially ordered; with PAR2 disabled the order is irrelevant.
    if config.par2 > 0 && metas.len() > 1 {
        let mut keyed = Vec::with_capacity(metas.len());
        for meta in &metas {
            let md5_16k = file_md5_16k(&meta.path, meta.size).await?;
            // Use the canonical client path so File ID ordering and FileDesc
            // serialization cannot disagree.
            let file_id = packet::compute_file_id(&md5_16k, meta.size, &meta.client_path);
            keyed.push((file_id, meta.clone()));
        }
        keyed.sort_by_key(|(file_id, _)| *file_id);
        metas = keyed.into_iter().map(|(_, meta)| meta).collect();
    }

    // `--file-counter`'s `[filenum/total]` numbers every file in the release,
    // so it can only be assigned now that the full file list is settled — not
    // at push time above.
    //
    // The number must follow the release's own order (`part1.rar` is `[1/N]`,
    // the PAR2 set closes it out), *not* `metas`' order: the File-ID sort
    // above keys on an MD5, i.e. it shuffles the volumes with respect to their
    // volume numbers. Indexers sort a collection by Subject and the counter is
    // the Subject's leading field, so inheriting that order listed the release
    // scrambled — a real upload came out with `part4.rar` as `[1/14]`.
    // `metas` itself stays in File-ID order, since the producer feeds PAR2
    // slices in that order and the par2 spec requires it (see the sort above).
    if config.file_counter {
        let mut order: Vec<usize> = (0..metas.len()).collect();
        order.sort_by(|&a, &b| natural_cmp(&metas[a].real_name, &metas[b].real_name));
        let mut rank = vec![0u32; metas.len()];
        for (pos, &idx) in order.iter().enumerate() {
            rank[idx] = pos as u32 + 1;
        }
        metas = metas
            .into_iter()
            .zip(rank)
            .map(|(m, file_index)| {
                Arc::new(FileMeta {
                    file_index,
                    ..(*m).clone()
                })
            })
            .collect();
    }

    let mut initial_segments = 0;
    for meta in &metas {
        initial_segments += yenc::segments(meta.size, config.article_size).len() as u64;
    }

    info!(
        entry = entry_label.unwrap_or(""),
        files = metas.len(),
        segments = initial_segments,
        article_size = config.article_size,
        par2_pct = config.par2,
        "upload plan"
    );

    let servers: Arc<Vec<crate::config::ServerEntry>> = Arc::new(config.all_servers().collect());
    // This validation intentionally happens before workers exist, so a bad
    // SOCKS5 credential can never send an article. Keep the resulting status
    // until after `Started`, because the terminal resets its panel then.
    let proxy_status = if let Some(proxy) = config.proxy.as_ref() {
        for server in servers.iter() {
            crate::nntp::validate_proxy(server).await?;
        }
        Some(if config.proxy_check_ip {
            format!(
                "SOCKS5 proxy active via {}; exit IP {}",
                proxy.address(),
                crate::nntp::proxy_exit_ip(proxy).await?
            )
        } else {
            format!(
                "SOCKS5 proxy active via {}; remote DNS enabled",
                proxy.address()
            )
        })
    } else {
        None
    };
    let total_conns = config.total_connections();

    let check_enabled = config.check && !config.dry_run && !config.par2_only;
    let (check_conns, upload_conns) = split_connections(config, check_enabled)?;

    let worker_count = if config.par2_only {
        0
    } else {
        upload_conns.max(1).min(initial_segments.max(1) as usize)
    };
    info!(
        workers = worker_count,
        check_workers = check_conns,
        connections = total_conns,
        "connection pool"
    );

    // Pre-seed the buffer pool with enough buffers to keep all workers and the
    // double-buffer reader supplied without allocating during the hot path.
    let pool_size = worker_count + 4;
    let initial_pool: Vec<Vec<u8>> = (0..pool_size)
        .map(|_| vec![0u8; config.article_size])
        .collect();

    // Unique per call to this function, i.e. per posting run — not per
    // process. `--each`/`--season` with `--jobs > 1` spawn several runs
    // concurrently in the same process; each needs its own PAR2 temp
    // directory (see `par2_temp_dir`'s doc comment / GitHub issue #67).
    static RUN_COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let run_id = RUN_COUNTER.fetch_add(1, Ordering::Relaxed);

    // Computed once, unconditionally, and reused below for `total_files`,
    // `par2_bytes_hint`, and the `--par2-before-upload` decision.
    // `par2_geometry` is metadata-only (file sizes + config, no I/O — see
    // its doc comment) so this is exact, not an estimate, and safe to
    // compute before `producer` actually runs the encoder.
    let (par2_slice_size, _total_slices, recovery_count) = par2_geometry(&metas, config);

    // Total release file count for `--file-counter`: data files, plus (when
    // there's any recovery data to write) the index file and every volume
    // `plan_volumes` will produce. Gated on `recovery_count > 0`, exactly
    // like `producer`'s own `worker_opt`/index-write gate — not on
    // `config.par2 > 0` directly, since `par2_geometry` can still land on
    // zero recovery blocks with PAR2 "on" (e.g. a tiny release where
    // `total_slices * pct / 100` floors to 0), in which case `producer`
    // never writes an index or volumes at all.
    let total_files: u32 = if config.file_counter {
        let par2_file_count = if recovery_count > 0 {
            usize::from(config.obfuscate.policy().publish_par2_index)
                + layout::plan_volumes(recovery_count as u32).len()
        } else {
            0
        };
        (metas.len() + par2_file_count) as u32
    } else {
        0
    };

    let shared = Arc::new(Shared {
        config: config.clone(),
        servers,

        results: Arc::new(Mutex::new(Vec::new())),
        failures: Mutex::new(Vec::new()),
        failed_tasks: Mutex::new(Vec::new()),
        events,
        cancelled: Arc::new(AtomicBool::new(false)),
        paused: Arc::new(AtomicBool::new(false)),
        resume: resume_arc,
        resume_path: resume_path_owned,
        spool_dir: spool_dir_owned,
        pool: Arc::new(Mutex::new(initial_pool)),
        encode_pool: Arc::new(Mutex::new(Vec::new())),
        total_retries: std::sync::atomic::AtomicUsize::new(0),
        post_group: pick_post_group(&config.groups),
        release_prefix,
        release_from,
        run_id,
        total_files,
        check_tx: Mutex::new(None),
    });

    // Announce the work plan: one `FileEntry` per source file, with the
    // segment count posting will use. PAR2 files are added later, once the
    // data pass has computed them, via `ProgressEvent::QueueExtended`.
    let (mode, target) = if config.par2_only {
        (RunMode::Par2Only, None)
    } else if config.dry_run {
        (RunMode::DryRun, None)
    } else {
        // Every configured server (primary + extra_servers) gets a share of
        // worker connections from the start (see `assign_workers`), unlike
        // `groups` — where only one of the configured groups is picked at
        // random per run — so the full server list is already known here,
        // not just after the fact. Reporting only `config.host` (the
        // primary) used to make a failover/multi-provider run look
        // single-server for its entire duration.
        let all_servers: Vec<_> = config.all_servers().collect();
        let label = target_label(&all_servers, config.total_connections());
        (RunMode::Post, Some(label))
    };
    let _ = &target; // used below
                     // Exact PAR2 recovery-set geometry, computed with the same formula
                     // `producer` will actually use — not an estimate. This lets the total
                     // segment/byte counts be seeded correctly up front instead of jumping
                     // once PAR2 encoding finishes and its volumes get queued for posting.
    let (par2_bytes_hint, par2_segments_hint) =
        if config.par2 > 0 && !config.par2_only && !config.dry_run {
            let recovery_bytes = recovery_count as u64 * par2_slice_size as u64;
            let packet_overhead = recovery_count as u64 * packet::HEADER_LEN as u64;
            // Small fixed overhead for the index file's Main/FileDesc/IFSC
            // packets — negligible next to recovery_bytes, not worth
            // computing exactly for a progress estimate.
            let base_est = metas.len() as u64 * 128 + 4096;
            let metadata_copies = layout::plan_volumes(recovery_count as u32).len() as u64
                + u64::from(config.obfuscate.policy().publish_par2_index);
            let bytes_hint = recovery_bytes + packet_overhead + base_est * metadata_copies;
            let segments_hint = yenc::segments(bytes_hint, config.article_size).len() as u64;
            (bytes_hint, segments_hint)
        } else {
            (0, 0)
        };
    let file_entries = metas
        .iter()
        .map(|m| FileEntry {
            name: m.real_name.clone(),
            segments: yenc::segments(m.size, config.article_size).len() as u64,
            bytes: m.size,
        })
        .collect();
    shared.emit(ProgressEvent::Started {
        mode,
        files: file_entries,
        connections: worker_count,
        check_connections: check_conns,
        target,
        par2_bytes_hint,
        par2_segments_hint,
    });

    // Warn when the release contains 0-byte files: download clients identify
    // obfuscated files by their md5_16k hash and cannot match empty files,
    if let Some(text) = proxy_status {
        shared.emit(ProgressEvent::ProxyStatus { text });
    }
    // so they end up misplaced after download.  Compression (--compress=rar
    // or --compress=7z) avoids the issue entirely.
    let zero_byte_names: Vec<&str> = metas
        .iter()
        .filter(|m| m.size == 0)
        .map(|m| m.client_path.as_str())
        .collect();
    if !zero_byte_names.is_empty() {
        let names = zero_byte_names.join(", ");
        shared.emit(ProgressEvent::Status {
            text: format!(
                "warning: release contains {n} empty file(s) ({names}); \
                 download clients cannot place empty files automatically — \
                 consider using --compress=rar or --compress=7z",
                n = zero_byte_names.len(),
            ),
        });
    }

    let cancel_handle = {
        let shared = shared.clone();
        tokio::spawn(async move {
            if external_cancel.is_none() && external_pause.is_none() {
                std::future::pending::<()>().await;
            }
            loop {
                if let Some(ref flag) = external_cancel {
                    if flag.load(Ordering::Relaxed) {
                        shared.cancelled.store(true, Ordering::Relaxed);
                        shared.emit(ProgressEvent::Interrupted);
                        return;
                    }
                }
                if let Some(ref flag) = external_pause {
                    let want = flag.load(Ordering::Relaxed);
                    if shared.paused.swap(want, Ordering::Relaxed) != want {
                        shared.emit(if want {
                            ProgressEvent::Paused
                        } else {
                            ProgressEvent::Resumed
                        });
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(50)).await;
            }
        })
    };

    // `--par2-before-upload`: when there's real recovery data to generate,
    // run PAR2 generation to completion *before* opening any NNTP
    // connection. `producer(.., None, .., 0)` writes every index/volume
    // file to `par2_dir` without posting (the `tx_opt: None` path already
    // used by `--par2-only`), and `active_connections: 0` means the PAR2
    // memory budget isn't shrunk to make room for connections that don't
    // exist yet — the connection pool and its workers only spin up further
    // down, once this is done. `post_pregenerated_release` then posts the
    // data files followed by the files this call already wrote, back to
    // back with no gap. See `ROADMAP.md` and GitHub issue #68.
    let will_defer = config.par2_before_upload && recovery_count > 0 && worker_count > 0;
    let par2_dir = par2_temp_dir(config.par2_temp_dir.as_deref(), run_id);
    let mut failure_reason: Option<String> = None;
    if will_defer {
        if let Err(e) = producer(metas.clone(), None, shared.clone(), 0).await {
            let description = format!("producer error: {e:#}");
            error!(error = %e, "producer error");
            shared.cancelled.store(true, Ordering::Relaxed);
            shared.emit(ProgressEvent::Failed {
                description: description.clone(),
            });
            failure_reason = Some(description);
        }
    }

    // One checkout of the episode's full budget so `--jobs` cannot sneak a
    // partial checkout in between check and upload (FIFO `acquire_many`
    // deadlock). Check workers share these slots; they never open extra TCP.
    let mut held_slots = if check_conns > 0 || worker_count > 0 {
        take_slots(
            broker.as_ref(),
            shared.servers.clone(),
            check_conns + upload_conns,
        )
        .await
    } else {
        Vec::new()
    };
    let check_slots: Vec<_> = held_slots
        .drain(..check_conns.min(held_slots.len()))
        .collect();
    let mut post_slots = held_slots;

    // Streaming check: every segment that gets a clean `240` is queued here
    // and STAT-checked a few seconds later, concurrently with the rest of
    // the upload, instead of waiting for the whole run to finish.
    let mut check_coordinator = if !check_slots.is_empty() {
        Some(spawn_check_coordinator(
            config.clone(),
            shared.post_group.clone(),
            Arc::clone(&shared.results),
            shared.events.clone(),
            Some(Arc::clone(&shared.cancelled)),
            check_slots,
            shared.resume.clone(),
        ))
    } else {
        None
    };
    if let Some(c) = check_coordinator.as_ref() {
        *shared.check_tx.lock().unwrap() = Some(c.sender());
    }

    crate::memory::set_phase(crate::memory::Phase::Posting);
    let t_post_start = std::time::Instant::now();
    let mut handles = Vec::with_capacity(worker_count);
    let mut encode_handles = Vec::new();
    let tx_opt = if worker_count > 0 && !post_slots.is_empty() {
        let spawn_n = worker_count.min(post_slots.len());
        let ready_n = ready_queue_depth(spawn_n);
        let post_depth = (ready_n / spawn_n).max(2);
        let mut post_senders = Vec::with_capacity(spawn_n);
        let mut post_receivers = Vec::with_capacity(spawn_n);
        for _ in 0..spawn_n {
            let (tx, rx) = tokio::sync::mpsc::channel(post_depth);
            post_senders.push(tx);
            post_receivers.push(rx);
        }
        let post_disp = Arc::new(TaskDispatcher::new(post_senders));
        let spawned: Vec<_> = post_slots.drain(..spawn_n).collect();
        for (idx, (slot, rx)) in spawned.into_iter().zip(post_receivers).enumerate() {
            handles.push(tokio::spawn(worker(shared.clone(), rx, idx, slot)));
        }

        let n_enc = encode_concurrency(parmesan::performance_core_count(), worker_count);
        let enc_depth = (ready_n / n_enc).max(2);
        let mut enc_senders = Vec::with_capacity(n_enc);
        let mut enc_receivers = Vec::with_capacity(n_enc);
        for _ in 0..n_enc {
            let (tx, rx) = tokio::sync::mpsc::channel(enc_depth);
            enc_senders.push(tx);
            enc_receivers.push(rx);
        }
        info!(
            encode_workers = n_enc,
            ready_queue = ready_n,
            "article encode pool"
        );
        for rx in enc_receivers {
            let shared = shared.clone();
            let post_disp = post_disp.clone();
            encode_handles.push(tokio::spawn(async move {
                encode_worker(shared, rx, post_disp).await;
            }));
        }
        Some(TaskDispatcher::new(enc_senders))
    } else {
        None
    };

    // The second signal (or the first signal's deadline) is deliberately
    // handled here rather than inside every NNTP read/write: aborting these
    // tasks drops their `TcpStream`s immediately, while preserving the
    // single final resume-state persistence path below.
    let mut force_abort = crate::cancel::abort_requested();
    if force_abort {
        shared.cancelled.store(true, Ordering::Release);
        shared.emit(ProgressEvent::Aborted);
    }

    // Producer (or, when PAR2 was already generated above, the
    // already-generated-files poster) runs in this thread. Skipped entirely
    // if the generation phase above already failed — nothing valid to post.
    if failure_reason.is_none() && !force_abort {
        let producer_shared = shared.clone();
        let producer_result: std::pin::Pin<
            Box<dyn std::future::Future<Output = Result<()>> + Send>,
        > = if will_defer {
            Box::pin(async move {
                match tx_opt.as_ref() {
                    Some(tx) => {
                        post_pregenerated_release(
                            &metas,
                            &par2_dir,
                            recovery_count,
                            tx,
                            &producer_shared,
                        )
                        .await
                    }
                    None => Ok(()),
                }
                // `tx_opt` is owned by this future, so it closes here
                // even when a force-abort cancels the future.
            })
        } else {
            Box::pin(producer(metas, tx_opt, producer_shared, total_conns))
        };
        let result = tokio::select! {
            result = producer_result => Some(result),
            _ = crate::cancel::aborted() => {
                force_abort = true;
                None
            }
        };
        if force_abort {
            shared.cancelled.store(true, Ordering::Release);
            shared.emit(ProgressEvent::Aborted);
        } else if let Some(Err(e)) = result {
            let description = format!("producer error: {e:#}");
            // `Failed` alone only reaches `--output-format json` consumers; log
            // it too so the reason survives in the session log file even when
            // the human-readable renderer (which only shows it via `Failed`,
            // see `ui::terminal`) is what's on screen.
            error!(error = %e, "producer error");
            shared.cancelled.store(true, Ordering::Relaxed);
            shared.emit(ProgressEvent::Failed {
                description: description.clone(),
            });
            failure_reason = Some(description);
        }
    }

    if !force_abort {
        while let Some(mut handle) = encode_handles.pop() {
            tokio::select! {
                _ = &mut handle => {},
                _ = crate::cancel::aborted() => {
                    handle.abort();
                    force_abort = true;
                    shared.cancelled.store(true, Ordering::Release);
                    shared.emit(ProgressEvent::Aborted);
                    break;
                }
            }
        }
    }
    if force_abort {
        for handle in encode_handles {
            handle.abort();
        }
        for handle in handles {
            handle.abort();
        }
        shared.cancelled.store(true, Ordering::Release);
    } else {
        while let Some(mut handle) = handles.pop() {
            tokio::select! {
                result = &mut handle => {
                    if let Ok(slot) = result {
                        post_slots.push(slot);
                    }
                }
                _ = crate::cancel::aborted() => {
                    handle.abort();
                    force_abort = true;
                    break;
                }
            }
        }
        if force_abort {
            for handle in handles {
                handle.abort();
            }
            shared.cancelled.store(true, Ordering::Release);
            shared.emit(ProgressEvent::Aborted);
        }
    }

    let mut failures = std::mem::take(&mut *shared.failures.lock().unwrap());
    let mut failed_tasks = std::mem::take(&mut *shared.failed_tasks.lock().unwrap());
    let cancelled_during_post = shared.cancelled.load(Ordering::Relaxed);

    // Blind retry for segments that never got a `240` in the main loop
    // (connection drops, timeouts, etc — never confirmed by the server at
    // all). Runs on the post slots still held — never `ConnectionSlot::new`.
    // Recovered segments flow into the same streaming check queue as
    // everything else, so they get the same STAT confirmation before the
    // run reports them as posted.
    if !failed_tasks.is_empty() && !cancelled_during_post {
        let n = failed_tasks.len();
        info!(count = n, "retrying segments that failed during upload");
        let recovered = repost_failed_tasks(
            config,
            &failed_tasks,
            &shared.post_group,
            shared.events.as_ref(),
            Some(&shared.cancelled),
            &mut post_slots,
        )
        .await
        .unwrap_or_else(|e| {
            warn!(error = %e, "retry: repost_failed_tasks error");
            Vec::new()
        });
        let recovered_keys: std::collections::HashSet<(String, u32, u32)> = recovered
            .iter()
            .map(|s| (s.file_name.clone(), s.part, s.total))
            .collect();
        for seg in recovered {
            if let Some(resume) = &shared.resume {
                resume.lock().unwrap().record_with(
                    &seg.file_name,
                    seg.part,
                    SegmentRecord {
                        message_id: seg.message_id.clone(),
                        bytes: seg.bytes,
                        confirmed: false,
                        check_disabled: !shared.config.check,
                        server_idx: seg.server_idx,
                        wire_identity: Some(persisted_identity(
                            &seg.wire_name,
                            &seg.wire_yenc_name,
                            &seg.from,
                            &seg.date,
                        )),
                    },
                );
            }
            shared.results.lock().unwrap().push(seg.clone());
            if let Some(tx) = shared.check_tx.lock().unwrap().as_ref() {
                let _ = tx.send(seg);
            }
        }
        failed_tasks.retain(|t| !recovered_keys.contains(&(t.file_name.clone(), t.part, t.total)));
        failures.retain(|f| {
            !recovered_keys.iter().any(|(name, part, total)| {
                f.starts_with(name.as_str()) && f.contains(&format!("{part}/{total}"))
            })
        });
    }

    // The PAR2 files posted in normal mode are written to a per-process temp
    // directory purely as an intermediate. Cleanup is deliberately *not* done
    // here: the streaming check's repost path may still need to re-read a
    // PAR2 file's bytes while it drains below. The caller is responsible for
    // removing `par2_temp_dir()` once it's truly done with the run (see
    // `run_single_upload` / `run_upload`).
    // Close the STAT feeder before drain. Shared outlives the coordinator;
    // leaving this sender alive would hang `finish_and_drain` forever.
    let _ = shared.check_tx.lock().unwrap().take();
    crate::memory::set_phase(crate::memory::Phase::Check);
    let drain = if force_abort {
        // Dropping the coordinator aborts all STAT/repost tasks (its Drop
        // implementation must not merely detach them), so no NNTP timeout
        // can delay resume persistence.
        drop(check_coordinator.take());
        check::CheckDrain::default()
    } else if let Some(mut coordinator) = check_coordinator.take() {
        // Ownership transfer: no checkin in between post-join and scale_up.
        coordinator.scale_up(std::mem::take(&mut post_slots));
        let mut drain_handle = tokio::spawn(async move { coordinator.finish_and_drain().await });
        tokio::select! {
            result = &mut drain_handle => result.unwrap_or_default(),
            _ = crate::cancel::aborted() => {
                drain_handle.abort();
                let _ = drain_handle.await;
                shared.cancelled.store(true, Ordering::Release);
                shared.emit(ProgressEvent::Aborted);
                check::CheckDrain::default()
            }
        }
    } else {
        check::CheckDrain {
            slots: std::mem::take(&mut post_slots),
            ..check::CheckDrain::default()
        }
    };
    let mut still_missing = drain.still_missing;
    let mut inconclusive = drain.inconclusive;
    post_slots = drain.slots;
    // Re-read after drain: a cancel during the STAT wait must persist
    // unconfirmed records rather than treating the dumped queue as
    // MissingConfirmed (the watcher stays alive until after persist).
    let cancelled = shared.cancelled.load(Ordering::Relaxed);

    // One more, bounded automatic recovery attempt for a small stubborn
    // tail. The common real-world case this targets: posting finished, the
    // streaming check failed to confirm a handful of articles even after
    // every `check_post_retries` round, and the NZB is about to be refused.
    // Reposting those few articles right here — still in this same process,
    // with the source files still on disk — is strictly cheaper and simpler
    // than requiring the user to notice the failure and rerun with
    // `--resume` by hand. Only kicks in when the leftover count is small
    // enough (`check_recover_percent`/`check_recover_max`) to still count as
    // "cheap": a release with a large fraction missing looks like a
    // systemic server problem, not a handful of unlucky articles, and
    // retrying that automatically would just hammer an already-struggling
    // server.
    if !still_missing.is_empty() && !cancelled {
        let total = shared.results.lock().unwrap().len();
        if is_cheap_to_recover(still_missing.len(), total, config) {
            let candidates: Vec<PostedSegment> = {
                let results = shared.results.lock().unwrap();
                results
                    .iter()
                    .filter(|s| still_missing.contains(&s.message_id))
                    .cloned()
                    .collect()
            };
            // `recover_missing` itself emits `CheckRecoverStarted`/
            // `CheckRecoverProgress` (structured, so the renderer can show a
            // real progress box instead of a one-shot status line — see
            // `ui::terminal`'s "recover" box).
            //
            // `recover_missing` returns *fresh* Message-IDs (every repost
            // gets a new one — see `repost_one`), so its output can never be
            // matched directly against `still_missing`'s old ids. Snapshot
            // old-id -> (file_name, part) identity before the candidates are
            // moved into the call, so the retain below can match by that
            // identity instead.
            let old_identity: std::collections::HashMap<String, (String, u32)> = candidates
                .iter()
                .map(|c| (c.message_id.clone(), (c.file_name.clone(), c.part)))
                .collect();
            let recovered = check::recover_missing(
                config,
                &shared.post_group,
                candidates,
                shared.events.as_ref(),
                std::mem::take(&mut post_slots),
            )
            .await;
            post_slots = recovered.slots;
            {
                let mut results = shared.results.lock().unwrap();
                for seg in recovered
                    .recovered
                    .iter()
                    .chain(recovered.inconclusive.iter())
                {
                    if let Some(existing) = results
                        .iter_mut()
                        .find(|s| s.file_name == seg.file_name && s.part == seg.part)
                    {
                        *existing = seg.clone();
                    }
                }
            }
            if let Some(resume) = &shared.resume {
                let mut state = resume.lock().unwrap();
                for seg in &recovered.recovered {
                    state.record_with(
                        &seg.file_name,
                        seg.part,
                        SegmentRecord {
                            message_id: seg.message_id.clone(),
                            bytes: seg.bytes,
                            confirmed: true,
                            check_disabled: false,
                            server_idx: seg.server_idx,
                            wire_identity: Some(persisted_identity(
                                &seg.wire_name,
                                &seg.wire_yenc_name,
                                &seg.from,
                                &seg.date,
                            )),
                        },
                    );
                }
                for seg in &recovered.inconclusive {
                    state.record_with(
                        &seg.file_name,
                        seg.part,
                        SegmentRecord {
                            message_id: seg.message_id.clone(),
                            bytes: seg.bytes,
                            confirmed: false,
                            check_disabled: false,
                            server_idx: seg.server_idx,
                            wire_identity: Some(persisted_identity(
                                &seg.wire_name,
                                &seg.wire_yenc_name,
                                &seg.from,
                                &seg.date,
                            )),
                        },
                    );
                }
            }
            let recovered_keys: std::collections::HashSet<(String, u32)> = recovered
                .recovered
                .iter()
                .chain(recovered.inconclusive.iter())
                .map(|s| (s.file_name.clone(), s.part))
                .collect();
            still_missing.retain(|id| {
                !old_identity
                    .get(id)
                    .is_some_and(|key| recovered_keys.contains(key))
            });
            inconclusive.extend(recovered.inconclusive.into_iter().map(|s| s.message_id));
        }
    }

    // One checkin of the whole set so `--jobs` keeps the next episode
    // blocked on the semaphore until this episode is fully done.
    release_slots(broker.as_deref(), post_slots).await;

    // Whatever is left in `still_missing` at this point is confirmed bad:
    // the original POST got a `240`, but every STAT check and every repost
    // attempt (both the normal `check_post_retries` rounds and the recovery
    // pass above) failed to make the article retrievable. Its recorded
    // Message-ID must be forgotten now — otherwise a later `--resume` would
    // trust that known-bad ID and silently skip re-posting the segment,
    // producing an NZB that looks complete but references an article that
    // was never actually confirmed present. Cancel is not a confirmed miss:
    // keep those records as `confirmed: false` so `--resume --check` can
    // re-STAT the same ids.
    if let Some(resume) = &shared.resume {
        if !cancelled && !still_missing.is_empty() {
            let results = shared.results.lock().unwrap();
            let mut state = resume.lock().unwrap();
            for id in &still_missing {
                if let Some(seg) = results.iter().find(|s| &s.message_id == id) {
                    state.remove(&seg.file_name, seg.part);
                    // No spool cleanup needed here: a segment can only reach
                    // `still_missing` after already being confirmed posted
                    // once (`commit_result`'s `posted: true` branch), and
                    // that branch already removes its spool entry — before
                    // the check coordinator that could ever mark it missing
                    // even sees it. See `crate::spool`.
                }
            }
        }
    }

    // Single, final resume-state persistence decision, replacing the old
    // per-segment write in `commit_result`. Persist whenever anything is
    // still unconfirmed: POST failures, MissingConfirmed (even with
    // `--allow-incomplete-nzb` — the opt-in publishes the NZB but a later
    // `--resume` can still fill the gap), Inconclusive, or a cancel that
    // already has Posted records. Complete runs delete the state file.
    if let (Some(resume), Some(rp)) = (&shared.resume, &shared.resume_path) {
        let has_post_failures = !failed_tasks.is_empty();
        let has_confirmed_missing = !cancelled && !still_missing.is_empty();
        let has_inconclusive = !inconclusive.is_empty();
        let has_progress = !resume.lock().unwrap().is_empty();
        let incomplete = has_post_failures
            || has_confirmed_missing
            || has_inconclusive
            || (cancelled && has_progress);
        if incomplete {
            let _ = resume.lock().unwrap().save(rp);
        } else {
            let _ = std::fs::remove_file(rp);
            if let Some(dir) = &shared.spool_dir {
                crate::spool::remove_all(dir);
            }
        }
    }

    shared.emit(ProgressEvent::Finished);

    let mut segments = std::mem::take(&mut *shared.results.lock().unwrap());
    // Natural (not lexicographic) by name, so the NZB lists `part2.rar` before
    // `part10.rar` — the same volume order `--file-counter` numbers by.
    segments.sort_by(|a, b| natural_cmp(&a.file_name, &b.file_name).then(a.part.cmp(&b.part)));

    // 26d/26g — network performance summary + post phase timing
    let total_retries = shared.total_retries.load(Ordering::Relaxed);
    info!(
        posted = segments.len(),
        failed = failures.len(),
        retries = total_retries,
        still_missing = still_missing.len(),
        inconclusive = inconclusive.len(),
        elapsed_ms = t_post_start.elapsed().as_millis(),
        phase = "post",
        "network summary"
    );

    let all_servers: Vec<_> = config.all_servers().collect();
    let mut used_server_idxs: Vec<usize> = segments.iter().map(|s| s.server_idx).collect();
    used_server_idxs.sort_unstable();
    used_server_idxs.dedup();
    let servers_used: Vec<String> = used_server_idxs
        .into_iter()
        .filter_map(|idx| all_servers.get(idx))
        .map(|s| s.host.clone())
        .collect();

    cancel_handle.abort();

    Ok(PostOutcome {
        segments,
        failures,
        failed_tasks,
        cancelled,
        groups: shared.post_group.clone(),
        still_missing,
        inconclusive,
        servers: servers_used,
        failure_reason,
        par2_temp_dir: par2_temp_dir(config.par2_temp_dir.as_deref(), shared.run_id),
    })
}

/// Per-run temp directory holding the intermediate PAR2 files written during
/// a normal posting run. Keyed by `run_id` (unique per [`PostOutcome`]), not
/// just the process ID: `--each`/`--season` with `--jobs > 1` run several
/// posting tasks concurrently *in the same process*, and a PID-only path
/// used to collide them all into one directory — one entry finishing would
/// delete PAR2 source files a sibling entry was still reading to repost
/// (see GitHub issue #67). Callers should remove
/// `par2_temp_dir(outcome.run_id)` (when `!config.par2_only`) once the
/// *entire* run is done — including any `--check` repost pass or end-of-run
/// failed-task retry — not right after the main post loop finishes, since
/// both of those may still need to re-read a PAR2 file's bytes from disk.
///
/// `base` overrides the parent directory the per-run subdirectory is created
/// under (see `Config::par2_temp_dir`). `None` falls back to
/// `std::env::temp_dir()`, which may sit on a different filesystem — with
/// less free space or a stricter quota — than the destination disk.
pub fn par2_temp_dir(base: Option<&Path>, run_id: u64) -> PathBuf {
    let base = base
        .map(Path::to_path_buf)
        .unwrap_or_else(std::env::temp_dir);
    base.join(format!("parmesan_{}_{run_id}", std::process::id()))
}

/// Restrict the global Rayon pool to physical cores. The PAR2 encoder is pure
/// SIMD/ALU work; sibling hyperthreads contend for the same execution ports
/// and add almost nothing, so one worker per logical CPU only heats the
/// machine. Called once; a no-op if a global pool already exists.
fn configure_rayon(threads: usize) {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let n = if threads > 0 {
            threads
        } else {
            parmesan::performance_core_count()
        };
        // Thread *count* stays at physical cores — PAR2 is the genuinely
        // CPU-bound stage and wants them. Only the per-thread stack shrinks,
        // from Rust's 2 MiB default: on a 128-core host that is ~130 MiB of
        // address space reclaimed for the PAR2 budget itself, at no cost to
        // throughput. See `crate::memory` for the per-thread measurements.
        let _ = rayon::ThreadPoolBuilder::new()
            .num_threads(n)
            .stack_size(crate::memory::ThreadTuning::detect().thread_stack_size)
            .build_global();
    });
}

/// MD5 of a file's first 16 KiB — the PAR2 "16k hash" half of a File ID.
/// Read in a tiny pre-pass so files can be ordered before the encode pass.
async fn file_md5_16k(path: &std::path::Path, size: u64) -> Result<[u8; 16]> {
    let mut file = File::open(path)
        .await
        .with_context(|| format!("opening `{}`", path.display()))?;
    let take = size.min(16 * 1024) as usize;
    let mut buf = vec![0u8; take];
    file.read_exact(&mut buf)
        .await
        .with_context(|| format!("reading `{}`", path.display()))?;
    let mut hasher = FileHasher::new();
    hasher.update(&buf);
    Ok(hasher.finish().md5_16k)
}

/// Directory where `--par2-only` writes the recovery set.
///
/// File Description packets store each file's *relative* name, so `par2` must
/// be run from the directory that contains the root folder. The published
/// name has one path component per directory level; stripping that many
/// components off the filesystem path lands exactly there. A loose file
/// (single component) yields its parent directory, as before.
fn par2_output_dir(meta: &FileMeta) -> PathBuf {
    let depth = meta.real_name.split('/').count();
    meta.path
        .ancestors()
        .nth(depth)
        .filter(|p| !p.as_os_str().is_empty())
        .map(|p| p.to_path_buf())
        .unwrap_or_else(|| PathBuf::from("."))
}

/// Posts a release whose PAR2 index/volumes were already fully generated by
/// an earlier `producer(metas, None, shared, 0)` call — see
/// `--par2-before-upload` in `post_files_with_progress_and_cancel`: that call
/// writes every index/volume file to `par2_dir` without posting anything
/// (`tx_opt: None` takes the `par2_only_ingest` path). This posts the data
/// files, then reads back and posts the already-written index and every
/// volume, so the whole release goes out back to back with no gap. Volume
/// file names/`file_index`es are recomputed from `recovery_count` alone
/// (via `layout::plan_volumes`), matching exactly what the generation call
/// already wrote — no I/O needed to know what's there.
async fn post_pregenerated_release(
    metas: &[Arc<FileMeta>],
    par2_dir: &Path,
    recovery_count: usize,
    tx: &TaskDispatcher<PostTask>,
    shared: &Arc<Shared>,
) -> Result<()> {
    if shared.cancelled.load(Ordering::Relaxed) {
        return Ok(());
    }
    post_data_files(metas, tx, shared).await?;

    if shared.config.obfuscate.policy().publish_par2_index {
        let index_name = layout::index_name(par2_release_base(&metas[0].real_name));
        let index_path = par2_dir.join(&index_name);
        let wire_override = shared.release_prefix.as_deref().map(layout::index_name);
        let file_index = metas.len() as u32 + 1;
        push_par2_file(
            &index_path,
            index_name,
            wire_override,
            file_index,
            shared,
            tx,
        )
        .await?;
    }

    let volumes = layout::plan_volumes(recovery_count as u32);
    for (vol_idx, vol) in volumes.iter().enumerate() {
        let vol_name = layout::volume_name(par2_release_base(&metas[0].real_name), *vol);
        let vol_path = par2_dir.join(&vol_name);
        let wire_override = shared
            .release_prefix
            .as_deref()
            .map(|prefix| layout::volume_name(prefix, *vol));
        let index_offset = u32::from(shared.config.obfuscate.policy().publish_par2_index);
        let file_index = metas.len() as u32 + 1 + index_offset + vol_idx as u32;
        push_par2_file(&vol_path, vol_name, wire_override, file_index, shared, tx).await?;
    }
    Ok(())
}

/// One read article: byte offset, buffer, and (on the file's last article)
/// the whole-file CRC-32 needed for the `=yend` line.
type ReadArticle = (u64, Vec<u8>, Option<u32>);

/// Spawns the double-buffered reader task shared by the data-posting loop in
/// `producer` and `post_data_files`: reads `segments` from `path` into a
/// bounded channel of capacity 2 so the OS can fetch article N+1 while the
/// caller processes article N, accumulating the whole-file CRC-32 (needed on
/// the `=yend` line of the last segment) as it goes.
fn spawn_double_buffered_reader(
    path: PathBuf,
    segments: Vec<(u64, usize)>,
    shared: &Arc<Shared>,
) -> (
    tokio::sync::mpsc::Receiver<ReadArticle>,
    tokio::task::JoinHandle<Result<()>>,
) {
    let (read_tx, read_rx) = tokio::sync::mpsc::channel::<ReadArticle>(2);
    let reader_shared = shared.clone();
    let reader_handle = tokio::spawn(async move {
        let mut file = File::open(&path).await?;
        let mut crc = yenc::Crc32::new();
        let last_idx = segments.len().saturating_sub(1);
        for (idx, (offset, len)) in segments.into_iter().enumerate() {
            // Phase 12b: acquire a buffer from the shared pool if available,
            // otherwise allocate. Workers return buffers to the same pool
            // after yEnc encoding.
            let mut buf = reader_shared
                .try_acquire_buffer(len)
                .context("allocating article buffer")?;
            file.read_exact(&mut buf).await?;
            crc.update(&buf);
            let full_crc32 = (idx == last_idx).then(|| crc.finalize());
            if read_tx.send((offset, buf, full_crc32)).await.is_err() {
                break; // caller dropped its end (cancelled)
            }
        }
        Ok::<_, anyhow::Error>(())
    });
    (read_rx, reader_handle)
}

/// Posts every data file's articles with no PAR2 involvement. Called from
/// `post_pregenerated_release` (`--par2-before-upload`, after PAR2
/// generation has already fully completed) to post the data files
/// immediately before the already-generated PAR2 index/volumes so the whole
/// release goes out back to back with no gap. Mirrors the data-posting half
/// of `producer`'s interleaved per-file loop, minus the PAR2 accumulation,
/// which is unnecessary here since PAR2 is already on disk.
async fn post_data_files(
    metas: &[Arc<FileMeta>],
    tx: &TaskDispatcher<PostTask>,
    shared: &Arc<Shared>,
) -> Result<()> {
    let article_size = shared.config.article_size;
    for meta in metas {
        let segments: Vec<(u64, usize)> = yenc::segments(meta.size, article_size);
        let total_parts = segments.len() as u32;
        const CHUNK_SIZE: u64 = 8 * 1024 * 1024;
        let mut file_buf = None;
        let mut read_rx = None;
        let mut reader_handle = None;

        if meta.size <= CHUNK_SIZE {
            file_buf = Some(
                tokio::fs::read(&meta.path)
                    .await
                    .with_context(|| format!("reading `{}`", meta.path.display()))?,
            );
        } else {
            let (rx, handle) =
                spawn_double_buffered_reader(meta.path.clone(), segments.clone(), shared);
            read_rx = Some(rx);
            reader_handle = Some(handle);
        }

        let mut crc = yenc::Crc32::new();
        let last_idx = segments.len().saturating_sub(1);

        let mut i: u32 = 0;
        for (idx, &(offset, len)) in segments.iter().enumerate() {
            if shared.cancelled.load(Ordering::Relaxed) {
                if let Some(handle) = reader_handle {
                    let _ = handle.await;
                }
                return Ok(());
            }

            let (buf, file_crc32) = if let Some(fb) = &file_buf {
                let mut buf = shared
                    .try_acquire_buffer(len)
                    .context("allocating article buffer")?;
                let start = offset as usize;
                buf.copy_from_slice(&fb[start..start + len]);
                crc.update(&buf);
                let full_crc32 = (idx == last_idx).then(|| crc.finalize());
                (buf, full_crc32)
            } else {
                match read_rx.as_mut().unwrap().recv().await {
                    Some((_, buf, file_crc32)) => (buf, file_crc32),
                    None => break,
                }
            };

            i += 1;
            if tx
                .send(make_task(
                    meta.clone(),
                    i,
                    total_parts,
                    offset,
                    buf,
                    file_crc32,
                    &shared.config,
                ))
                .await
                .is_err()
            {
                if let Some(handle) = reader_handle {
                    let _ = handle.await;
                }
                return Ok(());
            }
        }
        if let Some(handle) = reader_handle {
            let _ = handle.await?;
        }
    }
    Ok(())
}

async fn push_par2_file(
    path: &PathBuf,
    real_name: String,
    wire_override: Option<String>,
    file_index: u32,
    shared: &Arc<Shared>,
    tx: &TaskDispatcher<PostTask>,
) -> Result<()> {
    let size = tokio::fs::metadata(path).await?.len();
    let client_path = normalize_client_path(&real_name, None)?.to_owned();
    let segments = yenc::segments(size, shared.config.article_size);
    let total = segments.len() as u32;

    shared.emit(ProgressEvent::QueueExtended {
        file: real_name.clone(),
        segments: total as u64,
        bytes: size,
    });

    let (subject_name, yenc_name, from) = if let Some(name) = wire_override {
        // `name` carries the release's shared prefix (FullShared/Light) —
        // keep it on the subject for indexer grouping. Under `light`, the
        // yEnc body name= is that same string verbatim; under `full-shared`
        // it starts with that same prefix but adds its own random suffix
        // instead — see the main FullShared/Light branch above for why
        // (issue #106).
        let prefix = shared.release_prefix.as_deref().unwrap_or_default();
        let yenc = if shared.config.obfuscate == ObfuscateMode::Light {
            name.clone()
        } else {
            // NZBGet uses the yEnc name while collecting recovery volumes;
            // retain only the technical extension so it cleans them up after
            // repair without exposing the real volume name.
            format!("{}.par2", obfuscated_name_with_prefix(prefix))
        };
        (name, yenc, shared.release_from.clone().unwrap_or_default())
    } else {
        match shared.config.obfuscate {
            ObfuscateMode::None => {
                let wn = client_path.clone();
                (wn.clone(), wn, shared.config.from.clone())
            }
            ObfuscateMode::Full | ObfuscateMode::Article | ObfuscateMode::FullShared => (
                obfuscated_name(),
                obfuscated_yenc_name(&real_name),
                random_from(),
            ),
            ObfuscateMode::Light => {
                let name = obfuscated_name();
                (name.clone(), name, random_from())
            }
        }
    };
    let date = resolve_date(shared.config.date.as_deref());

    let meta = Arc::new(FileMeta {
        path: path.clone(),
        real_name,
        client_path,
        subject_name,
        yenc_name,
        from,
        date,
        size,
        file_index: if shared.config.file_counter {
            file_index
        } else {
            0
        },
    });

    // Whole-file CRC-32 accumulated as this same loop reads the file for
    // upload, rather than in a separate pre-pass — see the reader task in
    // `producer` for the equivalent path used by the main input files.
    let mut crc = yenc::Crc32::new();
    let last_idx = total.saturating_sub(1);
    let mut file = tokio::fs::File::open(path).await?;
    for (i, (offset, len)) in segments.into_iter().enumerate() {
        let mut buf = shared
            .try_acquire_buffer(len)
            .context("allocating PAR2 file buffer")?;
        file.read_exact(&mut buf).await?;
        crc.update(&buf);
        let file_crc32 = (i as u32 == last_idx).then(|| crc.finalize());
        if tx
            .send(make_task(
                meta.clone(),
                i as u32 + 1,
                total,
                offset,
                buf,
                file_crc32,
                &shared.config,
            ))
            .await
            .is_err()
        {
            break;
        }
    }
    Ok(())
}

/// Per-worker token-bucket rate limiter.
struct RateLimiter {
    /// Bytes per second; 0 = unlimited.
    rate: u64,
    tokens: f64,
    last: Instant,
}

impl RateLimiter {
    fn new(rate: u64) -> Self {
        RateLimiter {
            rate,
            tokens: rate as f64,
            last: Instant::now(),
        }
    }

    /// Wait until `bytes` tokens are available, then consume them.
    async fn acquire(&mut self, bytes: usize) {
        if self.rate == 0 {
            return;
        }
        let now = Instant::now();
        let elapsed = now.duration_since(self.last).as_secs_f64();
        self.tokens = (self.tokens + elapsed * self.rate as f64).min(self.rate as f64);
        self.last = now;

        let bytes_f = bytes as f64;
        if self.tokens >= bytes_f {
            self.tokens -= bytes_f;
        } else {
            let needed = bytes_f - self.tokens;
            let wait = Duration::from_secs_f64(needed / self.rate as f64);
            tokio::time::sleep(wait).await;
            self.tokens = 0.0;
            self.last = Instant::now();
        }
    }
}

async fn encode_worker(
    shared: Arc<Shared>,
    mut rx: tokio::sync::mpsc::Receiver<PostTask>,
    post_tx: Arc<TaskDispatcher<ReadyArticle>>,
) {
    while let Some(task) = rx.recv().await {
        if shared.cancelled.load(Ordering::Relaxed) {
            break;
        }
        if let Some(ready) = prepare_ready(&shared, task).await {
            if post_tx.send(ready).await.is_err() {
                break;
            }
        }
    }
}

/// Resume skip / spool / yEnc. `None` means the segment is already done
/// (skipped or re-queued for STAT of a stored id).
async fn prepare_ready(shared: &Arc<Shared>, mut task: PostTask) -> Option<ReadyArticle> {
    if let Some(resume) = &shared.resume {
        let existing = resume
            .lock()
            .unwrap()
            .get(&task.meta.real_name, task.part)
            .cloned();
        match resume_action(shared.config.check, existing.as_ref()) {
            ResumeAction::Post => {}
            action @ (ResumeAction::Skip | ResumeAction::ReStatStoredId) => {
                let existing = existing.expect("skip/re-STAT arms require a record");
                let wire_subject = existing
                    .wire_identity
                    .as_ref()
                    .map(|i| i.subject_name.as_str())
                    .unwrap_or(&task.subject_name);
                let wire_yenc = existing
                    .wire_identity
                    .as_ref()
                    .map(|i| i.yenc_name.as_str())
                    .unwrap_or(&task.yenc_name);
                let from = existing
                    .wire_identity
                    .as_ref()
                    .map(|i| i.from.as_str())
                    .unwrap_or(&task.from);
                let date = existing
                    .wire_identity
                    .as_ref()
                    .map(|i| (i.date.clone(), i.unix_date))
                    .unwrap_or_else(|| task.date.clone());
                let seg = PostedSegment {
                    file_name: task.meta.real_name.clone(),
                    file_path: Arc::from(task.meta.path.as_path()),
                    subject_name: Arc::from(task.meta.client_path.as_str()),
                    wire_name: Arc::from(wire_subject),
                    wire_yenc_name: Arc::from(wire_yenc),
                    file_size: task.meta.size,
                    part: task.part,
                    total: task.total,
                    message_id: existing.message_id,
                    bytes: existing.bytes,
                    from: Arc::from(from),
                    date,
                    full_crc32: task.file_crc32.unwrap_or(0),
                    server_idx: existing.server_idx,
                    file_index: task.meta.file_index,
                    total_files: shared.total_files,
                };
                shared.results.lock().unwrap().push(seg.clone());
                if action == ResumeAction::ReStatStoredId {
                    if let Some(tx) = shared.check_tx.lock().unwrap().as_ref() {
                        let _ = tx.send(seg);
                    }
                }
                let raw_bytes = task.data.len() as u64;
                shared.release_buffer(task.data);
                shared.emit(ProgressEvent::SegmentDone {
                    file: task.meta.real_name.clone(),
                    bytes: raw_bytes,
                    ok: true,
                });
                return None;
            }
        }
    }

    let spooled = shared
        .spool_dir
        .as_ref()
        .and_then(|dir| crate::spool::read(dir, &task.meta.real_name, task.part));
    let spooled = match spooled {
        Some(entry)
            if shared.config.obfuscate != ObfuscateMode::None && entry.wire_identity.is_none() =>
        {
            // PST1 did not record the logical Subject/yEnc/From values next
            // to the raw bytes. Replaying it would post one identity while
            // recording another in resume/check state. Re-encoding is cheap
            // and gives the segment one coherent identity.
            warn!(
                file = %task.meta.real_name,
                part = task.part,
                "ignoring legacy spool entry without obfuscated wire identity"
            );
            None
        }
        entry => entry,
    };

    let (message_id, headers, encoded, encode_time) = if let Some(spooled) = spooled {
        if let Some(identity) = spooled.wire_identity {
            task.subject_name = identity.subject_name;
            task.yenc_name = identity.yenc_name;
            task.from = identity.from;
            task.date = (identity.date, identity.unix_date);
        }
        let encoded = yenc::EncodedPart {
            number: task.part,
            total: task.total,
            begin: 0,
            end: 0,
            crc32: 0,
            body: spooled.body,
        };
        (spooled.message_id, spooled.headers, encoded, Duration::ZERO)
    } else {
        let t_enc = Instant::now();
        let file_crc32 = task.file_crc32;
        let mut encode_buf = shared.acquire_encode_buf();
        let encoded = yenc::encode_part_into(
            &task.yenc_name,
            task.meta.size,
            yenc::PartSpec {
                number: task.part,
                total: task.total,
                offset: task.offset,
            },
            &task.data,
            shared.config.line_length,
            file_crc32,
            &mut encode_buf,
        );
        let encode_time = t_enc.elapsed();
        let message_id = generate_message_id(shared.config.message_id_domain.as_deref());
        let (rfc_date, _ts) = &task.date;
        if let Some(d) = &rfc_date {
            debug!(segment = %message_id, date = %d, "article date");
        }
        let article = Article {
            message_id: message_id.clone(),
            from: task.from.clone(),
            newsgroups: shared.post_group.clone(),
            subject: default_subject(
                &task.subject_name,
                task.part,
                task.total,
                (shared.total_files > 0).then_some((task.meta.file_index, shared.total_files)),
            ),
            date: rfc_date.clone(),
            no_archive: shared.config.no_archive,
        };
        let headers = article.build_headers();
        if let Some(dir) = &shared.spool_dir {
            let identity =
                persisted_identity(&task.subject_name, &task.yenc_name, &task.from, &task.date);
            if let Err(e) = crate::spool::write_with_identity(
                dir,
                &task.meta.real_name,
                task.part,
                &message_id,
                &headers,
                &encoded.body,
                &identity,
            )
            .await
            {
                warn!(error = %e, "resume: failed to write spool entry; continuing without it");
            }
        }
        (message_id, headers, encoded, encode_time)
    };
    let date = task.date.clone();
    Some(ReadyArticle {
        task,
        message_id,
        headers,
        encoded,
        encode_time,
        date,
    })
}

async fn worker(
    shared: Arc<Shared>,
    mut rx: tokio::sync::mpsc::Receiver<ReadyArticle>,
    conn_id: usize,
    mut slot: ConnectionSlot,
) -> ConnectionSlot {
    let mut rate_limiter = RateLimiter::new(
        // Divide the global rate across all workers proportionally.
        if shared.config.upload_rate > 0 {
            let total = shared.config.total_connections().max(1);
            (shared.config.upload_rate / total as u64).max(1)
        } else {
            0
        },
    );

    // pipeline_depth == 0 means adaptive: measure RTT on the first article and
    // compute depth = ceil(post_time / encode_time), capped at MAX_AUTO_PIPELINE_DEPTH.
    let cfg_depth = shared.config.pipeline_depth;
    let is_adaptive = cfg_depth == 0;
    // Effective depth used for batch-filling; starts at 1 until warm-up is done.
    let mut effective_depth: usize = if is_adaptive || cfg_depth == 1 {
        1
    } else {
        cfg_depth
    };
    let mut warmup_done = !is_adaptive; // true from the start when not adaptive

    // Track when the connection was last used so we can send periodic keepalives
    // on idle connections (prevents servers from closing them during long PAR2
    // computations, check-phase waits, and --each transitions).
    let keepalive_interval = shared.config.keepalive_interval;
    let keepalive_enabled = keepalive_interval > 0;
    // Short wakeup period while idle: cycle through all workers quickly enough
    // that every connection gets its keepalive before the server's idle timeout.
    // 2 s × 30 workers = 60 s worst-case round-trip, well within a 2-min timeout.
    const IDLE_POLL: Duration = Duration::from_secs(2);
    // Wakeup period while paused — much shorter than `IDLE_POLL`, which is
    // tuned for keepalive fan-out across many workers, not for how quickly a
    // paused worker notices `cancelled`/resume. Cancelling must stay roughly
    // as responsive while paused as it already is everywhere else.
    const PAUSE_POLL: Duration = Duration::from_millis(100);
    let mut last_used = Instant::now();

    'worker: loop {
        if shared.cancelled.load(Ordering::Relaxed) {
            break;
        }

        if shared.paused.load(Ordering::Relaxed) {
            // Suspended at a segment-batch boundary: keep the connection
            // alive (the same MODE READER keepalive used for idle time
            // within a run) without consuming from the queue, so a producer
            // racing ahead applies natural back-pressure instead of the run
            // continuing underneath a "paused" UI that lied about it.
            while shared.paused.load(Ordering::Relaxed) && !shared.cancelled.load(Ordering::Relaxed)
            {
                if keepalive_enabled
                    && last_used.elapsed() >= Duration::from_secs(keepalive_interval)
                {
                    slot.keepalive().await;
                    last_used = Instant::now();
                }
                tokio::time::sleep(PAUSE_POLL).await;
            }
            continue;
        }

        let first = loop {
            if keepalive_enabled && last_used.elapsed() >= Duration::from_secs(keepalive_interval) {
                slot.keepalive().await;
                last_used = Instant::now();
            }
            tokio::select! {
                task = rx.recv() => match task {
                    Some(t) => {
                        last_used = Instant::now();
                        break t;
                    }
                    None => break 'worker,
                },
                _ = tokio::time::sleep(IDLE_POLL), if keepalive_enabled => {}
            }
        };
        let mut pending = vec![first];

        if effective_depth > 1 {
            while pending.len() < effective_depth {
                match rx.try_recv() {
                    Ok(t) => pending.push(t),
                    Err(_) => break,
                }
            }
        }

        for p in &pending {
            shared.emit(ProgressEvent::ConnectionBusy {
                conn: conn_id,
                file: p.task.meta.real_name.clone(),
            });
        }

        if pending.is_empty() {
            continue;
        }

        if shared.config.dry_run {
            for p in pending {
                shared.results.lock().unwrap().push(PostedSegment {
                    file_name: p.task.meta.real_name.clone(),
                    file_path: Arc::from(p.task.meta.path.as_path()),
                    // NZB uses the real filename, not wire subject (may be obfuscated).
                    subject_name: Arc::from(p.task.meta.client_path.as_str()),
                    wire_name: Arc::from(p.task.subject_name.as_str()),
                    wire_yenc_name: Arc::from(p.task.yenc_name.as_str()),
                    file_size: p.task.meta.size,
                    part: p.task.part,
                    total: p.task.total,
                    message_id: p.message_id,
                    bytes: (p.headers.len() + p.encoded.body.len()) as u64,
                    from: Arc::from(p.task.from.as_str()),
                    date: p.date.clone(),
                    full_crc32: p.task.file_crc32.unwrap_or(0),
                    // Nothing was actually posted in dry-run mode, so there's
                    // no real server and no check queue — see the field doc.
                    server_idx: 0,
                    file_index: p.task.meta.file_index,
                    total_files: shared.total_files,
                });
                let bytes = p.task.data.len() as u64;
                shared.release_buffer(p.task.data);
                shared.emit(ProgressEvent::SegmentDone {
                    file: p.task.meta.real_name.clone(),
                    bytes,
                    ok: true,
                });
            }
            continue;
        }

        // Rate-limit on total bytes for the whole batch.
        let total_bytes: usize = pending
            .iter()
            .map(|p| p.headers.len() + p.encoded.body.len())
            .sum();
        rate_limiter.acquire(total_bytes).await;

        let max_attempts = shared.config.retries;

        if pending.len() == 1 {
            // ── Sequential path (depth 1 or only one task left) ──────────────
            let mut p = pending.remove(0);
            let mut posted = false;
            let mut last_err = String::from("unknown error");
            let mut transient_attempts = 0u64;

            for attempt in 1..=max_attempts {
                let conn = match slot.ensure_connected().await {
                    Ok(c) => c,
                    Err(e) => {
                        last_err = format!("{e:#}");
                        warn!(segment = %p.message_id, attempt, max_attempts,
                              error = %last_err, "connection failed; will retry");
                        shared.total_retries.fetch_add(1, Ordering::Relaxed);
                        transient_attempts += 1;
                        if attempt < max_attempts {
                            shared.emit(ProgressEvent::ConnectionRetrying { conn: conn_id });
                            tokio::time::sleep(slot.retry_delay()).await;
                        }
                        continue;
                    }
                };
                let t_post = Instant::now();
                match conn.post_parts(&p.headers, &p.encoded.body).await {
                    Ok(returned_id) => {
                        // Some servers substitute their own Message-ID at
                        // accept time and echo it back in the 240 response
                        // instead of the one we sent — nyuu has handled this
                        // since 2016. Tracking our own ID after that would
                        // mean STAT (and the .nzb) reference an ID the
                        // server never actually stored anything under.
                        if let Some(server_id) = returned_id {
                            if server_id != p.message_id {
                                warn!(
                                    sent = %p.message_id,
                                    returned = %server_id,
                                    "server returned a different Message-ID than sent; adopting it"
                                );
                                p.message_id = server_id;
                            }
                        }
                        // Adaptive warm-up: compute pipeline depth from the
                        // ratio of post time (send + RTT) to encode time.
                        if is_adaptive && !warmup_done {
                            let post_us = t_post.elapsed().as_micros().max(1);
                            let enc_us = p.encode_time.as_micros().max(1);
                            let ratio = post_us.saturating_div(enc_us);
                            let depth = (ratio as usize).clamp(1, MAX_AUTO_PIPELINE_DEPTH);
                            effective_depth = depth;
                            warmup_done = true;
                            info!(
                                conn = conn_id,
                                depth,
                                post_ms = t_post.elapsed().as_millis(),
                                encode_us = enc_us,
                                "adaptive pipeline depth computed"
                            );
                        }
                        debug!(segment = %p.message_id, "posted");
                        posted = true;
                        break;
                    }
                    Err(e) => {
                        last_err = format!("{e:#}");
                        warn!(segment = %p.message_id, attempt, max_attempts,
                              error = %last_err, "post failed; rotating server");
                        shared.total_retries.fetch_add(1, Ordering::Relaxed);
                        transient_attempts += 1;
                        if attempt < max_attempts {
                            shared.emit(ProgressEvent::ConnectionRetrying { conn: conn_id });
                        }
                        slot.invalidate("post_err");
                    }
                }
                if attempt < max_attempts {
                    tokio::time::sleep(slot.retry_delay()).await;
                }
            }

            if posted && transient_attempts > 0 {
                shared.emit(ProgressEvent::PostRetryRecovered {
                    count: 1,
                    previously_failed: false,
                });
                shared.emit(ProgressEvent::ConnectionBusy {
                    conn: conn_id,
                    file: p.task.meta.real_name.clone(),
                });
            }
            let wire = p.headers.len() + p.encoded.body.len();
            commit_result(
                &shared,
                p.task,
                p.message_id,
                wire,
                posted,
                &last_err,
                p.date,
                slot.server_idx(),
            );
            shared.release_encode_buf(p.encoded.body);
        } else {
            // ── Pipelined path ───────────────────────────────────────────────
            // Send all articles back-to-back, flush once, then read all
            // responses. On any connection error the entire batch is retried.
            //
            // All conn usage is confined to the labeled block `'use_conn` so
            // that `slot.invalidate()` can be called after the block ends,
            // satisfying the borrow checker (conn borrows slot mutably).
            let n = pending.len();
            let mut pipeline_ok = false;
            let mut pipeline_retried = false;
            let mut pipe_results: Vec<Result<(), String>> = (0..n).map(|_| Ok(())).collect();

            'pipeline: for attempt in 1..=max_attempts {
                // `(needs_invalidate, error_message)` — conn is dropped when
                // the labeled block expression completes.
                let (needs_invalidate, pipe_err) = 'use_conn: {
                    let conn = match slot.ensure_connected().await {
                        Ok(c) => c,
                        Err(e) => {
                            warn!(attempt, max_attempts, error = %e,
                                  "connection failed during pipeline; will retry");
                            shared.total_retries.fetch_add(1, Ordering::Relaxed);
                            pipeline_retried = true;
                            if attempt < max_attempts {
                                shared.emit(ProgressEvent::ConnectionRetrying { conn: conn_id });
                                tokio::time::sleep(slot.retry_delay()).await;
                            }
                            continue 'pipeline;
                        }
                    };

                    // Enqueue all articles without flushing.
                    for p in &pending {
                        if let Err(e) = conn.enqueue_post(&p.headers, &p.encoded.body).await {
                            break 'use_conn (true, format!("{e:#}"));
                        }
                    }

                    // One flush covers all enqueued articles.
                    if let Err(e) = conn.flush_pipeline().await {
                        break 'use_conn (true, format!("{e:#}"));
                    }

                    // Read one (340, 240) pair per article. On error: record the
                    // failure index, break out of the for loop (dropping the
                    // iter_mut borrow), then mark remaining entries as failed.
                    let mut fail_at: Option<(usize, String)> = None;
                    for (i, result) in pipe_results.iter_mut().enumerate() {
                        match conn.read_post_response().await {
                            Ok(returned_id) => {
                                // See the sequential path above for why: some
                                // servers substitute their own Message-ID at
                                // accept time.
                                if let Some(server_id) = returned_id {
                                    if server_id != pending[i].message_id {
                                        warn!(
                                            sent = %pending[i].message_id,
                                            returned = %server_id,
                                            "server returned a different Message-ID than sent; adopting it"
                                        );
                                        pending[i].message_id = server_id;
                                    }
                                }
                                debug!(segment = %pending[i].message_id, "posted (pipelined)");
                                *result = Ok(());
                            }
                            Err(e) => {
                                *result = Err(format!("{e:#}"));
                                fail_at = Some((i + 1, format!("{e:#}")));
                                break;
                            }
                        }
                    }
                    // iter_mut borrow is dropped here; safe to index pipe_results.
                    if let Some((from, msg)) = fail_at {
                        for r in pipe_results[from..].iter_mut() {
                            // Remaining articles in the batch never received a
                            // response — the connection was lost after the first
                            // rejection. Use a distinct message so the log does
                            // not falsely repeat the first article's message-id.
                            *r = Err("pipeline interrupted after previous failure".into());
                        }
                        break 'use_conn (true, msg);
                    }

                    (false, String::new())
                }; // conn dropped; slot methods are safe to call again.

                if needs_invalidate {
                    warn!(attempt, max_attempts, error = %pipe_err,
                          "pipeline failed; rotating server");
                    shared.total_retries.fetch_add(1, Ordering::Relaxed);
                    pipeline_retried = true;
                    if attempt < max_attempts {
                        shared.emit(ProgressEvent::ConnectionRetrying { conn: conn_id });
                    }
                    slot.invalidate("post_err");
                    if attempt < max_attempts {
                        tokio::time::sleep(slot.retry_delay()).await;
                    }
                    continue;
                }

                pipeline_ok = true;
                break;
            }

            if pipeline_ok && pipeline_retried {
                shared.emit(ProgressEvent::PostRetryRecovered {
                    count: n as u64,
                    previously_failed: false,
                });
                if let Some(article) = pending.first() {
                    shared.emit(ProgressEvent::ConnectionBusy {
                        conn: conn_id,
                        file: article.task.meta.real_name.clone(),
                    });
                }
            }

            // The whole batch shares one connection/flush, so every article in
            // it — success or failure — was attempted against the same server.
            let batch_server_idx = slot.server_idx();
            for (p, result) in pending.into_iter().zip(pipe_results) {
                let posted = pipeline_ok && result.is_ok();
                let last_err = result.err().unwrap_or_else(|| "pipeline failed".into());
                let wire = p.headers.len() + p.encoded.body.len();
                commit_result(
                    &shared,
                    p.task,
                    p.message_id,
                    wire,
                    posted,
                    &last_err,
                    p.date,
                    batch_server_idx,
                );
                shared.release_encode_buf(p.encoded.body);
            }
        }
    }

    shared.emit(ProgressEvent::ConnectionIdle { conn: conn_id });
    slot
}

/// Build a `PostTask`, generating per-article identities for the two
/// article-level modes; otherwise copies them from `FileMeta`.
fn make_task(
    meta: Arc<FileMeta>,
    part: u32,
    total: u32,
    offset: u64,
    data: Vec<u8>,
    file_crc32: Option<u32>,
    config: &Config,
) -> PostTask {
    let (subject_name, yenc_name, from, date) = match config.obfuscate {
        ObfuscateMode::Full => (
            obfuscated_name(),
            meta.yenc_name.clone(),
            random_from(),
            meta.date.clone(),
        ),
        ObfuscateMode::Article => (
            obfuscated_name(),
            obfuscated_yenc_name(&meta.real_name),
            random_from(),
            meta.date.clone(),
        ),
        _ => {
            let date = if config.date.as_deref() == Some("now") {
                resolve_date(Some("now"))
            } else {
                meta.date.clone()
            };
            (
                meta.subject_name.clone(),
                meta.yenc_name.clone(),
                meta.from.clone(),
                date,
            )
        }
    };
    PostTask {
        meta,
        part,
        total,
        offset,
        data,
        subject_name,
        yenc_name,
        from,
        date,
        file_crc32,
    }
}

/// Persist a successfully posted segment or record a failure, then emit the
/// corresponding progress event and release the article buffer back to the pool.
#[allow(clippy::too_many_arguments)]
fn commit_result(
    shared: &Shared,
    task: PostTask,
    message_id: String,
    wire_bytes: usize,
    posted: bool,
    last_err: &str,
    date: (Option<String>, Option<u64>),
    server_idx: usize,
) {
    if posted {
        if let Some(resume) = &shared.resume {
            // In-memory only — no disk write here. Every commit used to
            // rewrite the entire state file while holding this lock, which
            // serialized all workers through one lock and turned state
            // tracking into an O(n^2) hot-path cost on large uploads. Now
            // that resume state is tracked unconditionally (not just when
            // --resume is passed), persisting had to move off this path
            // regardless — the whole point of resume is to survive the *end*
            // of a run being incomplete, not every individual segment, so a
            // single persist decided by the final outcome (see the
            // still_missing handling and `run_single_upload`'s cleanup)
            // covers the same guarantee at a fraction of the cost.
            //
            // `confirmed` is false until STAT 223; `--no-check` never flips
            // it (that run never STATed).
            resume.lock().unwrap().record_with(
                &task.meta.real_name,
                task.part,
                SegmentRecord {
                    message_id: message_id.clone(),
                    bytes: wire_bytes as u64,
                    confirmed: false,
                    check_disabled: !shared.config.check,
                    server_idx,
                    wire_identity: Some(persisted_identity(
                        &task.subject_name,
                        &task.yenc_name,
                        &task.from,
                        &date,
                    )),
                },
            );
        }
        // Confirmed posted — any spooled copy has served its purpose.
        if let Some(dir) = &shared.spool_dir {
            crate::spool::remove(dir, &task.meta.real_name, task.part);
        }
        let seg = PostedSegment {
            file_name: task.meta.real_name.clone(),
            file_path: Arc::from(task.meta.path.as_path()),
            // NZB uses the real filename for proper client-side renaming.
            subject_name: Arc::from(task.meta.client_path.as_str()),
            wire_name: Arc::from(task.subject_name.as_str()),
            wire_yenc_name: Arc::from(task.yenc_name.as_str()),
            file_size: task.meta.size,
            part: task.part,
            total: task.total,
            message_id,
            bytes: wire_bytes as u64,
            from: Arc::from(task.from.as_str()),
            date,
            full_crc32: task.file_crc32.unwrap_or(0),
            server_idx,
            file_index: task.meta.file_index,
            total_files: shared.total_files,
        };
        shared.results.lock().unwrap().push(seg.clone());
        if let Some(tx) = shared.check_tx.lock().unwrap().as_ref() {
            let _ = tx.send(seg);
        }
    } else {
        record_failure(shared, &task.meta, &task, message_id, last_err);
    }
    let article_bytes = task.data.len() as u64;
    shared.release_buffer(task.data);
    shared.emit(ProgressEvent::SegmentDone {
        file: task.meta.real_name.clone(),
        bytes: article_bytes,
        ok: posted,
    });
}

/// Add ±50 % jitter to `base` to prevent synchronized reconnect bursts.
///
/// Uses `slot_id` mixed with the current nanosecond timestamp as a cheap
/// pseudo-random seed — no external crate required.
fn jittered(base: Duration, slot_id: usize) -> Duration {
    let ns = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos() as u64;
    // 0..=999 range → [1.0, 1.5) multiplier
    let noise = (ns.wrapping_add(slot_id as u64 * 2_654_435_761) % 1000) as u32;
    let extra_ms = (base.as_millis() as u64 * noise as u64 / 2000) as u32;
    base + Duration::from_millis(extra_ms as u64)
}

/// Whether an automatic final recovery pass (see `check::recover_missing`)
/// is worth attempting for `missing` still-unconfirmed articles out of
/// `total` posted this run. Gated by *both* an absolute cap
/// (`check_recover_max`) and a percentage of the release
/// (`check_recover_percent`) — whichever is smaller wins, so behaviour
/// scales sanely from a small release (where even a large fraction missing
/// is only a handful of articles) to a huge one (where 15% could still be
/// thousands of articles, no longer "cheap" to retry automatically).
fn is_cheap_to_recover(missing: usize, total: usize, config: &Config) -> bool {
    if missing == 0 || config.check_recover_max == 0 {
        return false;
    }
    if missing > config.check_recover_max {
        return false;
    }
    let percent_cap = (total as f64 * config.check_recover_percent as f64 / 100.0).ceil() as usize;
    missing <= percent_cap.max(1)
}

/// Build the "→ ..." label shown in the live panel header and the `Started`
/// progress event's `target` field. Lists every configured server (not just
/// the primary) so a multi-server run doesn't look single-server for its
/// entire duration — see the call site's comment for why this is knowable
/// up front, unlike `groups`.
fn target_label(servers: &[crate::config::ServerEntry], total_connections: usize) -> String {
    match servers {
        [] => String::new(),
        [only] => format!("{}:{}", only.host, only.port),
        _ if servers.len() <= 3 => servers
            .iter()
            .map(|s| s.host.as_str())
            .collect::<Vec<_>>()
            .join(" + "),
        _ => format!("{} servers ({total_connections} conn)", servers.len()),
    }
}

fn record_failure(
    shared: &Shared,
    meta: &FileMeta,
    task: &PostTask,
    message_id: String,
    error: &str,
) {
    let description = format!(
        "{} part {}/{}: {error}",
        meta.real_name, task.part, task.total
    );
    shared.emit(ProgressEvent::PostRetryQueued);
    shared.emit(ProgressEvent::Failed {
        description: description.clone(),
    });
    shared.failures.lock().unwrap().push(description);
    shared.failed_tasks.lock().unwrap().push(FailedTask {
        file_name: meta.real_name.clone(),
        client_path: meta.client_path.clone(),
        file_path: meta.path.clone(),
        message_id,
        subject_name: task.subject_name.clone(),
        yenc_name: task.yenc_name.clone(),
        file_size: meta.size,
        part: task.part,
        total: task.total,
        from: task.from.clone(),
        date: task.date.clone(),
        full_crc32: task.file_crc32.unwrap_or(0),
        file_index: meta.file_index,
        total_files: shared.total_files,
    });
}

/// Post a fresh copy of each segment in `failed`, re-posting under the
/// *same* `Message-ID` the in-run attempt used (see the comment on
/// `message_id` below for why). Returns the `PostedSegment`s that were
/// successfully posted; tasks that exhaust all retries are silently dropped
/// (the caller can compare lengths to detect persistent failures).
pub async fn repost_failed_tasks(
    config: &Config,
    failed: &[FailedTask],
    groups: &[String],
    events: Option<&ProgressSender>,
    cancel: Option<&Arc<AtomicBool>>,
    slots: &mut [ConnectionSlot],
) -> Result<Vec<PostedSegment>> {
    if failed.is_empty() {
        return Ok(Vec::new());
    }

    // Never `ConnectionSlot::new` — extra TCP would exceed a budget already
    // held by this episode. No slot means nothing to retry on.
    let Some(slot) = slots.first_mut() else {
        return Ok(Vec::new());
    };

    let article_size = config.article_size as u64;
    let max_retries = config.retries.max(1);
    let mut recovered: Vec<PostedSegment> = Vec::new();

    for (i, task) in failed.iter().enumerate() {
        if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
            break;
        }
        let offset = (task.part as u64 - 1) * article_size;
        let read_len = (task.file_size - offset).min(article_size) as usize;

        // Re-read from the preserved absolute path, not `file_name` (which is
        // only the published/relative name and would resolve against the CWD).
        let path = task.file_path.clone();
        let mut file = match File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                warn!(file = %task.file_name, path = %path.display(), "retry: cannot open file: {e}");
                continue;
            }
        };

        use tokio::io::AsyncSeekExt;
        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
            warn!(file = %task.file_name, offset, "retry: seek failed: {e}");
            continue;
        }

        let mut buf = vec![0u8; read_len];
        if let Err(e) = file.read_exact(&mut buf).await {
            warn!(file = %task.file_name, "retry: read failed: {e}");
            continue;
        }

        let spec = yenc::PartSpec {
            number: task.part,
            total: task.total,
            offset,
        };
        let file_crc32 = (task.part == task.total).then_some(task.full_crc32);
        let encoded = yenc::encode_part(
            &task.yenc_name,
            task.file_size,
            spec,
            &buf,
            config.line_length,
            file_crc32,
        );
        // Re-post with the *same* Message-ID the in-run attempts used, so a
        // server that already has the article (lost `240` ack) deduplicates it
        // via `435 Already exists` instead of accepting a duplicate under a
        // fresh ID. See [`FailedTask::message_id`].
        let mut message_id = task.message_id.clone();
        let (rfc_date, _ts) = &task.date;
        let article = Article {
            message_id: message_id.clone(),
            from: task.from.clone(),
            newsgroups: groups.to_vec(),
            subject: default_subject(
                &task.subject_name,
                task.part,
                task.total,
                (task.total_files > 0).then_some((task.file_index, task.total_files)),
            ),
            date: rfc_date.clone(),
            no_archive: config.no_archive,
        };
        let headers = article.build_headers();
        let wire_bytes = (headers.len() + encoded.body.len()) as u64;

        let mut ok = false;
        for attempt in 1..=max_retries {
            match slot.ensure_connected().await {
                Ok(conn) => match conn.post_parts(&headers, &encoded.body).await {
                    Ok(returned_id) => {
                        // See the main post path for why: some servers
                        // substitute their own Message-ID at accept time.
                        if let Some(server_id) = returned_id {
                            if server_id != message_id {
                                warn!(
                                    sent = %message_id,
                                    returned = %server_id,
                                    "server returned a different Message-ID than sent; adopting it"
                                );
                                message_id = server_id;
                            }
                        }
                        ok = true;
                        break;
                    }
                    Err(e) => {
                        slot.invalidate("post_err");
                        warn!(file = %task.file_name, part = task.part, attempt, "retry attempt failed: {e}");
                        if attempt < max_retries {
                            if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
                                break;
                            }
                            tokio::time::sleep(Duration::from_secs(config.retry_delay)).await;
                        }
                    }
                },
                Err(e) => {
                    warn!(attempt, "retry: connect failed: {e}");
                    if attempt < max_retries {
                        if cancel.is_some_and(|f| f.load(Ordering::Relaxed)) {
                            break;
                        }
                        tokio::time::sleep(Duration::from_secs(config.retry_delay)).await;
                    }
                }
            }
        }

        if ok {
            recovered.push(PostedSegment {
                file_name: task.file_name.clone(),
                file_path: Arc::from(task.file_path.as_path()),
                // NZB uses the real filename, not obfuscated wire subject.
                subject_name: Arc::from(task.client_path.as_str()),
                wire_name: Arc::from(task.subject_name.as_str()),
                wire_yenc_name: Arc::from(task.yenc_name.as_str()),
                file_size: task.file_size,
                part: task.part,
                total: task.total,
                message_id,
                bytes: wire_bytes,
                server_idx: slot.server_idx(),
                from: Arc::from(task.from.as_str()),
                date: task.date.clone(),
                full_crc32: task.full_crc32,
                file_index: task.file_index,
                total_files: task.total_files,
            });
            if let Some(tx) = events {
                let _ = tx.send(ProgressEvent::PostRetryRecovered {
                    count: 1,
                    previously_failed: true,
                });
                let _ = tx.send(ProgressEvent::Status {
                    text: format!("retry: {}/{} segment(s) recovered", recovered.len(), i + 1),
                });
            }
        } else {
            warn!(
                file = %task.file_name,
                part = task.part,
                "retry: gave up after all attempts"
            );
        }
    }

    if let Some(tx) = events {
        let _ = tx.send(ProgressEvent::Status {
            text: String::new(),
        });
    }

    Ok(recovered)
}

#[cfg(test)]
mod tests;
