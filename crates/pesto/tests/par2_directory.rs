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
        verify: false,
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        nzb_name: None,
        nzb_password: None,
        nzb_category: None,
        indexer_url: None,
        indexer_api_key: None,
        indexer_category: None,
        no_upload: false,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        history: true,
        history_dir: None,
        nzb_dir: None,
        date: None,
        no_archive: false,
        message_id_domain: None,
        post_hook: None,
        no_hooks: false,
        nfo: false,
        quiet: false,
        bell: false,
        check: false,
        check_delay_secs: 30,
        check_retries: 2,
        pipeline_depth: 1,
    };

    let inputs = expand_inputs(std::slice::from_ref(&show)).unwrap();
    post_files(&config, &inputs).await.unwrap();

    // The PAR2 set lands in the directory that *contains* the root folder, so
    // its File Description packets' relative names resolve correctly.
    assert!(
        root.join("Show.par2").exists(),
        "index par2 should be written next to the root folder"
    );

    // The rest needs par2cmdline; skip cleanly when it is not installed.
    // Exit code 127 means the binary was not found by the kernel.
    let par2_verify = Command::new("par2")
        .args(["verify", "-q", "Show.par2"])
        .current_dir(&root)
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
        .current_dir(&root)
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
