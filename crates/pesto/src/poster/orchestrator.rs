//! Run lifecycle: validation, pipeline startup, check/repost and outcome.

use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use tracing::{error, info, warn};

use crate::article::{obfuscated_name, obfuscated_name_with_prefix, random_from};
use crate::config::{Config, ObfuscateMode};
use crate::nntp::pool::ConnectionBroker;
use crate::progress::{FileEntry, ProgressEvent, ProgressSender, RunMode};
use crate::resume::{ResumeState, SegmentRecord};
use crate::walk::{natural_cmp, InputFile};
use crate::yenc;
use parmesan::layout;
use parmesan::packet;

use super::check::{self, spawn_check_coordinator};
use super::connections::{release_slots, split_connections, take_slots};
use super::options::RunOptions;
use super::outcome::{PostOutcome, PostedSegment};
use super::par2::par2_geometry;
use super::producer::producer;
use super::result::{is_cheap_to_recover, repost_failed_tasks, target_label};
use super::shared::Shared;
use super::task::TaskDispatcher;
use super::worker::{encode_worker, worker};
use super::{
    configure_rayon, encode_concurrency, file_md5_16k, normalize_client_path, obfuscated_yenc_name,
    par2_temp_dir, persisted_identity, pick_post_group, post_pregenerated_release,
    ready_queue_depth, resolve_date, FileMeta,
};
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
    run(RunOptions {
        config,
        files,
        events,
        resume_state_path,
        external_cancel,
        entry_label,
        broker,
        external_pause,
        release_prefix_override,
    })
    .await
}

/// Internal run entry point; the public facades only assemble [`RunOptions`].
async fn run(options: RunOptions<'_>) -> Result<PostOutcome> {
    let RunOptions {
        config,
        files,
        events,
        resume_state_path,
        external_cancel,
        entry_label,
        broker,
        external_pause,
        release_prefix_override,
    } = options;

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
