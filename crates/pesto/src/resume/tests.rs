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
        par2_effort: 1.0,
        compress_volume_size: None,
        compress_password: None,
        line_length: 128,
        par2_layout: 0,
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

    let mut layout = base.clone();
    layout.par2_layout = PAR2_LAYOUT;
    assert_ne!(base, layout);

    let mut effort = base.clone();
    effort.par2_effort = 2.0;
    assert_ne!(base, effort);
}

#[test]
fn par2_state_from_an_older_layout_is_discarded() {
    // A fingerprint serialized by a pesto that predates the field.
    let mut old = serde_json::to_value(RunFingerprint {
        par2_percent: 10,
        ..fp(768_000)
    })
    .unwrap();
    old.as_object_mut().unwrap().remove("par2_layout");
    old.as_object_mut().unwrap().remove("par2_effort");
    let json = serde_json::json!({ "fingerprint": old, "files": {}, "segments": {} });
    let mut state: ResumeState = serde_json::from_value(json).unwrap();
    assert_eq!(
        state.fingerprint.as_ref().map(|f| f.par2_effort),
        Some(crate::config::DEFAULT_PAR2_EFFORT)
    );
    state.record("a.bin.vol00+01.par2", 1, "old@x", 10);

    let current = RunFingerprint {
        par2_percent: 10,
        par2_layout: PAR2_LAYOUT,
        ..fp(768_000)
    };
    assert!(!state.validate_run(&current));
    assert!(state.get("a.bin.vol00+01.par2", 1).is_none());
}

#[test]
fn state_without_par2_survives_the_layout_field() {
    let mut old = serde_json::to_value(fp(768_000)).unwrap();
    old.as_object_mut().unwrap().remove("par2_layout");
    let json = serde_json::json!({ "fingerprint": old, "files": {}, "segments": {} });
    let mut state: ResumeState = serde_json::from_value(json).unwrap();
    assert!(state.validate_run(&fp(768_000)));
}

#[test]
fn pre_schema_json_defaults_new_fields() {
    let json = r#"{"files":{},"segments":{"a.bin\u00001":{"message_id":"id1@x","bytes":100}}}"#;
    let loaded: ResumeState = serde_json::from_str(json).unwrap();
    let rec = loaded.get("a.bin", 1).unwrap();
    assert_eq!(rec.message_id, "id1@x");
    assert_eq!(rec.bytes, 100);
    assert!(!rec.confirmed);
    assert!(!rec.check_disabled);
    assert_eq!(rec.server_idx, 0);
}

#[test]
fn mark_confirmed_flips_only_that_record() {
    let mut s = ResumeState::default();
    s.record_with(
        "a.bin",
        1,
        SegmentRecord {
            message_id: "id1@x".into(),
            bytes: 100,
            confirmed: false,
            check_disabled: true,
            server_idx: 2,
            wire_identity: None,
        },
    );
    s.record("a.bin", 2, "id2@x", 50);
    s.mark_confirmed("a.bin", 1);
    let rec = s.get("a.bin", 1).unwrap();
    assert!(rec.confirmed);
    assert!(!rec.check_disabled);
    assert_eq!(rec.server_idx, 2);
    assert!(!s.get("a.bin", 2).unwrap().confirmed);
}

#[test]
fn resume_action_three_arms() {
    let confirmed = SegmentRecord {
        message_id: "id@x".into(),
        bytes: 1,
        confirmed: true,
        check_disabled: false,
        server_idx: 0,
        wire_identity: None,
    };
    let unconfirmed = SegmentRecord {
        confirmed: false,
        ..confirmed.clone()
    };
    let check_off = SegmentRecord {
        confirmed: false,
        check_disabled: true,
        ..confirmed.clone()
    };
    assert_eq!(resume_action(true, None), ResumeAction::Post);
    assert_eq!(resume_action(false, None), ResumeAction::Post);
    assert_eq!(resume_action(false, Some(&unconfirmed)), ResumeAction::Skip);
    assert_eq!(resume_action(false, Some(&check_off)), ResumeAction::Skip);
    assert_eq!(resume_action(false, Some(&confirmed)), ResumeAction::Skip);
    assert_eq!(resume_action(true, Some(&confirmed)), ResumeAction::Skip);
    assert_eq!(
        resume_action(true, Some(&unconfirmed)),
        ResumeAction::ReStatStoredId
    );
    assert_eq!(
        resume_action(true, Some(&check_off)),
        ResumeAction::ReStatStoredId
    );
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
