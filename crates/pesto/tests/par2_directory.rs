//! Phase 9b: a PAR2 set generated for a directory upload must let
//! `par2cmdline` rebuild the original nested directory layout, not just a
//! flat list of files.

use std::process::Command;

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files;
use pesto::walk::expand_inputs;

/// Deterministic, mutually independent pseudo-random bytes for one test file.
/// Each byte is a splitmix64 hash of `(seed, index)`, so the files share no
/// shift relationship that could defeat `par2cmdline`'s block scanner.
fn content(seed: u8) -> Vec<u8> {
    (0..200_000u64)
        .map(|i| {
            let mut z = i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (seed as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z >> 33) as u8
        })
        .collect()
}

/// A release that contains a 0-byte file must still produce a PAR2 set that
/// `par2cmdline` accepts. The 0-byte file contributes no input slices but must
/// appear in the File Description and IFSC packets with correct lengths.
#[tokio::test(flavor = "multi_thread")]
async fn par2_zero_byte_file_verifies() {
    let root = std::env::temp_dir().join(format!("pesto_zero_byte_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let show = root.join("Show");
    std::fs::create_dir_all(&show).unwrap();

    // Three files: one 0-byte file sandwiched between two normal files.
    std::fs::write(show.join("aaa.bin"), content(0)).unwrap();
    std::fs::write(show.join("bbb_empty.bin"), b"").unwrap(); // 0 bytes
    std::fs::write(show.join("ccc.bin"), content(1)).unwrap();

    let config = Config {
        host: "unused".to_string(),
        port: 563,
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 65536,
        line_length: 128,
        retries: 1,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 100,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: Some(1_000_000_000),
        par2_only: true,
        threads: 0,
        simd: pesto::par2::SimdPath::Auto,
        extra_servers: vec![],
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        nzb_tags: vec![],
        tmdb_id: None,
        tmdb_kind: None,
        imdb_id: None,
        tvdb_id: None,
        mal_id: None,
        indexer_url: None,
        indexer_api_key: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        history: true,
        history_dir: None,
        nzb_dir: None,
        date: None,
        no_archive: false,
        message_id_domain: None,
        pre_hooks: vec![],
        post_hooks: vec![],
        no_hooks: false,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 30,
        check_retries: 2,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        pipeline_depth: 1,
        keepalive_interval: 0,
    };

    let inputs = expand_inputs(std::slice::from_ref(&show)).unwrap();
    post_files(&config, &inputs).await.unwrap();

    // Move PAR2 files into Show/ (simulating a download client).
    assert!(root.join("Show.par2").exists());
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("par2") {
            let dest = show.join(path.file_name().unwrap());
            std::fs::rename(&path, dest).unwrap();
        }
    }

    // Delete a non-empty file that sorts AFTER bbb_empty.bin in File ID order.
    // Before the 0-byte fix, ccc.bin would get the wrong file_len in the PAR2
    // (the length of the next file in the hasher's output), making repair fail.
    std::fs::remove_file(show.join("ccc.bin")).unwrap();

    let repair = Command::new("par2")
        .args(["repair", "-q", "Show.par2"])
        .current_dir(&show)
        .output();
    match repair {
        Err(_) => {
            eprintln!("par2cmdline not found, skipping");
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        Ok(out) if out.status.code() == Some(127) => {
            eprintln!("par2cmdline not found (exit 127), skipping");
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        Ok(_) => {}
    }

    // ccc.bin must be restored correctly regardless of par2's exit code
    // (par2cmdline exits non-zero when 0-byte files are present, even after
    // successful repair, because it cannot verify them via checksums).
    assert_eq!(
        std::fs::read(show.join("ccc.bin")).unwrap(),
        content(1),
        "ccc.bin was not restored correctly after repair"
    );

    std::fs::remove_dir_all(&root).ok();
}

// The PAR2 encoder uses `block_in_place`, which needs a multi-thread runtime.
#[tokio::test(flavor = "multi_thread")]
async fn par2_only_directory_repair_recreates_tree() {
    let root = std::env::temp_dir().join(format!("pesto_dir_par2_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let show = root.join("Show");
    std::fs::create_dir_all(show.join("extras")).unwrap();

    // Three files spread over two directory levels.
    let files = [
        ("Show/ep01.bin", 0u8),
        ("Show/ep02.bin", 1u8),
        ("Show/extras/clip.bin", 2u8),
    ];
    for (rel, seed) in files {
        std::fs::write(root.join(rel), content(seed)).unwrap();
    }

    let config = Config {
        host: "unused".to_string(),
        port: 563,
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 65536,
        line_length: 128,
        retries: 1,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 100,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: Some(1_000_000_000),
        par2_only: true,
        threads: 0,
        simd: pesto::par2::SimdPath::Auto,
        extra_servers: vec![],
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        nzb_tags: vec![],
        tmdb_id: None,
        tmdb_kind: None,
        imdb_id: None,
        tvdb_id: None,
        mal_id: None,
        indexer_url: None,
        indexer_api_key: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        history: true,
        history_dir: None,
        nzb_dir: None,
        date: None,
        no_archive: false,
        message_id_domain: None,
        pre_hooks: vec![],
        post_hooks: vec![],
        no_hooks: false,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 30,
        check_retries: 2,
        check_connections: 1,
        check_post_retries: 1,
        allow_incomplete_nzb: false,
        pipeline_depth: 1,
        keepalive_interval: 0,
    };

    let inputs = expand_inputs(std::slice::from_ref(&show)).unwrap();
    post_files(&config, &inputs).await.unwrap();

    // The PAR2 set lands in the directory that *contains* the root folder
    // because pesto writes it alongside the release folder (Show.par2 next to
    // Show/). During posting, the PAR2 files are sent via NNTP just like data
    // files; a download client places all of them flat inside the release
    // folder. Simulate that by moving the PAR2 files into Show/.
    assert!(
        root.join("Show.par2").exists(),
        "index par2 should be written next to the root folder"
    );
    for entry in std::fs::read_dir(&root).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) == Some("par2") {
            let dest = show.join(path.file_name().unwrap());
            std::fs::rename(&path, dest).unwrap();
        }
    }

    // The rest needs par2cmdline; skip cleanly when it is not installed.
    // Exit code 127 means the binary was not found by the kernel.
    //
    // File Description packets store paths relative to the release root
    // (first path component stripped), so par2 is run from inside the
    // release folder — the same directory where all files landed.
    let par2_verify = Command::new("par2")
        .args(["verify", "-q", "Show.par2"])
        .current_dir(&show)
        .output();
    match par2_verify {
        Err(_) => {
            eprintln!("par2cmdline not found, skipping repair check");
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        Ok(out) if out.status.code() == Some(127) => {
            eprintln!("par2cmdline not found (exit 127), skipping repair check");
            std::fs::remove_dir_all(&root).ok();
            return;
        }
        Ok(out) => assert!(
            out.status.success(),
            "par2 verify failed on the pristine tree"
        ),
    }

    // Delete the whole nested subfolder, then repair from the PAR2 set.
    std::fs::remove_dir_all(show.join("extras")).unwrap();
    let repair = Command::new("par2")
        .args(["repair", "-q", "Show.par2"])
        .current_dir(&show)
        .status()
        .unwrap();
    assert!(repair.success(), "par2 repair failed");

    // The subfolder and its file must be recreated bit-exact.
    assert_eq!(
        std::fs::read(show.join("extras/clip.bin")).unwrap(),
        content(2),
        "nested file was not restored correctly"
    );

    std::fs::remove_dir_all(&root).ok();
}
