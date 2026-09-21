use super::queue::queue_entry_info_quick;
use super::{apply_indexer_field, queue_entry_info};
use std::fs;

mod watch_scan {
    use super::super::{fold_watch_scan, WatchState};
    use std::path::PathBuf;

    fn p(name: &str) -> PathBuf {
        PathBuf::from(format!("/watch/{name}"))
    }

    /// The first scan of a directory only baselines it: every entry
    /// already present is marked seen but nothing is queued, matching
    /// the "ignore what already exists" behavior watch mode promises.
    #[test]
    fn first_scan_baselines_without_queuing() {
        let mut w = WatchState::default();
        let lines = fold_watch_scan(&mut w, &[(p("a.mkv"), 100), (p("b.mkv"), 200)]);

        assert!(w.baseline_captured);
        assert!(w.ready.is_empty());
        assert!(w.pending.is_empty());
        assert!(w.seen.contains(&p("a.mkv")));
        assert!(w.seen.contains(&p("b.mkv")));
        assert_eq!(lines.len(), 1);
        assert!(lines[0].0.contains("baseline captured"));
    }

    /// A brand-new entry (after baselining) is tracked as pending on its
    /// first sighting, then queued only once its size repeats unchanged.
    #[test]
    fn new_entry_queues_once_size_is_stable_across_two_scans() {
        let mut w = WatchState::default();
        fold_watch_scan(&mut w, &[]); // baseline: nothing pre-existing

        fold_watch_scan(&mut w, &[(p("new.mkv"), 100)]);
        assert!(w.ready.is_empty(), "queued before stabilizing");
        assert_eq!(w.pending.get(&p("new.mkv")), Some(&100));

        fold_watch_scan(&mut w, &[(p("new.mkv"), 100)]);
        assert_eq!(w.ready.into_iter().collect::<Vec<_>>(), vec![p("new.mkv")]);
        assert!(w.pending.is_empty());
        assert!(w.seen.contains(&p("new.mkv")));
    }

    /// A still-growing file must never be queued: each differing size
    /// just re-arms the settle check instead.
    #[test]
    fn still_changing_size_is_never_queued() {
        let mut w = WatchState::default();
        fold_watch_scan(&mut w, &[]);

        fold_watch_scan(&mut w, &[(p("f.mkv"), 100)]);
        fold_watch_scan(&mut w, &[(p("f.mkv"), 150)]);
        fold_watch_scan(&mut w, &[(p("f.mkv"), 200)]);

        assert!(w.ready.is_empty());
        assert_eq!(w.pending.get(&p("f.mkv")), Some(&200));
    }

    /// A stable empty file/dir is ignored (marked seen) rather than
    /// queued for a pointless upload.
    #[test]
    fn stable_empty_entry_is_ignored_not_queued() {
        let mut w = WatchState::default();
        fold_watch_scan(&mut w, &[]);

        fold_watch_scan(&mut w, &[(p("empty.txt"), 0)]);
        let lines = fold_watch_scan(&mut w, &[(p("empty.txt"), 0)]);

        assert!(w.ready.is_empty());
        assert!(w.seen.contains(&p("empty.txt")));
        assert!(lines.iter().any(|(_, is_warn)| *is_warn));
    }

    /// An entry inside `done_dir` (watch mode's own output) must never
    /// be picked up, even once stable — otherwise a move-to-done would
    /// feed straight back into the watch loop.
    #[test]
    fn entries_inside_done_dir_are_never_queued() {
        let mut w = WatchState {
            done_dir: Some(PathBuf::from("/watch/done")),
            ..Default::default()
        };
        fold_watch_scan(&mut w, &[]);

        fold_watch_scan(&mut w, &[(PathBuf::from("/watch/done/old.mkv"), 100)]);
        fold_watch_scan(&mut w, &[(PathBuf::from("/watch/done/old.mkv"), 100)]);

        assert!(w.ready.is_empty());
        assert!(w.pending.is_empty());
    }

    /// An entry that disappears before settling (e.g. renamed or
    /// deleted) drops out of `pending` instead of lingering forever.
    #[test]
    fn vanished_entry_is_dropped_from_pending() {
        let mut w = WatchState::default();
        fold_watch_scan(&mut w, &[]);

        fold_watch_scan(&mut w, &[(p("gone.mkv"), 100)]);
        assert!(w.pending.contains_key(&p("gone.mkv")));

        fold_watch_scan(&mut w, &[]);
        assert!(w.pending.is_empty());
    }
}

fn indexer_str(doc_text: &str, field: &str) -> Option<String> {
    let doc = doc_text
        .parse::<toml_edit::DocumentMut>()
        .expect("output is valid TOML");
    // Read with `get` chaining: indexing a regular `Table` with a missing
    // key panics, and a removed field is legitimately absent.
    doc.get("output")
        .and_then(|o| o.get("indexer"))
        .and_then(|i| i.get(field))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Writing into an empty (or missing) config creates `[output.indexer]`
/// with the field, and the result is valid TOML.
#[test]
fn indexer_field_written_into_empty_config() {
    let out = apply_indexer_field("", "url", Some("http://localhost:9696")).unwrap();
    assert_eq!(
        indexer_str(&out, "url").as_deref(),
        Some("http://localhost:9696")
    );
    // No bare empty `[output]` header should precede the nested table.
    assert!(
        !out.contains("[output]\n"),
        "unexpected bare header:\n{out}"
    );
}

/// Existing keys and comments elsewhere in the file are preserved, and a new
/// `[output.indexer]` field is added alongside an existing one.
#[test]
fn indexer_field_preserves_rest_of_config() {
    let original = "\
# my config
[server]
host = \"news.example.com\" # keep me

[output]
nzb_dir = \"~/nzb\"

[output.indexer]
url = \"http://old:9696\"
";
    let out = apply_indexer_field(original, "api_key", Some("secret123")).unwrap();
    // Comment and unrelated keys survive verbatim.
    assert!(out.contains("# my config"));
    assert!(out.contains("host = \"news.example.com\" # keep me"));
    assert!(out.contains("nzb_dir = \"~/nzb\""));
    // Both the pre-existing url and the new api_key are present.
    assert_eq!(indexer_str(&out, "url").as_deref(), Some("http://old:9696"));
    assert_eq!(indexer_str(&out, "api_key").as_deref(), Some("secret123"));
}

/// Writing the same field twice updates in place rather than duplicating it.
#[test]
fn indexer_field_updates_in_place() {
    let step1 = apply_indexer_field("", "url", Some("http://a:1")).unwrap();
    let step2 = apply_indexer_field(&step1, "url", Some("http://b:2")).unwrap();
    assert_eq!(
        step2.matches("url =").count(),
        1,
        "url duplicated:\n{step2}"
    );
    assert_eq!(indexer_str(&step2, "url").as_deref(), Some("http://b:2"));
}

/// A `None` value removes the field (clearing it in the Config screen).
#[test]
fn indexer_field_removed_when_none() {
    let with = apply_indexer_field("", "api_key", Some("secret")).unwrap();
    let without = apply_indexer_field(&with, "api_key", None).unwrap();
    assert_eq!(indexer_str(&without, "api_key"), None);
}

/// The quick form must not walk a directory: it returns immediately with
/// `sized: false` and zeroed counts so the UI thread never blocks. The full
/// form then fills in the real numbers via the recursive walk.
#[test]
fn quick_info_defers_folder_sizing() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    fs::create_dir(&sub).unwrap();
    fs::write(dir.path().join("a.bin"), [0u8; 100]).unwrap();
    fs::write(sub.join("b.bin"), [0u8; 200]).unwrap();
    let path = dir.path().to_string_lossy().to_string();

    let quick = queue_entry_info_quick(&path);
    assert!(quick.is_dir);
    assert!(!quick.sized);
    assert_eq!(quick.file_count, 0);
    assert_eq!(quick.size_bytes, 0);
    assert_eq!(quick.files_label(), "…");

    // The full form walks the tree (2 files, 300 bytes) and is marked sized.
    let full = queue_entry_info(&path);
    assert!(full.sized);
    assert_eq!(full.file_count, 2);
    assert_eq!(full.size_bytes, 300);
    assert_eq!(full.files_label(), "2");
}

/// The aggregate bar must track each queue item, not stay pinned at the
/// previous item's 100%. `apply` only grows the counters within one item, so
/// `reset_for_item` is what lets the next item's smaller counts show.
#[test]
fn progress_bar_tracks_each_queue_item() {
    use super::UploadProgress;
    use crate::events::ProgressUpdate;

    fn upd(done_segments: u64, total_segments: u64) -> ProgressUpdate {
        ProgressUpdate {
            done_segments,
            total_segments,
            done_bytes: done_segments * 1000,
            total_bytes: total_segments * 1000,
            current_speed_mbps: 0.0,
            message: None,
            file_update: None,
            phase: None,
            par2_slices: None,
            check_progress: None,
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
        }
    }

    let mut p = UploadProgress::default();
    // Item 1 runs to completion.
    p.apply(&upd(100, 100));
    assert_eq!(p.done_segments, 100);
    assert_eq!(p.total_segments, 100);

    // Without the reset, item 2's smaller counts (5 < 100) would be ignored
    // by the monotonic apply and the bar would stay at 100%.
    p.reset_for_item();
    assert_eq!(p.done_segments, 0);
    assert_eq!(p.total_segments, 0);

    p.apply(&upd(5, 50));
    assert_eq!(p.done_segments, 5);
    assert_eq!(p.total_segments, 50);
    assert!((p.progress_pct() - 10.0).abs() < 1e-9);
}

/// A plain file is fully resolved by the quick form (a single `stat`), so it
/// never needs a background job.
#[test]
fn quick_info_resolves_plain_file() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("movie.mkv");
    fs::write(&file, [0u8; 42]).unwrap();

    let info = queue_entry_info_quick(&file.to_string_lossy());
    assert!(!info.is_dir);
    assert!(info.sized);
    assert_eq!(info.nzb_name, "movie");
    assert_eq!(info.size_bytes, 42);
}
