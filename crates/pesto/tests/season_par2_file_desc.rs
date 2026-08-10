//! Regression test: a `--season` global PAR2 recovery set must carry real
//! File Description + IFSC packets for every episode.
//!
//! Earlier versions of `generate_season_par2`/`write_season_par2_volumes`
//! produced anonymous recovery data — a Main packet with an *empty* File ID
//! list and no File Description/IFSC packets at all. That's syntactically
//! valid PAR2 (a compliant reader just sees zero described files), but no
//! PAR2 client can verify, repair, or — under `--obfuscate` — de-obfuscate a
//! season pack's episodes against it, since there is no file association
//! whatsoever. Per-episode PAR2 sets *did* carry the real name correctly,
//! but got discarded once merged into the consolidated season NZB in favor
//! of this global set (see `run_batch` in `bin/pesto.rs`).
//!
//! PAR2 File Description packets always carry the real (never obfuscated)
//! file name, regardless of `--obfuscate` — obfuscation only scrambles the
//! `Subject:`/yEnc `name=` of the *posted NNTP article*, a separate step
//! entirely (see `nzb::generate`'s doc comment for the equivalent reasoning
//! on the `.nzb` side). So this test doesn't need `--obfuscate` at all to
//! exercise the bug; the real name is always what belongs in a FileDesc
//! packet, obfuscated release or not.

use std::process::Command;

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::generate_and_write_season_par2;

/// Deterministic, mutually independent pseudo-random bytes for one episode.
fn content(seed: u8, len: usize) -> Vec<u8> {
    (0..len as u64)
        .map(|i| {
            let mut z = i.wrapping_mul(0x9E37_79B9_7F4A_7C15)
                ^ (seed as u64).wrapping_mul(0xD1B5_4A32_D192_ED03);
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            (z >> 33) as u8
        })
        .collect()
}

fn season_config(par2_slice_size: usize) -> Config {
    Config {
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
        par2_slice_size: Some(par2_slice_size),
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_memory_limit: Some(1_000_000_000),
        memory_limit: None,
        par2_temp_dir: None,
        compress_temp_dir: None,
        par2_only: false,
        par2_before_upload: false,
        threads: 0,
        simd: pesto::par2::SimdPath::Auto,
        extra_servers: vec![],
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        compress_volume_size: None,
        nzb_title: None,
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
        file_counter: false,
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
        check_recover_percent: 15,
        check_recover_max: 0,
        pipeline_depth: 1,
        keepalive_interval: 0,
    }
}

// The PAR2 encoder uses `block_in_place`, which needs a multi-thread runtime.
#[tokio::test(flavor = "multi_thread")]
async fn season_par2_verifies_and_repairs_under_the_real_episode_names() {
    let root = std::env::temp_dir().join(format!("pesto_season_par2_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();

    let episodes = [
        ("S01E01.mkv", 0u8),
        ("S01E02.mkv", 1u8),
        ("S01E03.mkv", 2u8),
    ];
    let episode_paths: Vec<_> = episodes
        .iter()
        .map(|(name, seed)| {
            let path = root.join(name);
            std::fs::write(&path, content(*seed, 50_000)).unwrap();
            path
        })
        .collect();

    let config = season_config(16_384);
    let par2_dir = root.join("par2out");
    std::fs::create_dir_all(&par2_dir).unwrap();

    generate_and_write_season_par2(&episode_paths, "Season01", &par2_dir, &config)
        .await
        .unwrap();

    let volume_files: Vec<_> = std::fs::read_dir(&par2_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("par2"))
        .collect();
    assert!(
        !volume_files.is_empty(),
        "season PAR2 must produce at least one volume file"
    );

    // Move the PAR2 volumes alongside the episodes, as a download client would.
    for vol in &volume_files {
        std::fs::rename(vol, root.join(vol.file_name().unwrap())).unwrap();
    }
    let any_volume = volume_files[0].file_name().unwrap().to_owned();

    // Every volume carries the full base packet set (Main + Creator +
    // FileDesc + IFSC per episode), so par2cmdline can be pointed at any one
    // of them — it must find every episode by its *real* name and confirm
    // the pristine tree.
    let verify = Command::new("par2")
        .arg("verify")
        .arg("-q")
        .arg(&any_volume)
        .current_dir(&root)
        .output();
    let verify = match verify {
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
        Ok(out) => out,
    };
    assert!(
        verify.status.success(),
        "par2 verify failed on the pristine season tree: {}",
        String::from_utf8_lossy(&verify.stderr)
    );

    // Delete one episode entirely, then repair from the season PAR2 set —
    // this only succeeds if the File Description packet named it correctly.
    std::fs::remove_file(root.join("S01E02.mkv")).unwrap();
    let repair = Command::new("par2")
        .arg("repair")
        .arg("-q")
        .arg(&any_volume)
        .current_dir(&root)
        .status()
        .unwrap();
    assert!(repair.success(), "par2 repair failed");

    assert_eq!(
        std::fs::read(root.join("S01E02.mkv")).unwrap(),
        content(1, 50_000),
        "S01E02.mkv was not restored under its real name — the season PAR2's \
         File Description packet must have named it correctly"
    );

    std::fs::remove_dir_all(&root).ok();
}
