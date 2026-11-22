//! Type-1 resume spool: cache the fully-encoded article (headers + yEnc
//! body, under the exact `Message-ID` it was sent with) for a segment right
//! before it goes over the wire, so a `POST` whose `240` acknowledgement is
//! lost to a dropped connection can be resumed by replaying the *exact same
//! bytes under the exact same Message-ID* — instead of silently re-encoding
//! and re-posting under a fresh one, which risks a duplicate article on the
//! server if the original `POST` actually succeeded.
//!
//! This only closes the "maybe-posted, ack lost" ambiguity for whatever
//! segment(s) were in flight at the moment of interruption — it does not
//! remove the need to re-derive segments the run never reached at all (see
//! `resume`'s module doc comment for the broader picture).
//!
//! Opt-in (only active when `config.resume` is set): every write here is a
//! real disk write on the posting hot path, which a plain (non-resume) run
//! must never pay for. See GitHub issue #18's resume follow-up discussion.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::resume::PersistedWireIdentity;

const V2_MAGIC: &[u8; 4] = b"PST2";
const V3_MAGIC: &[u8; 4] = b"PST3";

/// Encryption identity stamped into every spool entry written by the poster
/// (Round 12 PU12-01 / DECISIONS Q3(a)): the creating run's encryption mode
/// and, for encrypted runs, a fingerprint of its session salt. Replay only
/// accepts an entry whose stamp matches the current run exactly, so spool
/// bytes created as plaintext can never be replayed into an encrypted NZB
/// (or vice versa), and a salt rotation invalidates old entries even when
/// the spool directory itself survived. The fingerprint is a SHA-256 digest
/// of the salt — it identifies the salt without carrying it, and is never
/// logged.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SpoolEncryptionStamp {
    pub encrypted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub salt_fingerprint: Option<String>,
}

impl SpoolEncryptionStamp {
    /// Stamp for an ordinary (unencrypted) run.
    pub fn ordinary() -> Self {
        Self {
            encrypted: false,
            salt_fingerprint: None,
        }
    }

    /// Stamp for an encrypted run using `salt` as its session salt.
    pub fn encrypted(salt: &[u8; 16]) -> Self {
        let digest = Sha256::digest(salt);
        let mut fingerprint = String::with_capacity(digest.len() * 2);
        for b in digest {
            use std::fmt::Write;
            let _ = write!(&mut fingerprint, "{b:02x}");
        }
        Self {
            encrypted: true,
            salt_fingerprint: Some(fingerprint),
        }
    }

    /// Stamp for the run an encryption adapter belongs to, or the ordinary
    /// stamp when the run has no adapter.
    pub fn for_adapter(adapter: Option<&crate::crypto::UploadEncryptionAdapter>) -> Self {
        match adapter {
            Some(adapter) => Self::encrypted(&adapter.salt()),
            None => Self::ordinary(),
        }
    }

    /// Whether an entry carrying `entry_stamp` may be replayed by a run
    /// stamped `self`. Legacy entries written before stamps existed carry
    /// no stamp: they may only be replayed by an ordinary run (encrypted
    /// runs always wrote stamped entries through the poster, so an
    /// unstamped entry is never trusted with ciphertext).
    pub fn allows_replay(&self, entry_stamp: Option<&Self>) -> bool {
        match entry_stamp {
            Some(stamp) => stamp == self,
            None => !self.encrypted,
        }
    }
}

#[derive(Serialize, Deserialize)]
struct SpoolEnvelope {
    wire_identity: Option<PersistedWireIdentity>,
    encryption_stamp: Option<SpoolEncryptionStamp>,
}

/// Directory holding spooled articles for one upload session — a sibling of
/// the `.pesto-state` file (not nested inside it, since that path is a
/// plain file), so both can be inspected or deleted independently.
pub fn spool_dir(resume_path: &Path) -> PathBuf {
    resume_path.with_extension("pesto-spool")
}

fn entry_path(dir: &Path, file_name: &str, part: u32) -> PathBuf {
    // `file_name` is a relative path (e.g. `season01/ep01.mkv`) — replace
    // separators so it collapses to a single valid path component instead
    // of implying subdirectories that were never created.
    let safe_name = file_name.replace(['/', '\\'], "_");
    dir.join(format!("{safe_name}.{part}.spool"))
}

/// One spooled article, as read back for replay.
pub struct SpooledArticle {
    pub message_id: String,
    pub headers: Vec<u8>,
    pub body: Vec<u8>,
    pub wire_identity: Option<PersistedWireIdentity>,
    /// Encryption stamp of the run that created this entry, if the entry
    /// was written with one (all poster-written entries carry it).
    pub encryption_stamp: Option<SpoolEncryptionStamp>,
}

/// Persist a fully-encoded article to the spool, creating the directory on
/// first use. Best-effort by design: a spool write failing (e.g. disk full)
/// must never abort an otherwise-successful post — callers log and move on
/// rather than propagate.
pub async fn write(
    dir: &Path,
    file_name: &str,
    part: u32,
    message_id: &str,
    headers: &[u8],
    body: &[u8],
) -> Result<()> {
    write_inner(dir, file_name, part, message_id, headers, body, None, None).await
}

/// Versioned spool writer used by the poster. Unlike the legacy public
/// helper, this records the logical wire identity alongside the exact bytes
/// so resume metadata cannot disagree with a replayed article.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_with_identity(
    dir: &Path,
    file_name: &str,
    part: u32,
    message_id: &str,
    headers: &[u8],
    body: &[u8],
    wire_identity: &PersistedWireIdentity,
    encryption_stamp: &SpoolEncryptionStamp,
) -> Result<()> {
    write_inner(
        dir,
        file_name,
        part,
        message_id,
        headers,
        body,
        Some(wire_identity),
        Some(encryption_stamp),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn write_inner(
    dir: &Path,
    file_name: &str,
    part: u32,
    message_id: &str,
    headers: &[u8],
    body: &[u8],
    wire_identity: Option<&PersistedWireIdentity>,
    encryption_stamp: Option<&SpoolEncryptionStamp>,
) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating spool directory `{}`", dir.display()))?;
    // PST3 layout: [magic][u32 envelope_len][envelope JSON], followed by the
    // legacy [u32 id_len][id][u32 headers_len][headers][body] payload. The
    // envelope carries both the wire identity and the encryption stamp.
    // PST2 ([magic][u32 identity_len][bare identity JSON]) entries remain
    // readable. The body is raw (non-UTF-8) yEnc bytes, so the small binary
    // envelope avoids copying it through a general-purpose serialization format.
    let envelope = if wire_identity.is_some() || encryption_stamp.is_some() {
        Some(
            serde_json::to_vec(&SpoolEnvelope {
                wire_identity: wire_identity.cloned(),
                encryption_stamp: encryption_stamp.cloned(),
            })
            .context("serialising spool envelope")?,
        )
    } else {
        None
    };
    let envelope_len = envelope.as_ref().map_or(0, Vec::len);
    let mut buf = Vec::with_capacity(
        V3_MAGIC.len() + 4 + envelope_len + 4 + message_id.len() + 4 + headers.len() + body.len(),
    );
    if let Some(envelope) = envelope {
        buf.extend_from_slice(V3_MAGIC);
        buf.extend_from_slice(&(envelope.len() as u32).to_le_bytes());
        buf.extend_from_slice(&envelope);
    }
    buf.extend_from_slice(&(message_id.len() as u32).to_le_bytes());
    buf.extend_from_slice(message_id.as_bytes());
    buf.extend_from_slice(&(headers.len() as u32).to_le_bytes());
    buf.extend_from_slice(headers);
    buf.extend_from_slice(body);
    let path = entry_path(dir, file_name, part);
    tokio::fs::write(&path, buf)
        .await
        .with_context(|| format!("writing spool entry `{}`", path.display()))
}

fn parse(buf: &[u8]) -> Option<SpooledArticle> {
    let mut pos = 0usize;
    let (wire_identity, encryption_stamp) = if buf.starts_with(V3_MAGIC) {
        pos += V3_MAGIC.len();
        let envelope_len = u32::from_le_bytes(buf.get(pos..pos + 4)?.try_into().ok()?) as usize;
        pos += 4;
        let envelope: SpoolEnvelope =
            serde_json::from_slice(buf.get(pos..pos + envelope_len)?).ok()?;
        pos += envelope_len;
        (envelope.wire_identity, envelope.encryption_stamp)
    } else if buf.starts_with(V2_MAGIC) {
        pos += V2_MAGIC.len();
        let identity_len = u32::from_le_bytes(buf.get(pos..pos + 4)?.try_into().ok()?) as usize;
        pos += 4;
        let identity = serde_json::from_slice(buf.get(pos..pos + identity_len)?).ok()?;
        pos += identity_len;
        (Some(identity), None)
    } else {
        (None, None)
    };
    let id_len = u32::from_le_bytes(buf.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    let message_id = String::from_utf8(buf.get(pos..pos + id_len)?.to_vec()).ok()?;
    pos += id_len;
    let headers_len = u32::from_le_bytes(buf.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    let headers = buf.get(pos..pos + headers_len)?.to_vec();
    pos += headers_len;
    let body = buf.get(pos..)?.to_vec();
    Some(SpooledArticle {
        message_id,
        headers,
        body,
        wire_identity,
        encryption_stamp,
    })
}

/// Load a spooled article for `(file_name, part)`, if one was recorded.
/// `None` covers both "nothing spooled" (the common case) and a corrupt
/// entry — either way the caller's only sane response is a cache miss
/// (fall back to a fresh encode), never a hard failure of the whole run.
pub fn read(dir: &Path, file_name: &str, part: u32) -> Option<SpooledArticle> {
    let buf = std::fs::read(entry_path(dir, file_name, part)).ok()?;
    parse(&buf)
}

/// Remove one segment's spool entry — once it's confirmed posted (success or
/// permanently failed), its cached bytes serve no further purpose. A no-op
/// if nothing was ever spooled for it.
pub fn remove(dir: &Path, file_name: &str, part: u32) {
    let _ = std::fs::remove_file(entry_path(dir, file_name, part));
}

/// Remove the whole spool directory — called once a run's resume state
/// itself is deleted (fully successful run: nothing left to ever replay).
pub fn remove_all(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_then_read_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(write(
            &spool,
            "season01/ep01.mkv",
            3,
            "abc123@pesto.test",
            b"Message-ID: <abc123@pesto.test>\r\n",
            b"=ybegin ...\r\nnot valid utf8 \xff\xfe\r\n=yend",
        ))
        .unwrap();

        let entry = read(&spool, "season01/ep01.mkv", 3).unwrap();
        assert_eq!(entry.message_id, "abc123@pesto.test");
        assert_eq!(entry.headers, b"Message-ID: <abc123@pesto.test>\r\n");
        assert_eq!(
            entry.body,
            b"=ybegin ...\r\nnot valid utf8 \xff\xfe\r\n=yend"
        );
        assert!(entry.wire_identity.is_none());
    }

    #[test]
    fn versioned_spool_round_trips_wire_identity() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        let identity = PersistedWireIdentity {
            subject_name: "subject-token".into(),
            yenc_name: "yenc-token".into(),
            from: "Poster <opaque@example.com>".into(),
            date: None,
            unix_date: None,
        };
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(write_with_identity(
                &spool,
                "movie.bin",
                1,
                "id@example.com",
                b"headers",
                b"body",
                &identity,
                &SpoolEncryptionStamp::ordinary(),
            ))
            .unwrap();

        let entry = read(&spool, "movie.bin", 1).unwrap();
        assert_eq!(entry.wire_identity, Some(identity));
        assert_eq!(
            entry.encryption_stamp,
            Some(SpoolEncryptionStamp::ordinary())
        );
    }

    #[test]
    fn read_of_missing_entry_is_none() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        assert!(read(&spool, "never-spooled.bin", 1).is_none());
    }

    #[test]
    fn read_of_corrupt_entry_is_none_not_a_panic() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        std::fs::create_dir_all(&spool).unwrap();
        std::fs::write(entry_path(&spool, "a.bin", 1), b"\x00\x00\x00\xff").unwrap();
        assert!(read(&spool, "a.bin", 1).is_none());
    }

    #[test]
    fn remove_deletes_only_the_named_entry() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(write(&spool, "a.bin", 1, "id-a@x", b"h", b"b"))
            .unwrap();
        rt.block_on(write(&spool, "b.bin", 1, "id-b@x", b"h", b"b"))
            .unwrap();
        remove(&spool, "a.bin", 1);
        assert!(read(&spool, "a.bin", 1).is_none());
        assert!(read(&spool, "b.bin", 1).is_some());
    }

    #[test]
    fn remove_all_deletes_the_whole_directory() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(write(&spool, "a.bin", 1, "id-a@x", b"h", b"b"))
            .unwrap();
        remove_all(&spool);
        assert!(!spool.exists());
    }

    #[test]
    fn stamped_spool_round_trips_encryption_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        let identity = PersistedWireIdentity {
            subject_name: "subject-token".into(),
            yenc_name: "yenc-token".into(),
            from: "Poster <opaque@example.com>".into(),
            date: None,
            unix_date: None,
        };
        let stamp = SpoolEncryptionStamp::encrypted(&[0x42u8; 16]);
        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(write_with_identity(
                &spool,
                "movie.bin",
                1,
                "id@example.com",
                b"headers",
                b"body",
                &identity,
                &stamp,
            ))
            .unwrap();

        let entry = read(&spool, "movie.bin", 1).unwrap();
        assert_eq!(entry.wire_identity, Some(identity));
        assert_eq!(entry.encryption_stamp, Some(stamp));
    }

    #[test]
    fn stamp_allows_only_matching_replay() {
        let encrypted_a = SpoolEncryptionStamp::encrypted(&[0x11u8; 16]);
        let encrypted_b = SpoolEncryptionStamp::encrypted(&[0x22u8; 16]);
        let ordinary = SpoolEncryptionStamp::ordinary();
        assert!(encrypted_a.allows_replay(Some(&encrypted_a)));
        assert!(!encrypted_a.allows_replay(Some(&encrypted_b)));
        assert!(!encrypted_a.allows_replay(Some(&ordinary)));
        assert!(!encrypted_a.allows_replay(None));
        assert!(ordinary.allows_replay(Some(&ordinary)));
        assert!(ordinary.allows_replay(None));
        assert!(!ordinary.allows_replay(Some(&encrypted_a)));
    }

    #[test]
    fn file_names_with_path_separators_are_sanitised_to_one_component() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(write(&spool, "season01/ep01.mkv", 1, "id@x", b"h", b"b"))
            .unwrap();
        // No subdirectory was implied by the '/' in the file name.
        assert!(!spool.join("season01").exists());
        assert!(read(&spool, "season01/ep01.mkv", 1).is_some());
    }
}
