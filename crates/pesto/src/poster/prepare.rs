//! Run preparation: resume/spool state, posting inputs and run resources.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use tracing::info;

use crate::article::{obfuscated_name, obfuscated_name_with_prefix, random_from};
use crate::config::{Config, ObfuscateMode};
use crate::resume::ResumeState;
use crate::walk::{natural_cmp, InputFile};
use crate::yenc;
use parmesan::layout;
use parmesan::packet;

use super::connections::split_connections;
use super::file_md5_16k;
use super::identity::{normalize_client_path, obfuscated_yenc_name, resolve_date};
use super::par2::par2_geometry;
use super::FileMeta;

/// Validate any loaded resume state, prepare the spool directory and generate
/// the once-per-run shared release identity used by `light`/`full-shared`
/// obfuscation. Returns `(resume, resume_path, spool_dir, release_prefix,
/// release_from)`.
#[allow(clippy::type_complexity)]
pub(super) fn prepare_resume(
    config: &Config,
    resume_state_path: Option<&Path>,
    release_prefix_override: Option<&str>,
) -> Result<(
    Option<Arc<Mutex<ResumeState>>>,
    Option<std::path::PathBuf>,
    Option<std::path::PathBuf>,
    Option<String>,
    Option<String>,
)> {
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

    Ok((
        resume_arc,
        resume_path_owned,
        spool_dir_owned,
        release_prefix,
        release_from,
    ))
}

/// Build the per-file metadata in posting order: read sizes, fingerprint each
/// file against resume state, normalize published names, assign wire
/// identities, sort a multi-file PAR2 set by File ID and number the release
/// for `--file-counter`. Returns the ordered `metas` and the planned segment
/// total.
pub(super) async fn prepare_inputs(
    config: &Config,
    files: &[InputFile],
    resume_arc: Option<Arc<Mutex<ResumeState>>>,
    release_prefix: Option<String>,
    release_from: Option<String>,
) -> Result<(Vec<Arc<FileMeta>>, u64)> {
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

    Ok((metas, initial_segments))
}

/// Connection, buffer-pool and PAR2 geometry resources prepared before the
/// pipeline starts.
pub(super) struct RunResources {
    pub(super) servers: Arc<Vec<crate::config::ServerEntry>>,
    pub(super) proxy_status: Option<String>,
    pub(super) total_conns: usize,
    pub(super) check_conns: usize,
    pub(super) upload_conns: usize,
    pub(super) worker_count: usize,
    pub(super) run_id: u64,
    pub(super) par2_slice_size: usize,
    pub(super) recovery_count: usize,
    pub(super) total_files: u32,
    pub(super) initial_pool: Vec<Vec<u8>>,
}

/// Validate the proxy before any worker exists, split the connection budget,
/// size the worker pool and pre-fill the reusable article-buffer pool.
pub(super) async fn prepare_resources(
    config: &Config,
    metas: &[Arc<FileMeta>],
    initial_segments: u64,
) -> Result<RunResources> {
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
    let (par2_slice_size, _total_slices, recovery_count) = par2_geometry(metas, config);

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

    Ok(RunResources {
        servers,
        proxy_status,
        total_conns,
        check_conns,
        upload_conns,
        worker_count,
        run_id,
        par2_slice_size,
        recovery_count,
        total_files,
        initial_pool,
    })
}
