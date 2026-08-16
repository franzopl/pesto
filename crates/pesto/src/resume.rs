//! Upload resume: persist already-posted segments so an interrupted run can
//! continue without re-sending articles the server already accepted.
//!
//! State is stored as a JSON file (`.pesto-state`) beside the `.nzb` output.
//! Each record maps a `(relative_file_name, part_number)` pair to the
//! `Message-ID` (and wire size) issued when the segment was originally
//! posted. On resume, workers skip segments present in the state and inject
//! the stored `Message-ID`/`bytes` directly into the results, so the final
//! `.nzb` is correct.
//!
//! Trusting a state file blindly is unsafe (see GitHub issue #18): the same
//! output name can be reused for an unrelated or edited file, or with
//! different posting parameters (`--article-size`, `--obfuscate`,
//! `--compress`, `--par2`, `--file-counter`) that change how the input is
//! chunked and named.
//! [`RunFingerprint`] and [`FileFingerprint`] guard against exactly that —
//! a run-level mismatch discards the whole state, a per-file mismatch
//! discards only that file's segments.

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFingerprint {
    pub article_size: u64,
    pub obfuscate: ObfuscateMode,
    pub compress_format: Option<String>,
    pub par2_percent: u8,
    /// `--file-counter`: toggling it changes every subject in the release
    /// (the `[filenum/total]` prefix), so a mismatch must discard the state
    /// the same way an `--obfuscate`/`--par2` change does — otherwise a
    /// resumed run's reused Message-IDs would be recorded against a subject
    /// that no longer matches what the original run actually posted.
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
    pub line_length: usize,
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
            line_length: config.line_length,
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

/// One recorded segment: the `Message-ID` a prior run's `POST` was
/// acknowledged under, and the wire size actually sent (headers + encoded
/// body) — needed so a resumed segment can report its real size in the NZB
/// instead of `0`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SegmentRecord {
    pub message_id: String,
    pub bytes: u64,
}

/// Persistent state for a single upload session.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResumeState {
    /// Posting parameters this state was recorded under. `None` for a fresh
    /// state (nothing recorded yet) or a state file written before this
    /// field existed — in either case there is nothing to compare against
    /// yet, so `validate_run` treats it as compatible and starts tracking.
    fingerprint: Option<RunFingerprint>,
    /// The obfuscated archive stem this release was compressed under, when
    /// `--compress`+`--obfuscate` produced one. Randomly generated fresh on
    /// every run by default — recorded here so a `--resume` run with a
    /// matching fingerprint can reuse the *same* name instead of generating
    /// a new one, which would otherwise make this file's segments
    /// unresumable (the resume key is this very name). See issue #18's
    /// resume follow-up discussion.
    archive_stem: Option<String>,
    /// The shared wire name prefix and sender identity every file in an
    /// `ObfuscateMode::FullShared` run posts under — see
    /// `ObfuscateMode::FullShared`. Randomly generated fresh on every run by
    /// default; recorded here for the same reason as `archive_stem`, so a
    /// `--resume` run reuses the same identity instead of a new one.
    release_prefix: Option<String>,
    release_from: Option<String>,
    /// Per-file identity, keyed by `file_name`.
    files: HashMap<String, FileFingerprint>,
    /// Key: `"{file_name}\0{part}"`.
    segments: HashMap<String, SegmentRecord>,
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
        match serde_json::from_str(&text) {
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
                self.segments.clear();
                self.files.clear();
                self.archive_stem = None;
                self.release_prefix = None;
                self.release_from = None;
                self.fingerprint = Some(current.clone());
                return false;
            }
        }
        self.fingerprint = Some(current.clone());
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

    /// Record a successfully posted segment.
    pub fn record(&mut self, file_name: &str, part: u32, message_id: &str, bytes: u64) {
        self.segments.insert(
            Self::key(file_name, part),
            SegmentRecord {
                message_id: message_id.to_string(),
                bytes,
            },
        );
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fp(article_size: u64) -> RunFingerprint {
        RunFingerprint {
            article_size,
            obfuscate: ObfuscateMode::None,
            compress_format: None,
            par2_percent: 0,
            file_counter: false,
            par2_slice_size: None,
            par2_slice_count: None,
            par2_recovery_count: None,
            compress_volume_size: None,
            compress_password: None,
            line_length: 128,
        }
    }

    #[test]
    fn round_trip() {
        let mut s = ResumeState::default();
        s.record("file.bin", 1, "abc@example.com", 100);
        s.record("file.bin", 2, "def@example.com", 200);
        assert_eq!(s.get("file.bin", 1).unwrap().message_id, "abc@example.com");
        assert_eq!(s.get("file.bin", 1).unwrap().bytes, 100);
        assert!(s.get("file.bin", 3).is_none());
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn remove_forgets_a_recorded_segment() {
        let mut s = ResumeState::default();
        s.record("file.bin", 1, "abc@example.com", 100);
        s.record("file.bin", 2, "def@example.com", 200);
        s.remove("file.bin", 1);
        assert!(s.get("file.bin", 1).is_none());
        assert_eq!(s.get("file.bin", 2).unwrap().message_id, "def@example.com");
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn remove_of_an_unrecorded_segment_is_a_no_op() {
        let mut s = ResumeState::default();
        s.record("file.bin", 1, "abc@example.com", 100);
        s.remove("file.bin", 99);
        assert_eq!(s.len(), 1);
    }

    #[test]
    fn save_and_load() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = ResumeState::default();
        s.record("a.bin", 1, "id1@x", 100);
        s.save(&path).unwrap();
        let loaded = ResumeState::load(&path).unwrap();
        assert_eq!(loaded.get("a.bin", 1).unwrap().message_id, "id1@x");
        assert_eq!(loaded.get("a.bin", 1).unwrap().bytes, 100);
    }

    #[test]
    fn missing_file_returns_empty() {
        let state = ResumeState::load(Path::new("/nonexistent/path.json")).unwrap();
        assert!(state.is_empty());
    }

    #[test]
    fn fresh_state_accepts_any_fingerprint() {
        let mut s = ResumeState::default();
        assert!(s.validate_run(&fp(768_000)));
    }

    #[test]
    fn matching_fingerprint_keeps_segments() {
        let mut s = ResumeState::default();
        assert!(s.validate_run(&fp(768_000)));
        s.record("a.bin", 1, "id1@x", 100);
        assert!(s.validate_run(&fp(768_000)));
        assert_eq!(s.len(), 1, "matching fingerprint must not clear segments");
    }

    #[test]
    fn mismatched_fingerprint_discards_everything() {
        let mut s = ResumeState::default();
        assert!(s.validate_run(&fp(768_000)));
        s.record_file(
            "a.bin",
            FileFingerprint {
                size: 10,
                mtime: Some(1),
            },
        );
        s.record("a.bin", 1, "id1@x", 100);
        s.set_archive_stem("Xk3mQp".to_string());
        // A resume run using a different --article-size than the original.
        assert!(!s.validate_run(&fp(384_000)));
        assert_eq!(s.len(), 0, "mismatched fingerprint must discard segments");
        assert!(
            s.file_matches(
                "a.bin",
                &FileFingerprint {
                    size: 10,
                    mtime: Some(1)
                }
            ),
            "per-file fingerprints must be cleared too"
        );
        assert_eq!(
            s.archive_stem(),
            None,
            "archive stem must be cleared too — reusing it for incompatible parameters is unsafe"
        );
    }

    #[test]
    fn archive_stem_survives_a_matching_fingerprint() {
        let mut s = ResumeState::default();
        assert!(s.validate_run(&fp(768_000)));
        s.set_archive_stem("Xk3mQp".to_string());
        assert!(s.validate_run(&fp(768_000)));
        assert_eq!(s.archive_stem(), Some("Xk3mQp"));
    }

    #[test]
    fn file_matches_is_permissive_when_nothing_was_recorded() {
        let s = ResumeState::default();
        assert!(s.file_matches(
            "new.bin",
            &FileFingerprint {
                size: 5,
                mtime: Some(1)
            }
        ));
    }

    #[test]
    fn file_matches_detects_content_change() {
        let mut s = ResumeState::default();
        s.record_file(
            "a.bin",
            FileFingerprint {
                size: 100,
                mtime: Some(1000),
            },
        );
        assert!(s.file_matches(
            "a.bin",
            &FileFingerprint {
                size: 100,
                mtime: Some(1000)
            }
        ));
        assert!(!s.file_matches(
            "a.bin",
            &FileFingerprint {
                size: 200,
                mtime: Some(1000)
            }
        ));
        assert!(!s.file_matches(
            "a.bin",
            &FileFingerprint {
                size: 100,
                mtime: Some(2000)
            }
        ));
    }

    #[test]
    fn forget_file_only_removes_that_files_segments() {
        let mut s = ResumeState::default();
        s.record("a.bin", 1, "id1@x", 10);
        s.record("a.bin", 2, "id2@x", 10);
        s.record("b.bin", 1, "id3@x", 10);
        s.forget_file("a.bin");
        assert!(s.get("a.bin", 1).is_none());
        assert!(s.get("a.bin", 2).is_none());
        assert!(s.get("b.bin", 1).is_some());
    }

    #[test]
    fn forget_all_segments_clears_everything_including_par2_volumes() {
        let mut s = ResumeState::default();
        s.record("movie.mkv", 1, "id1@x", 10);
        s.record("movie.mkv.vol000+001.par2", 1, "id2@x", 10);
        s.record_file(
            "movie.mkv",
            FileFingerprint {
                size: 100,
                mtime: Some(1),
            },
        );
        s.forget_all_segments();
        assert!(s.get("movie.mkv", 1).is_none());
        assert!(s.get("movie.mkv.vol000+001.par2", 1).is_none());
        assert_eq!(s.len(), 0);
        // Per-file fingerprints survive — an unrelated file's identity is
        // still worth remembering.
        assert!(s.file_matches(
            "movie.mkv",
            &FileFingerprint {
                size: 100,
                mtime: Some(1)
            }
        ));
    }

    #[test]
    fn corrupt_state_file_loads_as_empty() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        std::fs::write(&path, "this is not json {").unwrap();
        let loaded = ResumeState::load(&path).unwrap();
        assert!(loaded.is_empty());
    }

    #[test]
    fn save_is_readable_after_replace() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("state.json");
        let mut s = ResumeState::default();
        s.record("a.bin", 1, "id1@x", 100);
        s.save(&path).unwrap();
        assert!(!path.with_file_name("state.json.tmp").exists());
        let loaded = ResumeState::load(&path).unwrap();
        assert_eq!(loaded.get("a.bin", 1).unwrap().message_id, "id1@x");
    }

    #[test]
    fn each_new_fingerprint_field_changes_identity() {
        let base = fp(768_000);
        let mut slice = base.clone();
        slice.par2_slice_size = Some(4096);
        assert_ne!(base, slice);

        let mut count = base.clone();
        count.par2_slice_count = Some(100);
        assert_ne!(base, count);

        let mut rec = base.clone();
        rec.par2_recovery_count = Some(20);
        assert_ne!(base, rec);

        let mut vol = base.clone();
        vol.compress_volume_size = Some("500m".into());
        assert_ne!(base, vol);

        let mut pw = base.clone();
        pw.compress_password = Some(hash_secret("secret"));
        assert_ne!(base, pw);

        let mut ll = base.clone();
        ll.line_length = 256;
        assert_ne!(base, ll);
    }
}
