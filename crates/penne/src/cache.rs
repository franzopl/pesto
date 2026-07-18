//! On-disk cache of fetched article bodies, keyed by Message-ID — the
//! resumable unit for `ROADMAP.md` Phase 8. If `penne download` is killed
//! partway through a large queue, a segment already fetched and cached
//! doesn't get re-downloaded on the next run: [`crate::download::download_queue`]
//! checks the cache before making any network request.
//!
//! Stores the *raw* (still yEnc-encoded) bytes exactly as fetched over
//! NNTP, not a decoded [`pesto::yenc::DecodedPart`] — decoding is cheap, so
//! the cache format never needs to track `pesto::yenc`'s internal
//! representation, and a cache entry is byte-for-byte what a real re-fetch
//! from any server holding the article would return.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// Directory name (under a download's destination) used to cache fetched
/// article bodies across runs.
pub const CACHE_DIR_NAME: &str = ".penne-cache";

/// Path within `dest_dir`'s cache directory for `message_id`.
pub fn cache_path(dest_dir: &Path, message_id: &str) -> PathBuf {
    dest_dir.join(CACHE_DIR_NAME).join(sanitize(message_id))
}

/// Read a previously cached body, if present.
pub fn load(dest_dir: &Path, message_id: &str) -> Option<Vec<u8>> {
    std::fs::read(cache_path(dest_dir, message_id)).ok()
}

/// Write a freshly fetched body to the cache, for a future run to resume
/// from if this one is interrupted.
pub fn store(dest_dir: &Path, message_id: &str, body: &[u8]) -> Result<()> {
    let path = cache_path(dest_dir, message_id);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))
}

/// Remove the whole cache directory. Call once a download completes
/// successfully (every file assembled, and PAR2-clean or repaired) so
/// cached bodies don't accumulate forever once they're no longer needed —
/// their only purpose was resuming *this* download.
pub fn clear(dest_dir: &Path) -> Result<()> {
    let dir = dest_dir.join(CACHE_DIR_NAME);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).with_context(|| format!("removing {}", dir.display()))?;
    }
    Ok(())
}

/// Turn a Message-ID into a filesystem-safe, collision-free file name.
///
/// Message-IDs are `local-part@domain` and RFC 5322's `atext`/quoted forms
/// allow characters that are not safe (or not even valid) in a file name —
/// so this percent-encodes anything outside `[A-Za-z0-9._-]` rather than
/// trying to enumerate which characters happen to be safe. Percent-encoding
/// is a bijection (distinct inputs always produce distinct output, since the
/// escape character `%` is itself escaped), which a hash would not
/// guarantee — a hash collision between two different Message-IDs would
/// silently serve the wrong cached article.
fn sanitize(message_id: &str) -> String {
    let id = message_id.trim_start_matches('<').trim_end_matches('>');
    let mut out = String::with_capacity(id.len() + 5);
    for b in id.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'-' | b'_' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02x}")),
        }
    }
    out.push_str(".yenc");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), "art1@test", b"hello yenc").unwrap();
        assert_eq!(load(dir.path(), "art1@test"), Some(b"hello yenc".to_vec()));
    }

    #[test]
    fn load_missing_entry_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path(), "never-cached@test"), None);
    }

    #[test]
    fn angle_brackets_do_not_change_the_cache_key() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), "<art1@test>", b"body").unwrap();
        assert_eq!(load(dir.path(), "art1@test"), Some(b"body".to_vec()));
    }

    #[test]
    fn distinct_message_ids_never_collide() {
        // Two IDs that would hash identically under a naive weak hash must
        // still map to distinct files; percent-encoding guarantees this
        // regardless of the specific IDs involved.
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), "a@test", b"AAAA").unwrap();
        store(dir.path(), "b@test", b"BBBB").unwrap();
        assert_eq!(load(dir.path(), "a@test"), Some(b"AAAA".to_vec()));
        assert_eq!(load(dir.path(), "b@test"), Some(b"BBBB".to_vec()));
        assert_ne!(
            cache_path(dir.path(), "a@test"),
            cache_path(dir.path(), "b@test")
        );
    }

    #[test]
    fn sanitize_escapes_filesystem_unsafe_characters() {
        // A message-id containing a literal slash must not be interpreted
        // as a path separator.
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), "weird/id@test", b"x").unwrap();
        assert_eq!(load(dir.path(), "weird/id@test"), Some(b"x".to_vec()));
        assert!(cache_path(dir.path(), "weird/id@test")
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains("%2f"));
    }

    #[test]
    fn clear_removes_the_cache_directory() {
        let dir = tempfile::tempdir().unwrap();
        store(dir.path(), "art1@test", b"x").unwrap();
        assert!(dir.path().join(CACHE_DIR_NAME).exists());
        clear(dir.path()).unwrap();
        assert!(!dir.path().join(CACHE_DIR_NAME).exists());
    }

    #[test]
    fn clear_on_a_directory_with_no_cache_is_a_no_op() {
        let dir = tempfile::tempdir().unwrap();
        clear(dir.path()).unwrap();
    }
}
