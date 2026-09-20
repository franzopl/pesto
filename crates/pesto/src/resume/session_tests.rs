//! Tests for the encrypted-upload session-identity additions to
//! `ResumeState`: salt persistence, layout validation, and invalidation.
//! The upstream test module stays inline in `resume.rs`.

use super::*;

fn fp(article_size: u64) -> RunFingerprint {
    let mut fp = super::tests::fp(article_size);
    fp.encrypt_password = None;
    fp
}

fn names(file_names: &[&str]) -> Vec<String> {
    file_names.iter().map(|s| s.to_string()).collect()
}

#[test]
fn encrypt_password_fingerprint_invalidation() {
    let mut s = ResumeState::default();
    let mut fp1 = fp(768_000);
    fp1.encrypt_password = Some(hash_secret("pw1"));
    assert!(s.validate_run(&fp1));
    assert!(s.validate_run(&fp1));

    let mut fp2 = fp(768_000);
    fp2.encrypt_password = Some(hash_secret("pw2"));
    assert!(!s.validate_run(&fp2));
}

#[test]
fn fingerprint_mismatch_invalidates_session_identity_and_salt() {
    use crate::poster::ReleaseLayout;

    let mut s = ResumeState::default();
    let fp1 = fp(768_000);
    assert!(s.validate_run(&fp1));

    let salt = [0x55u8; 16];
    let layout = ReleaseLayout::from_parts(1, &[(1, 2)]).unwrap();
    s.set_session_identity(UploadSessionIdentity {
        salt,
        layout,
        source_names: names(&["a.bin"]),
    });
    s.record("a.bin", 1, "id1@x", 100);
    assert_eq!(s.session_salt(), Some(&salt));

    // Matching fingerprint preserves salt and identity.
    assert!(s.validate_run(&fp1));
    assert_eq!(s.session_salt(), Some(&salt));
    assert_eq!(s.len(), 1);

    // Parameter change (e.g. article size changed 768k -> 384k):
    // even if layout part count could stay the same on another file,
    // a fingerprint mismatch MUST invalidate the session salt to prevent nonce reuse .
    let fp2 = fp(384_000);
    assert!(!s.validate_run(&fp2));
    assert!(
        s.session_salt().is_none(),
        "salt must be cleared on fingerprint change"
    );
    assert!(
        s.take_session_invalidated(),
        "session_invalidated flag must be set for spool cleanup"
    );
    assert_eq!(s.len(), 0, "segments must be cleared");
}

#[test]
fn session_identity_round_trip() {
    use crate::poster::ReleaseLayout;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session_state.json");
    let mut state = ResumeState::default();

    let salt = [0x5au8; 16];
    let layout = ReleaseLayout::from_parts(2, &[(1, 2), (2, 3)]).unwrap();
    state.set_session_identity(UploadSessionIdentity {
        salt,
        layout: layout.clone(),
        source_names: names(&["file1.bin", "file2.bin"]),
    });

    state.record_with(
        "file1.bin",
        1,
        SegmentRecord {
            message_id: "msg1@example.com".into(),
            bytes: 1024,
            confirmed: true,
            check_disabled: false,
            server_idx: 1,
            wire_identity: None,
        },
    );

    state.save(&path).unwrap();

    // Serialized state contains no plaintext password.
    let serialized = std::fs::read_to_string(&path).unwrap();
    assert!(!serialized.contains("password"));
    assert!(!serialized.contains("secret"));

    // Load back: salt/layout/record identity all survive.
    let loaded = ResumeState::load(&path).unwrap();
    assert_eq!(loaded.session_salt(), Some(&salt));
    assert_eq!(
        loaded.session_identity.as_ref().map(|s| &s.layout),
        Some(&layout)
    );
    let rec = loaded.get("file1.bin", 1).unwrap();
    assert_eq!(rec.message_id, "msg1@example.com");
    assert_eq!(rec.bytes, 1024);

    // Repeated save/load is idempotent.
    let path2 = dir.path().join("session_state_2.json");
    loaded.save(&path2).unwrap();
    let mut loaded2 = ResumeState::load(&path2).unwrap();
    assert_eq!(loaded.session_salt(), loaded2.session_salt());
    assert_eq!(loaded.get("file1.bin", 1), loaded2.get("file1.bin", 1));

    // The unchanged layout still validates on the loaded state, and a
    // pre-schema record (no session identity) loads with identity unset.
    assert!(loaded2.validate_session(&layout, &names(&["file1.bin", "file2.bin"])));
    let json = r#"{"files":{},"segments":{"a.bin\u00001":{"message_id":"id1@x","bytes":100}}}"#;
    let loaded: ResumeState = serde_json::from_str(json).unwrap();
    assert!(loaded.session_salt().is_none());
    assert_eq!(loaded.get("a.bin", 1).unwrap().message_id, "id1@x");
}

#[test]
fn session_validation_invalidates_all_state_on_layout_mismatches() {
    use crate::poster::ReleaseLayout;

    let base_parts = &[(1, 2), (2, 3)];
    let base_layout = ReleaseLayout::from_parts(2, base_parts).unwrap();
    let salt = [0xabu8; 16];

    let make_state = || {
        let mut s = ResumeState::default();
        s.set_session_identity(UploadSessionIdentity {
            salt,
            layout: base_layout.clone(),
            source_names: names(&["a.bin", "b.bin"]),
        });
        s.set_archive_stem("arch_stem".into());
        s.set_release_identity("rel_prefix".into(), "rel_from".into());
        s.record_file(
            "a.bin",
            FileFingerprint {
                size: 200,
                mtime: Some(100),
            },
        );
        s.record_file(
            "b.bin",
            FileFingerprint {
                size: 300,
                mtime: Some(100),
            },
        );
        s
    };

    // Added file (total_files: 3 instead of 2)
    let mut s = make_state();
    let added_layout = ReleaseLayout::from_parts(3, &[(1, 2), (2, 3), (3, 1)]).unwrap();
    assert!(!s.validate_session(&added_layout, &names(&["a.bin", "b.bin", "c.bin"])));
    assert!(s.session_salt().is_none());
    assert!(s.archive_stem().is_none());
    assert!(s.release_identity().is_none());
    assert_eq!(s.len(), 0);
    assert!(s.file_matches(
        "a.bin",
        &FileFingerprint {
            size: 999,
            mtime: None
        }
    )); // files cleared

    // Removed file (total_files: 1 instead of 2)
    let mut s = make_state();
    let removed_layout = ReleaseLayout::from_parts(1, &[(1, 2)]).unwrap();
    assert!(!s.validate_session(&removed_layout, &names(&["a.bin"])));
    assert!(s.session_salt().is_none());
    assert_eq!(s.len(), 0);

    // Resized file (parts for file 1: 5 instead of 2)
    let mut s = make_state();
    let resized_layout = ReleaseLayout::from_parts(2, &[(1, 5), (2, 3)]).unwrap();
    assert!(!s.validate_session(&resized_layout, &names(&["a.bin", "b.bin"])));
    assert!(s.session_salt().is_none());
    assert_eq!(s.len(), 0);
}

/// T13 / PE-NEXT-1: a same-layout source replacement (same ordinals, same
/// part counts, different ordered file names) must invalidate the session
/// even though the count-only layout is equal.
#[test]
fn same_layout_source_replacement_invalidates_session() {
    use crate::poster::ReleaseLayout;

    let base_layout = ReleaseLayout::from_parts(2, &[(1, 2), (2, 3)]).unwrap();
    let salt = [0x77u8; 16];
    let mut s = ResumeState::default();
    s.set_session_identity(UploadSessionIdentity {
        salt,
        layout: base_layout.clone(),
        source_names: names(&["a.bin", "b.bin"]),
    });
    s.record("b.bin", 1, "id1@x", 100);
    assert!(s.validate_session(&base_layout, &names(&["a.bin", "b.bin"])));
    assert_eq!(s.session_salt(), Some(&salt), "unchanged identity is kept");

    // Replaced source: same layout, different ordered name list.
    assert!(!s.validate_session(&base_layout, &names(&["a.bin", "c.bin"])));
    assert!(s.session_salt().is_none(), "salt must rotate");
    assert_eq!(s.len(), 0, "stale segment records must be cleared");
    assert!(
        s.take_session_invalidated(),
        "spool cleanup flag must be set"
    );

    // Reordered names are also an identity change.
    let mut s = ResumeState::default();
    s.set_session_identity(UploadSessionIdentity {
        salt,
        layout: base_layout.clone(),
        source_names: names(&["a.bin", "b.bin"]),
    });
    assert!(!s.validate_session(&base_layout, &names(&["b.bin", "a.bin"])));
    assert!(s.session_salt().is_none());
}

/// T13 / PE-NEXT-1: an experimental saved session without persisted source
/// names must rotate the salt rather than trust a count-only layout match.
#[test]
fn pre_name_session_identity_rotates_rather_than_count_match() {
    use crate::poster::ReleaseLayout;

    let layout = ReleaseLayout::from_parts(1, &[(1, 2)]).unwrap();
    let salt = [0x11u8; 16];
    // Hand-craft a legacy identity without source names (as an
    // experimental state file would deserialize).
    let mut s = ResumeState::default();
    s.set_session_identity(UploadSessionIdentity {
        salt,
        layout: layout.clone(),
        source_names: Vec::new(),
    });
    s.record("a.bin", 1, "id1@x", 100);

    assert!(
        !s.validate_session(&layout, &names(&["a.bin"])),
        "a legacy identity without source names must not validate against any source set"
    );
    assert!(
        s.session_salt().is_none(),
        "legacy identity must rotate salt"
    );
    assert_eq!(s.len(), 0);
}

#[test]
fn no_check_record_never_sets_confirmed() {
    let mut s = ResumeState::default();
    s.record_with(
        "a.bin",
        1,
        SegmentRecord {
            message_id: "id@x".into(),
            bytes: 10,
            confirmed: false,
            check_disabled: true,
            server_idx: 0,
            wire_identity: None,
        },
    );
    let rec = s.get("a.bin", 1).unwrap();
    assert!(!rec.confirmed && rec.check_disabled);
}
