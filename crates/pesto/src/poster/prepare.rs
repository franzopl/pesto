//! Run preparation: resume/spool state, posting inputs and run resources.

use std::path::Path;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
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
use super::identity::{
    normalize_client_path, obfuscated_yenc_name, par2_release_base, resolve_date,
};
use super::outcome::SegmentIdentity;
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
    spool_dir: Option<&Path>,
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
                if state.session_identity().is_some() {
                    eprintln!(
                        "resume: `{real_name}` changed size or modification time since the \
                         saved session state was recorded — invalidating entire session"
                    );
                    state.invalidate_session();
                    if let Some(dir) = spool_dir {
                        crate::spool::remove_all(dir);
                    }
                } else if config.par2 > 0 {
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
                    if let Some(dir) = spool_dir {
                        crate::spool::remove_all(dir);
                    }
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
            mtime: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs()),
            // Assigned below, once `metas`' final posting order is settled —
            // see the natural order pass after the File-ID sort.
            release_ordinal: 0,
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

    // Assign `release_ordinal` based on the release's natural filename order
    // (`part1.rar` is ordinal 1, `part2.rar` is ordinal 2). This order is
    // independent of `metas`' processing order (File-ID sort for PAR2) and
    // serves as the immutable release ordinal for segment identity.
    //
    // When `--file-counter` is enabled, `file_index` reflects this ordinal for
    // the `[filenum/total]` subject prefix. When disabled, `file_index` remains
    // 0 while `release_ordinal` is always assigned.
    let mut order: Vec<usize> = (0..metas.len()).collect();
    order.sort_by(|&a, &b| natural_cmp(&metas[a].real_name, &metas[b].real_name));
    let mut rank = vec![0u32; metas.len()];
    for (pos, &idx) in order.iter().enumerate() {
        rank[idx] = pos as u32 + 1;
    }
    metas = metas
        .into_iter()
        .zip(rank)
        .map(|(m, release_ordinal)| {
            let file_index = if config.file_counter {
                release_ordinal
            } else {
                0
            };
            Arc::new(FileMeta {
                release_ordinal,
                file_index,
                ..(*m).clone()
            })
        })
        .collect();

    let mut initial_segments = 0;
    for meta in &metas {
        initial_segments += yenc::segments(meta.size, config.article_size).len() as u64;
    }

    Ok((metas, initial_segments))
}

/// An entry in a release layout representing one file's ordinal, part count,
/// and prefix part sum (total segments preceding this file in release order).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutEntry {
    pub release_ordinal: u32,
    pub part_count: u32,
    pub prefix_parts: u64,
    #[serde(default)]
    pub file_name: Option<String>,
    #[serde(default)]
    pub fingerprint: Option<crate::resume::FileFingerprint>,
}

/// An immutable, complete release layout computed before concurrent task dispatch.
///
/// Maps every 1-based release ordinal `1..=total_files` to its part count and
/// preceding segment prefix sum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleaseLayout {
    pub(crate) total_files: u32,
    pub(crate) total_segments: u64,
    pub(crate) entries: Vec<LayoutEntry>,
}

impl ReleaseLayout {
    /// Construct a ReleaseLayout from a slice of `(release_ordinal, part_count)`.
    ///
    /// Validates that:
    /// - `total_files > 0`
    /// - Every ordinal from 1 to `total_files` is present exactly once
    /// - Every file has `part_count >= 1`
    /// - Cumulative segments and individual indices do not overflow `u32::MAX`
    pub fn from_parts(total_files: u32, parts: &[(u32, u32)]) -> Result<Self> {
        let detailed: Vec<(
            u32,
            u32,
            Option<String>,
            Option<crate::resume::FileFingerprint>,
        )> = parts
            .iter()
            .map(|&(ord, count)| (ord, count, None, None))
            .collect();
        Self::from_detailed_parts(total_files, &detailed)
    }

    /// Construct a ReleaseLayout from a slice with file name and fingerprint details.
    pub fn from_detailed_parts(
        total_files: u32,
        parts: &[(
            u32,
            u32,
            Option<String>,
            Option<crate::resume::FileFingerprint>,
        )],
    ) -> Result<Self> {
        if total_files == 0 {
            bail!("release layout cannot have 0 total files");
        }
        if parts.len() != total_files as usize {
            bail!(
                "release layout parts count ({}) does not match total files ({})",
                parts.len(),
                total_files
            );
        }

        let mut sorted = parts.to_vec();
        sorted.sort_by_key(|&(ord, _, _, _)| ord);

        let mut entries = Vec::with_capacity(sorted.len());
        let mut prefix_parts = 0u64;

        for (expected_idx, &(ord, count, ref name, ref fp)) in sorted.iter().enumerate() {
            let expected_ord = expected_idx as u32 + 1;
            if ord != expected_ord {
                bail!("release layout missing or non-contiguous ordinal: expected {expected_ord}, got {ord}");
            }
            if count == 0 {
                bail!("release layout file {ord} has zero parts");
            }
            entries.push(LayoutEntry {
                release_ordinal: ord,
                part_count: count,
                prefix_parts,
                file_name: name.clone(),
                fingerprint: *fp,
            });
            prefix_parts = prefix_parts
                .checked_add(u64::from(count))
                .context("release layout cumulative segment count overflow")?;
            if prefix_parts > u64::from(u32::MAX) {
                bail!("release layout total segments exceeds u32::MAX: {prefix_parts}");
            }
        }

        Ok(ReleaseLayout {
            total_files,
            total_segments: prefix_parts,
            entries,
        })
    }

    /// Total files in the release.
    pub fn total_files(&self) -> u32 {
        self.total_files
    }

    /// Total segments in the release.
    pub fn total_segments(&self) -> u64 {
        self.total_segments
    }

    /// Entries in the release layout.
    pub fn entries(&self) -> &[LayoutEntry] {
        &self.entries
    }

    /// Layout fingerprint computed deterministically from layout fields.
    pub fn fingerprint(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut hasher = Sha256::new();
        hasher.update(self.total_files.to_le_bytes());
        hasher.update(self.total_segments.to_le_bytes());
        for entry in &self.entries {
            hasher.update(entry.release_ordinal.to_le_bytes());
            hasher.update(entry.part_count.to_le_bytes());
            hasher.update(entry.prefix_parts.to_le_bytes());
        }
        let digest = hasher.finalize();
        digest.iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Get the layout entry for a given 1-based release ordinal.
    pub fn entry(&self, release_ordinal: u32) -> Option<&LayoutEntry> {
        if release_ordinal == 0 || release_ordinal > self.total_files {
            None
        } else {
            self.entries.get((release_ordinal - 1) as usize)
        }
    }

    /// Compute the SegmentIdentity for a given file ordinal and part number.
    pub fn segment_identity(
        &self,
        release_ordinal: u32,
        part_number: u32,
    ) -> Option<SegmentIdentity> {
        let entry = self.entry(release_ordinal)?;
        if part_number == 0 || part_number > entry.part_count {
            return None;
        }
        SegmentIdentity::checked(
            entry.prefix_parts,
            release_ordinal,
            self.total_files,
            part_number,
        )
    }

    /// Build the release layout from prepared input files and PAR2 configuration.
    pub(super) fn build(
        metas: &[Arc<FileMeta>],
        config: &Config,
        recovery_count: usize,
        par2_slice_size: usize,
    ) -> Result<Self> {
        let article_size = config.article_size;
        let mut parts_list = Vec::with_capacity(metas.len() + 16);

        for meta in metas {
            let segs = yenc::segments(meta.size, article_size);
            let part_count =
                u32::try_from(segs.len()).context("data file part count exceeds u32")?;
            let fp = crate::resume::FileFingerprint {
                size: meta.size,
                mtime: meta.mtime,
            };
            parts_list.push((
                meta.release_ordinal,
                part_count,
                Some(meta.real_name.clone()),
                Some(fp),
            ));
        }

        let par2_file_count = if recovery_count > 0 {
            let publish_index = config.obfuscate.policy().publish_par2_index;
            let base_len = par2_base_packets_len(metas, par2_slice_size);
            let base_name = metas.first().map(|m| par2_release_base(&m.real_name));

            if publish_index {
                let index_ordinal = metas.len() as u32 + 1;
                let index_parts = u32::try_from(yenc::segments(base_len, article_size).len())
                    .context("PAR2 index part count exceeds u32")?;
                let index_name = base_name.map(layout::index_name);
                parts_list.push((index_ordinal, index_parts, index_name, None));
            }

            let volumes = layout::plan_volumes(recovery_count as u32);
            let index_offset = u32::from(publish_index);
            for (vol_idx, vol) in volumes.iter().enumerate() {
                let vol_ordinal = metas.len() as u32 + 1 + index_offset + vol_idx as u32;
                let vol_len = par2_volume_len(base_len, vol.count, par2_slice_size);
                let vol_parts = u32::try_from(yenc::segments(vol_len, article_size).len())
                    .context("PAR2 volume part count exceeds u32")?;
                let vol_name = base_name.map(|base| layout::volume_name(base, *vol));
                parts_list.push((vol_ordinal, vol_parts, vol_name, None));
            }

            usize::from(publish_index) + volumes.len()
        } else {
            0
        };

        let total_files = u32::try_from(metas.len() + par2_file_count)
            .context("total release files exceed u32")?;

        Self::from_detailed_parts(total_files, &parts_list)
    }
}

/// Compute the exact byte length of PAR2 base packets (Main + Creator + FileDesc + IFSC)
/// without performing any file reads or hashes.
pub(crate) fn par2_base_packets_len(metas: &[Arc<FileMeta>], par2_slice_size: usize) -> u64 {
    let mut total: u64 = 64 + 12 + 16 * metas.len() as u64; // Main packet
    total += 72; // Creator packet ("pesto")
    let s = par2_slice_size.max(1);
    for meta in metas {
        let path_len = meta.client_path.len();
        let padded_path = (path_len + 3) & !3;
        total += 64 + 56 + padded_path as u64; // File Description

        let slice_count = if meta.size == 0 {
            0
        } else {
            (meta.size as usize).div_ceil(s)
        };
        total += 64 + 16 + 20 * slice_count as u64; // IFSC
    }
    total
}

/// Compute the exact byte length of a PAR2 recovery volume.
pub(crate) fn par2_volume_len(
    base_packets_len: u64,
    recovery_slice_count: u32,
    par2_slice_size: usize,
) -> u64 {
    base_packets_len + (recovery_slice_count as u64) * (68 + par2_slice_size as u64)
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
    pub(super) release_layout: Arc<ReleaseLayout>,
    pub(super) encryption_adapter: Option<Arc<crate::crypto::UploadEncryptionAdapter>>,
    pub(super) initial_pool: Vec<Vec<u8>>,
}

/// Validate the proxy before any worker exists, split the connection budget,
/// size the worker pool and pre-fill the reusable article-buffer pool.
pub(super) async fn prepare_resources(
    config: &Config,
    metas: &[Arc<FileMeta>],
    initial_segments: u64,
    resume_arc: Option<&Arc<Mutex<ResumeState>>>,
    spool_dir: Option<&Path>,
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

    let release_layout = Arc::new(ReleaseLayout::build(
        metas,
        config,
        recovery_count,
        par2_slice_size,
    )?);

    if let Some(resume) = resume_arc {
        let mut state = resume.lock().unwrap();
        let was_invalidated = state.take_session_invalidated();
        let valid = state.validate_session(&release_layout);
        if was_invalidated || !valid {
            if !valid {
                eprintln!(
                    "resume: release layout changed since the saved state was recorded \
                     — clearing session identity, records, and spool"
                );
            }
            if let Some(dir) = spool_dir {
                crate::spool::remove_all(dir);
            }
        }
        if state.session_identity().is_none() {
            state.set_session_identity(crate::resume::UploadSessionIdentity::new(
                None,
                (*release_layout).clone(),
            ));
        }
    }

    let encryption_adapter = if let Some(ref password) = config.encrypt_password {
        let salt = if let Some(resume) = resume_arc {
            let mut state = resume.lock().unwrap();
            if let Some(existing_salt) = state.session_salt() {
                *existing_salt
            } else {
                let fresh_salt = crate::crypto::control::generate_alphabet_salt();
                state.set_session_identity(crate::resume::UploadSessionIdentity::new(
                    Some(fresh_salt),
                    (*release_layout).clone(),
                ));
                fresh_salt
            }
        } else {
            crate::crypto::control::generate_alphabet_salt()
        };
        let session = Arc::new(crate::crypto::EncryptionSession::new(password, salt)?);
        Some(Arc::new(crate::crypto::UploadEncryptionAdapter::new(
            session,
        )))
    } else {
        None
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
        release_layout,
        encryption_adapter,
        initial_pool,
    })
}
