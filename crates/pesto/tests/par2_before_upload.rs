//! `--par2-before-upload` must generate every PAR2 file (index and volumes)
//! before posting anything, then post the data file(s) followed by the
//! already-generated PAR2 files back to back — unlike the default streaming
//! pipeline, where PAR2 encoding runs concurrently with the data upload and,
//! when recovery data needs multiple read passes, can leave a real gap
//! between the last data article and the last PAR2 article. See
//! `ROADMAP.md` and GitHub issue #68.

use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::{post_files, post_files_with_progress};
use pesto::progress::ProgressEvent;
use pesto::walk::expand_inputs;

/// Accept-all mock NNTP server that records the `Subject:` header of every
/// posted article, in the exact order the single connection sends them, into
/// `subjects`.
fn spawn_recording_server(subjects: Arc<Mutex<Vec<String>>>) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let subjects = subjects.clone();
            std::thread::spawn(move || handle_connection(stream, subjects));
        }
    });

    addr
}

fn handle_connection(stream: TcpStream, subjects: Arc<Mutex<Vec<String>>>) {
    let mut writer = match stream.try_clone() {
        Ok(w) => w,
        Err(_) => return,
    };
    let mut reader = BufReader::new(stream);
    if writer.write_all(b"200 pesto mock ready\r\n").is_err() {
        return;
    }

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end().to_string();

        if command == "POST" {
            if writer.write_all(b"340 send article\r\n").is_err() {
                return;
            }
            let mut raw = Vec::new();
            let mut subject = String::new();
            loop {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if raw == b".\r\n" {
                    break;
                }
                if let Some(rest) = std::str::from_utf8(&raw)
                    .ok()
                    .and_then(|l| l.strip_prefix("Subject: "))
                {
                    subject = rest.trim_end().to_string();
                }
            }
            subjects.lock().unwrap().push(subject);
            if writer.write_all(b"240 article received\r\n").is_err() {
                return;
            }
        } else if command.starts_with("MODE READER") {
            if writer.write_all(b"200 reader mode\r\n").is_err() {
                return;
            }
        } else if command == "QUIT" {
            let _ = writer.write_all(b"205 bye\r\n");
            return;
        } else if writer.write_all(b"500 unknown command\r\n").is_err() {
            return;
        }
    }
}

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

/// A real (network-touching) config posting to `addr` over a single
/// connection, so POST order on the wire is deterministic. `par2_memory_limit`
/// is deliberately tiny so `producer` needs several read passes to generate
/// the requested recovery data — the scenario `--par2-before-upload` exists
/// for (see ROADMAP.md / issue #68).
fn config(addr: SocketAddr, par2_before_upload: bool) -> Config {
    Config {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        ssl: false,
        connections: 1,
        username: None,
        password: None,
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 8192,
        line_length: 128,
        retries: 1,
        retry_delay: 1,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 20,
        par2_slice_size: Some(8192),
        par2_slice_count: None,
        par2_recovery_count: None,
        // Small enough that the recovery data for this file needs multiple
        // read passes (see the multi-pass log line `split into N passes`
        // `producer` emits when this happens).
        par2_memory_limit: Some(16 * 1024),
        par2_temp_dir: None,
        compress_temp_dir: None,
        par2_only: false,
        par2_before_upload,
        threads: 0,
        simd: pesto::par2::SimdPath::Auto,
        extra_servers: vec![],
        resume: false,
        upload_rate: 0,
        compress_format: None,
        compress_password: None,
        compress_volume_size: None,
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
        check_delay_secs: 5,
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

#[tokio::test(flavor = "multi_thread")]
async fn par2_before_upload_posts_all_data_before_any_par2_file() {
    let subjects = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_recording_server(subjects.clone());
    let dir = std::env::temp_dir().join(format!(
        "pesto_par2_before_upload_defer_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");
    std::fs::write(&input, content(0, 8192 * 40)).unwrap();

    let cfg = config(addr, true);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files(&cfg, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );

    let par2_segment_count = outcome
        .segments
        .iter()
        .filter(|s| s.file_name.ends_with(".par2"))
        .count();
    assert!(
        par2_segment_count > 0,
        "expected PAR2 segments among: {:?}",
        outcome
            .segments
            .iter()
            .map(|s| &s.file_name)
            .collect::<Vec<_>>()
    );

    // Wire order, as actually received by the mock server over the single
    // connection: every data-file (`.bin`) subject must come before the
    // first PAR2 (`.par2`) subject — no interleaving, no gap-then-catch-up.
    let wire_subjects = subjects.lock().unwrap().clone();
    assert!(
        !wire_subjects.is_empty(),
        "mock server recorded no posted subjects"
    );
    let last_bin = wire_subjects
        .iter()
        .rposition(|s| !s.contains(".par2") && s.contains(".bin"));
    let first_par2 = wire_subjects.iter().position(|s| s.contains(".par2"));
    match (last_bin, first_par2) {
        (Some(last_bin), Some(first_par2)) => assert!(
            last_bin < first_par2,
            "a PAR2 article was posted before the last data article: {wire_subjects:?}"
        ),
        other => panic!("expected both .bin and .par2 subjects on the wire, got: {other:?} in {wire_subjects:?}"),
    }

    let _ = std::fs::remove_dir_all(&outcome.par2_temp_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test(flavor = "multi_thread")]
async fn par2_before_upload_off_by_default_still_posts_successfully() {
    let subjects = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_recording_server(subjects.clone());
    let dir = std::env::temp_dir().join(format!(
        "pesto_par2_before_upload_default_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");
    std::fs::write(&input, content(1, 8192 * 40)).unwrap();

    let cfg = config(addr, false);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let outcome = post_files(&cfg, &inputs).await.unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );
    assert!(
        outcome
            .segments
            .iter()
            .any(|s| s.file_name.ends_with(".par2")),
        "expected PAR2 segments among: {:?}",
        outcome
            .segments
            .iter()
            .map(|s| &s.file_name)
            .collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&outcome.par2_temp_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Resume must skip already-posted segments the same way it does for the
/// default streaming pipeline — including PAR2 index/volume segments, which
/// `--par2-before-upload` posts via the same generic worker-level resume
/// check (see `producer`'s `defer_data_posting` branch: it doesn't add any
/// PAR2-specific resume handling, it relies on the existing per-article
/// check already being generic). PAR2 is deterministic given the same
/// input+config, so a resumed run regenerates identical files and its
/// segments match the previously recorded ones by (file_name, part).
#[tokio::test(flavor = "multi_thread")]
async fn par2_before_upload_resume_skips_already_posted_segments() {
    let dir = std::env::temp_dir().join(format!(
        "pesto_par2_before_upload_resume_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");
    std::fs::write(&input, content(2, 8192 * 40)).unwrap();
    let state_path = dir.join("movie.bin.pesto-state");

    // Learn the exact (file_name, part) set this input+config produces —
    // data segments, the PAR2 index and every volume — with a real run.
    // A fully successful run deletes its own state file (nothing left to
    // resume), so this is only for discovering the segment list, not for
    // reusing its state file directly.
    let subjects = Arc::new(Mutex::new(Vec::new()));
    let addr = spawn_recording_server(subjects.clone());
    let cfg = config(addr, true);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
    let discovery = post_files(&cfg, &inputs).await.unwrap();
    assert!(
        discovery.failures.is_empty(),
        "failures: {:?}",
        discovery.failures
    );
    let _ = std::fs::remove_dir_all(&discovery.par2_temp_dir);

    // Pre-populate resume state with fabricated Message-IDs for every
    // segment discovery found — distinct from anything a real post would
    // generate, so a passing test unambiguously means these were reused
    // from state rather than a fresh (coincidentally matching) post.
    let mut state = pesto::resume::ResumeState::default();
    for (i, seg) in discovery.segments.iter().enumerate() {
        state.record(
            &seg.file_name,
            seg.part,
            &format!("preposted-{i}@resume.test"),
            seg.bytes,
        );
    }
    state.save(&state_path).unwrap();

    // Real run with `--resume` against a fresh mock server: every segment —
    // data and PAR2 alike — must resolve from the pre-populated state
    // instead of posting, and the outcome must carry the fabricated
    // Message-IDs verbatim.
    let subjects2 = Arc::new(Mutex::new(Vec::new()));
    let addr2 = spawn_recording_server(subjects2.clone());
    let mut cfg2 = config(addr2, true);
    cfg2.resume = true;
    let resumed = post_files_with_progress(&cfg2, &inputs, None, Some(&state_path), None)
        .await
        .unwrap();
    assert!(
        resumed.failures.is_empty(),
        "failures: {:?}",
        resumed.failures
    );
    assert_eq!(
        resumed.segments.len(),
        discovery.segments.len(),
        "resumed run produced a different segment count"
    );
    assert_eq!(
        subjects2.lock().unwrap().len(),
        0,
        "resumed run should not have posted anything — all segments were already recorded"
    );
    assert!(
        resumed
            .segments
            .iter()
            .all(|s| s.message_id.ends_with("@resume.test")),
        "resumed segments should carry the fabricated Message-IDs from state, got: {:?}",
        resumed
            .segments
            .iter()
            .map(|s| &s.message_id)
            .collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(&resumed.par2_temp_dir);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Extract `(filenum, total_files)` from a `--file-counter` subject's
/// `[filenum/total_files] - "name" ...` prefix.
fn parse_counter(subject: &str) -> Option<(u32, u32)> {
    let rest = subject.strip_prefix('[')?;
    let (nums, _) = rest.split_once(']')?;
    let (n, m) = nums.split_once('/')?;
    Some((n.parse().ok()?, m.parse().ok()?))
}

/// `--file-counter`'s `[filenum/total]` numbering must stay a clean bijection
/// over every file in the release — no gaps, no duplicates, one consistent
/// `total` — regardless of whether PAR2 generation needed multiple read
/// passes (forced here via a tiny `--memory-limit`) and regardless of
/// `--par2-before-upload`. Multi-pass generation only changes *when* a
/// volume's bytes get produced (a volume's recovery blocks can even span two
/// passes — see the `append` open mode on the volume file), never *which*
/// file-index/total any file gets: both come from `layout::plan_volumes`
/// applied to the release's total recovery-block count, computed once up
/// front, independent of how many passes it takes to actually fill it in.
#[tokio::test(flavor = "multi_thread")]
async fn file_counter_numbering_is_a_clean_bijection_under_multi_pass() {
    for defer in [false, true] {
        let subjects = Arc::new(Mutex::new(Vec::new()));
        let addr = spawn_recording_server(subjects.clone());
        let dir = std::env::temp_dir().join(format!(
            "pesto_par2_before_upload_counter_{}_{defer}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let input = dir.join("movie.bin");
        std::fs::write(&input, content(3, 8192 * 40)).unwrap();

        let mut cfg = config(addr, defer);
        cfg.file_counter = true;
        let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();
        let outcome = post_files(&cfg, &inputs).await.unwrap();
        assert!(
            outcome.failures.is_empty(),
            "defer={defer}: failures: {:?}",
            outcome.failures
        );

        let wire_subjects = subjects.lock().unwrap().clone();
        let parsed: Vec<(u32, u32)> = wire_subjects
            .iter()
            .map(|s| {
                parse_counter(s)
                    .unwrap_or_else(|| panic!("defer={defer}: no [N/M] prefix in subject: {s}"))
            })
            .collect();
        assert!(
            !parsed.is_empty(),
            "defer={defer}: mock server recorded no posted subjects"
        );

        // Every segment of the same file repeats that file's `filenum`, so
        // dedupe by (filenum) to get the actual per-file numbering.
        let mut per_file: Vec<u32> = parsed.iter().map(|(n, _)| *n).collect();
        per_file.sort_unstable();
        per_file.dedup();

        let totals: std::collections::HashSet<u32> = parsed.iter().map(|(_, m)| *m).collect();
        assert_eq!(
            totals.len(),
            1,
            "defer={defer}: inconsistent total across subjects: {totals:?}"
        );
        let total = *totals.iter().next().unwrap();

        assert_eq!(
            per_file,
            (1..=total).collect::<Vec<u32>>(),
            "defer={defer}: file-index numbering isn't a clean 1..=total bijection: {per_file:?}"
        );

        // At least one data file and at least one PAR2 file, or this test
        // isn't actually exercising the interesting part of the release.
        let has_par2 = outcome
            .segments
            .iter()
            .any(|s| s.file_name.ends_with(".par2"));
        assert!(has_par2, "defer={defer}: expected PAR2 segments");

        let _ = std::fs::remove_dir_all(&outcome.par2_temp_dir);
        let _ = std::fs::remove_dir_all(&dir);
    }
}

/// Regression test for a progress-tracking bug: the generation-only
/// `producer(.., None, .., 0)` pre-pass (see `post_files_with_progress_and_cancel`)
/// reuses the same `tx_opt: None` path `--par2-only` takes, which fakes a
/// `SegmentDone` per data article since `--par2-only` never posts anything
/// for real. Under `--par2-before-upload` the data *does* get posted for
/// real afterward (`post_pregenerated_release`), so without a fix each data
/// segment would get double-counted — visible on screen as `done_segments`
/// (and the progress percentage) running past `total_segments`.
#[tokio::test(flavor = "multi_thread")]
async fn par2_before_upload_does_not_double_count_data_segment_progress() {
    let addr = spawn_recording_server(Arc::new(Mutex::new(Vec::new())));
    let dir = std::env::temp_dir().join(format!(
        "pesto_par2_before_upload_progress_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let input = dir.join("movie.bin");
    std::fs::write(&input, content(4, 8192 * 40)).unwrap();

    let cfg = config(addr, true);
    let expected_data_segments = (8192u64 * 40).div_ceil(cfg.article_size as u64);
    let inputs = expand_inputs(std::slice::from_ref(&input)).unwrap();

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<ProgressEvent>();
    let handle =
        tokio::spawn(
            async move { post_files_with_progress(&cfg, &inputs, Some(tx), None, None).await },
        );

    let mut data_segment_done_count: u64 = 0;
    while let Some(ev) = rx.recv().await {
        if let ProgressEvent::SegmentDone { file, .. } = ev {
            if file == "movie.bin" {
                data_segment_done_count += 1;
            }
        }
    }

    let outcome = handle.await.unwrap().unwrap();
    assert!(
        outcome.failures.is_empty(),
        "failures: {:?}",
        outcome.failures
    );
    assert_eq!(
        data_segment_done_count, expected_data_segments,
        "movie.bin should get exactly one SegmentDone per real segment, not double-counted \
         by the generation-only pre-pass's fake --par2-only-style progress reporting"
    );

    let _ = std::fs::remove_dir_all(&outcome.par2_temp_dir);
    let _ = std::fs::remove_dir_all(&dir);
}
