//! Upload resume: persist already-posted segments so an interrupted run can
//! continue without re-sending articles the server already accepted.
//!
//! State is stored as a JSON file (`.pesto-state`) beside the `.nzb` output.
//! Each record maps a `(relative_file_name, part_number)` pair to the
//! `Message-ID` (and wire size) issued when the segment was originally
//! posted, plus whether STAT 223 confirmed it. On resume, `prepare_ready`
//! either skips the segment, re-STATs the stored id without a second POST,
//! or POSTs a fresh article — see [`resume_action`].
//!
//! Trusting a state file blindly is unsafe (see GitHub issue #18): the same
//! output name can be reused for an unrelated or edited file, or with
//! different posting parameters (`--article-size`, `--obfuscate`,
//! `--compress`, `--par2`, `--file-counter`) that change how the input is
//! chunked and named. [`RunFingerprint`] and [`FileFingerprint`] guard
//! against that — a run-level mismatch discards the whole state, a per-file
//! mismatch discards only that file's segments.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::config::{Config, ObfuscateMode};

/// Posting parameters that change how the *whole* input is chunked or named.
/// Captured once per run and compared on `--resume`: any difference means
/// every recorded Message-ID in the state could reference the wrong byte
/// range or the wrong wire name, so the whole state is discarded rather than
/// trusted partially.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunFingerprint {
    pub article_size: u64,
    pub obfuscate: ObfuscateMode,
    pub compress_format: Option<String>,
    pub par2_percent: u8,
    /// `--file-counter`: toggling it changes every subject in the release
    /// (the `[filenum/total]` prefix), so a mismatch must discard the state
    /// the same way an `--obfuscate`/`--par2` change does.
    pub file_counter: bool,
    /// Geometry / compression options that change the posted bytes without
    /// touching `article_size` or `par2_percent`. `#[serde(default)]` so a
    /// state file written before these fields existed still loads.
    #[serde(default)]
    pub par2_slice_size: Option<usize>,
    #[serde(default)]
    pub par2_slice_count: Option<usize>,
    #[serde(default)]
    pub par2_recovery_count: Option<usize>,
    #[serde(default)]
    pub compress_volume_size: Option<String>,
    /// SHA-256 hex of the compression password. The state file is plaintext;
    /// never store the password itself.
    #[serde(default)]
    pub compress_password: Option<String>,
    #[serde(default)]
    pub encrypt_password: Option<String>,
    #[serde(default)]
    pub line_length: usize,
    /// See [`Config::par2_effort`]. A state written before this field
    /// existed was built at the default.
    #[serde(default = "default_par2_effort")]
    pub par2_effort: f64,
    /// PAR2 layout the set was built with: [`PAR2_LAYOUT`] when `--par2` is
    /// on, `0` otherwise. A state written by an older layout, or before this
    /// field existed, defaults to `0` and is discarded if the resumed run has
    /// PAR2 enabled, since its PAR2 segments do not match what this version
    /// builds.
    #[serde(default)]
    pub par2_layout: u8,
}

/// [`RunFingerprint::par2_layout`] for the current PAR2 layout: File IDs
/// sorted little-endian per the PAR2 spec (issue #212) and slices aligned to
/// the article size (issue #217). Bump it whenever the same config would
/// produce different PAR2 bytes.
pub const PAR2_LAYOUT: u8 = 1;

fn default_par2_effort() -> f64 {
    crate::config::DEFAULT_PAR2_EFFORT
}

impl RunFingerprint {
    pub fn from_config(config: &Config) -> Self {
        Self {
            article_size: config.article_size as u64,
            obfuscate: config.obfuscate,
            compress_format: config.compress_format.clone(),
            par2_percent: config.par2,
            file_counter: config.file_counter,
            par2_slice_size: config.par2_slice_size,
            par2_slice_count: config.par2_slice_count,
            par2_recovery_count: config.par2_recovery_count,
            compress_volume_size: config.compress_volume_size.clone(),
            compress_password: config.compress_password.as_deref().map(hash_secret),
            encrypt_password: config.encrypt_password.as_deref().map(hash_secret),
            line_length: config.line_length,
            par2_effort: config.par2_effort,
            par2_layout: if config.par2 > 0 { PAR2_LAYOUT } else { 0 },
        }
    }
}

fn hash_secret(secret: &str) -> String {
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(secret.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// A single file's identity at the time its segments were recorded. Compared
/// on `--resume` to catch the same output name being reused for edited or
/// unrelated content — a mismatch discards only this file's segments, not
/// the whole state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileFingerprint {
    pub size: u64,
    /// Unix timestamp (seconds). `None` when the filesystem/platform can't
    /// report an mtime — treated as "nothing to compare", not a mismatch.
    pub mtime: Option<u64>,
}

/// In-memory confirmation state for a posted article. Not serialized —
/// disk uses [`SegmentRecord`]'s two bools + `server_idx`.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConfirmState {
    Posted,
    CheckPending,
    CheckInFlight,
    Confirmed,
    RetryWait,
    Reposting,
    Recovering,
    MissingConfirmed,
    Inconclusive,
}

/// What `--resume` should do with a stored [`SegmentRecord`] (or the lack of
/// one) on this run. Evaluated in this order: no record → POST; check off
/// or already confirmed → skip; otherwise re-STAT the stored Message-ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResumeAction {
    /// Inject the stored `PostedSegment`; no POST; no STAT.
    Skip,
    /// Inject the stored Message-ID into results and enqueue a STAT of that
    /// same id. Do not encode or POST.
    ReStatStoredId,
    /// Encode and POST a fresh Message-ID (MissingConfirmed stripped, or
    /// never posted).
    Post,
}

/// Decide the `--resume` arm for this segment. Pre-schema JSON (no
/// `confirmed`/`check_disabled` fields) deserializes both bools as `false`,
/// which is the re-STAT path under `--check` and the skip path under
/// `--no-check`. `check_disabled` is not inferred from old files.
pub fn resume_action(check: bool, rec: Option<&SegmentRecord>) -> ResumeAction {
    match rec {
        None => ResumeAction::Post,
        Some(_) if !check => ResumeAction::Skip,
        Some(r) if r.confirmed => ResumeAction::Skip,
        Some(_) => ResumeAction::ReStatStoredId,
    }
}

/// One recorded segment: the `Message-ID` a prior run's `POST` was
/// acknowledged under, and the wire size actually sent (headers + encoded
/// body) — needed so a resumed segment can report its real size in the NZB
/// instead of `0`.
///
/// `confirmed` is set **only** after STAT 223. A `--no-check` run never
/// writes `confirmed: true` (that run never STATed). New fields use
/// `#[serde(default)]` so a pre-schema `{message_id, bytes}` record still
/// loads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SegmentRecord {
    pub message_id: String,
    pub bytes: u64,
    /// STAT 223 observed in the run that wrote this record.
    #[serde(default)]
    pub confirmed: bool,
    /// The writing run had `--no-check`. Informational; skip-when-check-off
    /// does not require this field (any existing record is skipped when
    /// check is off).
    #[serde(default)]
    pub check_disabled: bool,
    /// Server that accepted the 240. Arm 2 copies this into `PostedSegment`
    /// so STAT retargets the right host. Pre-schema JSON → 0.
    #[serde(default)]
    pub server_idx: usize,
    /// Exact headers/body identity used by the accepted article. Older state
    /// files deserialize this as `None`; they remain usable for STAT/skip but
    /// must not silently invent names when a private-mode repost is required.
    #[serde(default)]
    pub wire_identity: Option<PersistedWireIdentity>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PersistedWireIdentity {
    pub subject_name: String,
    pub yenc_name: String,
    pub from: String,
    pub date: Option<String>,
    pub unix_date: Option<u64>,
}

/// Upload session identity: 16-byte salt, complete layout and ordered source names.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadSessionIdentity {
    pub salt: [u8; 16],
    pub layout: crate::poster::ReleaseLayout,
    /// Ordered data file names in posting order. Replaced sources with
    /// identical part counts must rotate salt (nonce-reuse guard).
    /// `#[serde(default)]` so pre-identity state files load but fail this
    /// comparison and rotate rather than trust a count-only match.
    #[serde(default)]
    pub source_names: Vec<String>,
}

/// Persistent state for a single upload session.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ResumeState {
    /// Posting parameters this state was recorded under. `None` for a fresh
    /// state or one written before this field existed — either way nothing
    /// to compare against, so `validate_run` treats it as compatible.
    fingerprint: Option<RunFingerprint>,
    /// The obfuscated archive stem this release was compressed under, when
    /// `--compress`+`--obfuscate` produced one. Recorded so a `--resume`
    /// run with a matching fingerprint reuses the *same* name (the resume
    /// key is this very name). See issue #18's resume follow-up discussion.
    archive_stem: Option<String>,
    /// Shared wire name prefix/sender for an `ObfuscateMode::FullShared`
    /// run; recorded so `--resume` reuses the same identity.
    release_prefix: Option<String>,
    release_from: Option<String>,
    /// Per-file identity, keyed by `file_name`.
    files: HashMap<String, FileFingerprint>,
    /// Key: `"{file_name}\0{part}"`.
    segments: HashMap<String, SegmentRecord>,
    /// Session identity (salt, layout, ordered source names).
    #[serde(default)]
    session_identity: Option<UploadSessionIdentity>,
    /// Set when the session was invalidated so the spool directory is cleared.
    #[serde(skip)]
    session_invalidated: bool,
}

impl ResumeState {
    fn key(file_name: &str, part: u32) -> String {
        format!("{file_name}\0{part}")
    }

    /// Load state from `path`. Returns an empty state when the file does not
    /// exist (fresh run) or cannot be parsed (corrupt state file). I/O
    /// failures other than not-found remain hard errors.
    pub fn load(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(e) => {
                return Err(e)
                    .with_context(|| format!("reading resume state `{}`", path.display()));
            }
        };
        match serde_json::from_str::<Self>(&text) {
            Ok(state) => Ok(state),
            Err(_) => Ok(Self::default()),
        }
    }

    /// Write the current state to `path` via a sibling temp file + rename
    /// so a crash mid-write cannot leave a truncated JSON document.
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string(self).context("serialising resume state")?;
        let tmp = {
            let mut name = path.as_os_str().to_owned();
            name.push(".tmp");
            std::path::PathBuf::from(name)
        };
        std::fs::write(&tmp, &text)
            .with_context(|| format!("writing resume state `{}`", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| {
            format!(
                "replacing resume state `{}` from `{}`",
                path.display(),
                tmp.display()
            )
        })
    }

    /// Compare `current` against the fingerprint this state was last
    /// recorded under. On a mismatch, every segment and per-file fingerprint
    /// is discarded (the whole state can no longer be trusted — see the
    /// module doc comment) and `false` is returned so the caller can warn
    /// the user. On a match, or when this state has no fingerprint yet
    /// (fresh state, or one written before this field existed), the
    /// fingerprint is set to `current` and `true` is returned.
    pub fn validate_run(&mut self, current: &RunFingerprint) -> bool {
        if let Some(stored) = &self.fingerprint {
            if stored != current {
                self.invalidate_session();
                self.fingerprint = Some(current.clone());
                return false;
            }
        }
        self.fingerprint = Some(current.clone());
        true
    }

    /// Record (or update) the upload session identity.
    pub fn set_session_identity(&mut self, identity: UploadSessionIdentity) {
        self.session_identity = Some(identity);
    }

    /// The opaque 16-byte session salt recorded for this state, if any.
    pub fn session_salt(&self) -> Option<&[u8; 16]> {
        self.session_identity.as_ref().map(|s| &s.salt)
    }

    /// Invalidate the entire upload session, clearing session identity (salt,
    /// layout, source names), all segment records, all file fingerprints,
    /// archive stem, and shared release identity. Used when the encrypted
    /// session can no longer be trusted: a changed posting fingerprint,
    /// layout, or source set can reuse the same key/index space over
    /// *different* plaintext, so the salt must be regenerated and every
    /// record dropped.
    pub fn invalidate_session(&mut self) {
        self.session_invalidated = true;
        self.session_identity = None;
        self.segments.clear();
        self.files.clear();
        self.archive_stem = None;
        self.release_prefix = None;
        self.release_from = None;
    }

    /// Take and clear the session_invalidated dirty bit (set by
    /// `validate_run`/`invalidate_session`). `prepare_resources` consumes
    /// it to clear the spool directory along with the invalidated state.
    pub fn take_session_invalidated(&mut self) -> bool {
        std::mem::take(&mut self.session_invalidated)
    }

    /// Validate the current release layout and ordered source identity
    /// against this session's recorded identity. A mismatch (including a
    /// stored identity without source names) invalidates the session and
    /// returns `false`. (The posting fingerprint is validated separately
    /// by `validate_run`.)
    pub fn validate_session(
        &mut self,
        current_layout: &crate::poster::ReleaseLayout,
        current_source_names: &[String],
    ) -> bool {
        if let Some(stored) = &self.session_identity {
            if stored.layout != *current_layout || stored.source_names != current_source_names {
                self.invalidate_session();
                return false;
            }
        }

        true
    }

    /// The archive stem recorded for this state, if any — see the field doc.
    pub fn archive_stem(&self) -> Option<&str> {
        self.archive_stem.as_deref()
    }

    /// Record the archive stem this run decided on, for a future `--resume`
    /// to reuse. Call only after `validate_run` so a mismatched fingerprint
    /// has already cleared any stale value first.
    pub fn set_archive_stem(&mut self, stem: String) {
        self.archive_stem = Some(stem);
    }

    /// The `ObfuscateMode::FullShared` prefix/sender identity recorded for
    /// this state, if any — see the field doc.
    pub fn release_identity(&self) -> Option<(&str, &str)> {
        Some((
            self.release_prefix.as_deref()?,
            self.release_from.as_deref()?,
        ))
    }

    /// Record the `FullShared` prefix/sender identity this run decided on,
    /// for a future `--resume` to reuse. Call only after `validate_run`.
    pub fn set_release_identity(&mut self, prefix: String, from: String) {
        self.release_prefix = Some(prefix);
        self.release_from = Some(from);
    }

    /// Compare `current` against the fingerprint recorded for `file_name`,
    /// if any. `None` recorded yet (a new file, or a state file written
    /// before this field existed) is treated as compatible — there is
    /// nothing to contradict. A concrete mismatch returns `false`.
    pub fn file_matches(&self, file_name: &str, current: &FileFingerprint) -> bool {
        match self.files.get(file_name) {
            Some(stored) => stored == current,
            None => true,
        }
    }

    /// Whether this state file has a recorded fingerprint for `file_name`.
    pub fn has_file(&self, file_name: &str) -> bool {
        self.files.contains_key(file_name)
    }

    /// Record (or update) a file's fingerprint.
    pub fn record_file(&mut self, file_name: &str, fp: FileFingerprint) {
        self.files.insert(file_name.to_string(), fp);
    }

    /// Forget every segment recorded for `file_name` — used when
    /// `file_matches` reports a mismatch, so stale segments for the old
    /// content don't linger keyed under the same name.
    pub fn forget_file(&mut self, file_name: &str) {
        let prefix = format!("{file_name}\0");
        self.segments.retain(|k, _| !k.starts_with(&prefix));
    }

    /// Forget every segment recorded for *every* file, including PAR2
    /// volumes (which never go through `record_file`/`file_matches` at all
    /// — see the call site). Used instead of `forget_file` when PAR2 is
    /// active: PAR2 recovery blocks are computed over the *whole* recovery
    /// set together, not per file, so one file's content changing
    /// invalidates every recovery volume's segments too, not just that
    /// file's own. Per-file fingerprints are kept so an unrelated,
    /// unchanged file doesn't get re-flagged as new on the next check.
    pub fn forget_all_segments(&mut self) {
        self.segments.clear();
    }

    /// Return the stored record for a segment, if it was already posted.
    pub fn get(&self, file_name: &str, part: u32) -> Option<&SegmentRecord> {
        self.segments.get(&Self::key(file_name, part))
    }

    /// Record a successfully posted segment. `confirmed`/`check_disabled`/
    /// `server_idx` default to `(false, false, 0)` — the POST-240 path
    /// should call [`Self::record_with`] so those flags match the run.
    pub fn record(&mut self, file_name: &str, part: u32, message_id: &str, bytes: u64) {
        self.record_with(
            file_name,
            part,
            SegmentRecord {
                message_id: message_id.to_string(),
                bytes,
                confirmed: false,
                check_disabled: false,
                server_idx: 0,
                wire_identity: None,
            },
        );
    }

    /// Insert or overwrite the record for `(file_name, part)`.
    pub fn record_with(&mut self, file_name: &str, part: u32, rec: SegmentRecord) {
        self.segments.insert(Self::key(file_name, part), rec);
    }

    /// Flip `confirmed` after STAT 223. No-op if the segment is not recorded.
    /// Never called on a `--no-check` run (that path never STATs).
    pub fn mark_confirmed(&mut self, file_name: &str, part: u32) {
        if let Some(rec) = self.segments.get_mut(&Self::key(file_name, part)) {
            rec.confirmed = true;
            rec.check_disabled = false;
        }
    }

    /// Forget a segment — used when a POST that once succeeded is later
    /// confirmed missing by the streaming check (and every repost/recovery
    /// attempt for it also failed): the recorded Message-ID is known bad, so
    /// a later `--resume` must not trust it and skip re-posting the segment.
    pub fn remove(&mut self, file_name: &str, part: u32) {
        self.segments.remove(&Self::key(file_name, part));
    }

    /// Number of segments recorded in this state.
    pub fn len(&self) -> usize {
        self.segments.len()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }

    /// Whether any recorded article predates exact Subject/yEnc/From
    /// persistence. Such records are safe to STAT, but an interrupted
    /// obfuscated file cannot be completed without splitting its wire identity.
    pub fn has_legacy_wire_identities(&self) -> bool {
        self.segments
            .values()
            .any(|record| record.wire_identity.is_none())
    }
}

#[cfg(test)]
#[cfg(test)]
mod tests;

#[cfg(test)]
mod session_tests;
