use super::render::fmt_bytes;
use super::{release_key, FileTree};
use std::fs;

#[test]
fn release_key_matches_media_and_nzb_names() {
    // The reported case: a media file and its sibling .nzb (named after the
    // release, no media extension) must produce the same key.
    let media = "Zootopia.2016.1080p.DSNP.WEB-DL.DDP5.1.H.264.DUAL-cza.mkv";
    let nzb = "Zootopia.2016.1080p.DSNP.WEB-DL.DDP5.1.H.264.DUAL-cza.nzb";
    assert_eq!(release_key(media), release_key(nzb));
    assert!(!release_key(media).is_empty());
}

#[test]
fn release_key_handles_upapasta_timestamp_and_double_extension() {
    // upapasta-generated NZBs carry a timestamp prefix and keep the media
    // extension before `.nzb`: `<ts>_<release>.mkv.nzb`.
    let media = "Zootopia.2.2025.1080p.DSNP.WEB-DL.DDP5.1.H.264.DUAL-BiOMA.mkv";
    let nzb = "20260427T151003Z_Zootopia.2.2025.1080p.DSNP.WEB-DL.DDP5.1.H.264.DUAL-BiOMA.mkv.nzb";
    assert_eq!(release_key(media), release_key(nzb));
}

#[test]
fn release_key_keeps_codec_and_group_tags() {
    // A trailing alphanumeric tag like `x264` is NOT an extension we strip,
    // so two distinct releases stay distinct.
    assert_ne!(
        release_key("Movie.2024.1080p.x264-AAA.mkv"),
        release_key("Movie.2024.1080p.x264-BBB.mkv")
    );
}

#[test]
fn fmt_bytes_scales_units() {
    assert_eq!(fmt_bytes(0), "0 B");
    assert_eq!(fmt_bytes(512), "512 B");
    assert_eq!(fmt_bytes(1024), "1.0 KB");
    assert_eq!(fmt_bytes(1536), "1.5 KB");
    assert_eq!(fmt_bytes(3 * 1024 * 1024 * 1024), "3.0 GB");
}

/// refresh() must stay off the filesystem-walk path: a scan is pending and
/// the summary is not ready until the background job is applied.
#[test]
fn refresh_defers_summary_to_background_scan() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.bin"), [0u8; 100]).unwrap();
    fs::write(dir.path().join("b.bin"), [0u8; 50]).unwrap();

    let mut tree = FileTree::new();
    tree.current_dir = dir.path().to_path_buf();
    tree.refresh();

    // Listing is available immediately, but the backed/size numbers are not.
    assert_eq!(tree.items.len(), 2);
    assert!(!tree.summary_ready);

    // Run the deferred job (as the blocking worker would) and fold it back.
    let job = tree.take_scan_job().expect("a scan should be pending");
    let (generation, results) = job.run();
    tree.apply_scan(generation, results);

    assert!(tree.summary_ready);
    // Nothing in the (empty) catalog, so both files are unbacked: 2 items,
    // 2 unbacked, 150 bytes to upload.
    assert_eq!(tree.summary(), (2, 2, 150));
}

/// A scan whose generation was superseded (e.g. the user navigated away)
/// must be discarded rather than overwriting the current directory's state.
#[test]
fn stale_scan_is_ignored() {
    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.bin"), [0u8; 10]).unwrap();

    let mut tree = FileTree::new();
    tree.current_dir = dir.path().to_path_buf();
    tree.refresh();
    let job = tree.take_scan_job().expect("a scan should be pending");
    let (stale_gen, results) = job.run();

    // Simulate navigating away before the scan returned.
    tree.refresh();
    tree.apply_scan(stale_gen, results);

    // The stale result was dropped, so we are still waiting on the fresh one.
    assert!(!tree.summary_ready);
    assert!(tree.scan_pending);
}
