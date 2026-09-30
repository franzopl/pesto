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

use crate::resume::PersistedWireIdentity;

const V2_MAGIC: &[u8; 4] = b"PST2";
const V3_MAGIC: &[u8; 4] = b"PST3";

const MAX_METADATA_LEN: usize = 64 * 1024;
const MAX_ID_LEN: usize = 4096;
const MAX_HEADERS_LEN: usize = 64 * 1024;

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

/// Metadata object stored in PST3 spool envelopes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SpoolMetadata {
    #[serde(default)]
    pub wire_identity: Option<PersistedWireIdentity>,
    #[serde(default)]
    pub segment_identity: Option<crate::poster::SegmentIdentity>,
    #[serde(default)]
    pub session_salt: Option<[u8; 16]>,
    #[serde(default)]
    pub layout_fingerprint: Option<String>,
}

/// One spooled article, as read back for replay.
pub struct SpooledArticle {
    pub message_id: String,
    pub headers: Vec<u8>,
    pub body: Vec<u8>,
    pub wire_identity: Option<PersistedWireIdentity>,
    pub segment_identity: Option<crate::poster::SegmentIdentity>,
    pub session_salt: Option<[u8; 16]>,
    pub layout_fingerprint: Option<String>,
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
    write_inner(dir, file_name, part, message_id, headers, body, None).await
}

/// Versioned spool writer used by the poster. Unlike the legacy public
/// helper, this records the logical wire identity alongside the exact bytes
/// so resume metadata cannot disagree with a replayed article.
#[allow(dead_code)]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn write_with_identity(
    dir: &Path,
    file_name: &str,
    part: u32,
    message_id: &str,
    headers: &[u8],
    body: &[u8],
    wire_identity: &PersistedWireIdentity,
) -> Result<()> {
    write_inner(
        dir,
        file_name,
        part,
        message_id,
        headers,
        body,
        Some(wire_identity),
    )
    .await
}

/// Write a PST3 versioned spool entry containing exact article payload and
/// full session/layout/segment metadata.
pub async fn write_with_metadata(
    dir: &Path,
    file_name: &str,
    part: u32,
    message_id: &str,
    headers: &[u8],
    body: &[u8],
    metadata: &SpoolMetadata,
) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating spool directory `{}`", dir.display()))?;

    let meta_json = serde_json::to_vec(metadata).context("serialising spool metadata")?;
    let mut buf = Vec::with_capacity(
        V3_MAGIC.len()
            + 4
            + meta_json.len()
            + 4
            + message_id.len()
            + 4
            + headers.len()
            + body.len(),
    );
    buf.extend_from_slice(V3_MAGIC);
    buf.extend_from_slice(&(meta_json.len() as u32).to_le_bytes());
    buf.extend_from_slice(&meta_json);
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

#[allow(clippy::too_many_arguments)]
async fn write_inner(
    dir: &Path,
    file_name: &str,
    part: u32,
    message_id: &str,
    headers: &[u8],
    body: &[u8],
    wire_identity: Option<&PersistedWireIdentity>,
) -> Result<()> {
    tokio::fs::create_dir_all(dir)
        .await
        .with_context(|| format!("creating spool directory `{}`", dir.display()))?;
    // PST2 layout: [magic][u32 identity_len][identity JSON], followed by the
    // legacy [u32 id_len][id][u32 headers_len][headers][body] payload. The
    // body is raw (non-UTF-8) yEnc bytes, so the small binary envelope avoids
    // copying it through a general-purpose serialization format.
    let identity = wire_identity
        .map(serde_json::to_vec)
        .transpose()
        .context("serialising spool wire identity")?;
    let identity_len = identity.as_ref().map_or(0, Vec::len);
    let mut buf = Vec::with_capacity(
        V2_MAGIC.len() + 4 + identity_len + 4 + message_id.len() + 4 + headers.len() + body.len(),
    );
    if let Some(identity) = identity {
        buf.extend_from_slice(V2_MAGIC);
        buf.extend_from_slice(&(identity.len() as u32).to_le_bytes());
        buf.extend_from_slice(&identity);
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
    let (wire_identity, segment_identity, session_salt, layout_fingerprint) =
        if buf.starts_with(V3_MAGIC) {
            pos += V3_MAGIC.len();
            if buf.len() < pos + 4 {
                return None;
            }
            let meta_len = u32::from_le_bytes(buf.get(pos..pos + 4)?.try_into().ok()?) as usize;
            pos += 4;
            if meta_len > MAX_METADATA_LEN || buf.len() < pos + meta_len {
                return None;
            }
            let meta: SpoolMetadata = serde_json::from_slice(buf.get(pos..pos + meta_len)?).ok()?;
            pos += meta_len;
            (
                meta.wire_identity,
                meta.segment_identity,
                meta.session_salt,
                meta.layout_fingerprint,
            )
        } else if buf.starts_with(V2_MAGIC) {
            pos += V2_MAGIC.len();
            if buf.len() < pos + 4 {
                return None;
            }
            let identity_len = u32::from_le_bytes(buf.get(pos..pos + 4)?.try_into().ok()?) as usize;
            pos += 4;
            if identity_len > MAX_METADATA_LEN || buf.len() < pos + identity_len {
                return None;
            }
            let identity: PersistedWireIdentity =
                serde_json::from_slice(buf.get(pos..pos + identity_len)?).ok()?;
            pos += identity_len;
            (Some(identity), None, None, None)
        } else {
            (None, None, None, None)
        };

    if buf.len() < pos + 4 {
        return None;
    }
    let id_len = u32::from_le_bytes(buf.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    if id_len > MAX_ID_LEN || buf.len() < pos + id_len {
        return None;
    }
    let message_id = String::from_utf8(buf.get(pos..pos + id_len)?.to_vec()).ok()?;
    pos += id_len;

    if buf.len() < pos + 4 {
        return None;
    }
    let headers_len = u32::from_le_bytes(buf.get(pos..pos + 4)?.try_into().ok()?) as usize;
    pos += 4;
    if headers_len > MAX_HEADERS_LEN || buf.len() < pos + headers_len {
        return None;
    }
    let headers = buf.get(pos..pos + headers_len)?.to_vec();
    pos += headers_len;

    let body = buf.get(pos..)?.to_vec();

    Some(SpooledArticle {
        message_id,
        headers,
        body,
        wire_identity,
        segment_identity,
        session_salt,
        layout_fingerprint,
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
            ))
            .unwrap();

        let entry = read(&spool, "movie.bin", 1).unwrap();
        assert_eq!(entry.wire_identity, Some(identity));
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

    #[test]
    fn pst3_versioned_spool_round_trips_exact_identity() {
        use crate::poster::SegmentIdentity;

        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        let wire_id = PersistedWireIdentity {
            subject_name: "test-subj".into(),
            yenc_name: "test-yenc".into(),
            from: "sender <s@x>".into(),
            date: Some("Thu, 24 Sep 2026 00:00:00 GMT".into()),
            unix_date: Some(1790208000),
        };
        let seg_id = SegmentIdentity::checked(10, 2, 5, 3).unwrap();
        let salt = [0x42u8; 16];
        let layout_fp = "d3b07384d113edec49eaa6238ad5ff00";

        let meta = SpoolMetadata {
            wire_identity: Some(wire_id.clone()),
            segment_identity: Some(seg_id),
            session_salt: Some(salt),
            layout_fingerprint: Some(layout_fp.to_string()),
        };

        tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(write_with_metadata(
                &spool,
                "data.bin",
                3,
                "msg3@example.com",
                b"headers-pst3",
                b"body-pst3\x00\xff",
                &meta,
            ))
            .unwrap();

        let entry = read(&spool, "data.bin", 3).unwrap();
        assert_eq!(entry.message_id, "msg3@example.com");
        assert_eq!(entry.headers, b"headers-pst3");
        assert_eq!(entry.body, b"body-pst3\x00\xff");
        assert_eq!(entry.wire_identity, Some(wire_id));
        assert_eq!(entry.segment_identity, Some(seg_id));
        assert_eq!(entry.session_salt, Some(salt));
        assert_eq!(entry.layout_fingerprint.as_deref(), Some(layout_fp));
    }

    #[test]
    fn legacy_pst2_and_pre_magic_remain_readable_with_new_fields_absent() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        let rt = tokio::runtime::Runtime::new().unwrap();

        // 1. Pre-magic legacy entry
        rt.block_on(write(
            &spool,
            "legacy.bin",
            1,
            "leg@example.com",
            b"head-leg",
            b"body-leg",
        ))
        .unwrap();

        let leg_entry = read(&spool, "legacy.bin", 1).unwrap();
        assert_eq!(leg_entry.message_id, "leg@example.com");
        assert_eq!(leg_entry.headers, b"head-leg");
        assert_eq!(leg_entry.body, b"body-leg");
        assert!(leg_entry.wire_identity.is_none());
        assert!(leg_entry.segment_identity.is_none());
        assert!(leg_entry.session_salt.is_none());
        assert!(leg_entry.layout_fingerprint.is_none());

        // 2. PST2 legacy entry
        let wire_id = PersistedWireIdentity {
            subject_name: "pst2-subj".into(),
            yenc_name: "pst2-yenc".into(),
            from: "pst2 <p@x>".into(),
            date: None,
            unix_date: None,
        };
        rt.block_on(write_with_identity(
            &spool,
            "pst2.bin",
            2,
            "pst2@example.com",
            b"head-pst2",
            b"body-pst2",
            &wire_id,
        ))
        .unwrap();

        let pst2_entry = read(&spool, "pst2.bin", 2).unwrap();
        assert_eq!(pst2_entry.message_id, "pst2@example.com");
        assert_eq!(pst2_entry.wire_identity, Some(wire_id));
        assert!(pst2_entry.segment_identity.is_none());
        assert!(pst2_entry.session_salt.is_none());
        assert!(pst2_entry.layout_fingerprint.is_none());
    }

    #[test]
    fn corrupt_and_oversized_metadata_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let spool = dir.path().join("release.pesto-spool");
        std::fs::create_dir_all(&spool).unwrap();

        // Truncated PST3 length
        let path1 = entry_path(&spool, "trunc.bin", 1);
        std::fs::write(&path1, b"PST3\x05\x00\x00").unwrap();
        assert!(read(&spool, "trunc.bin", 1).is_none());

        // Oversized PST3 metadata length (> 1 MB limit)
        let path2 = entry_path(&spool, "big.bin", 1);
        let mut buf = Vec::new();
        buf.extend_from_slice(b"PST3");
        buf.extend_from_slice(&(2 * 1024 * 1024u32).to_le_bytes()); // 2MB length claim
        std::fs::write(&path2, buf).unwrap();
        assert!(read(&spool, "big.bin", 1).is_none());

        // Invalid JSON inside PST3
        let path3 = entry_path(&spool, "bad_json.bin", 1);
        let mut buf = Vec::new();
        buf.extend_from_slice(b"PST3");
        buf.extend_from_slice(&(4u32).to_le_bytes());
        buf.extend_from_slice(b"{bad");
        buf.extend_from_slice(&(1u32).to_le_bytes());
        buf.extend_from_slice(b"x");
        buf.extend_from_slice(&(1u32).to_le_bytes());
        buf.extend_from_slice(b"h");
        buf.extend_from_slice(b"b");
        std::fs::write(&path3, buf).unwrap();
        assert!(read(&spool, "bad_json.bin", 1).is_none());
    }
}
