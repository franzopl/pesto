//! Inconclusive ≠ MissingConfirmed (design T5 / T5b / T6 / T11 / T21).
//!
//! Mock NNTP only — never a real provider.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files_with_progress;
use pesto::resume::ResumeState;
use pesto::upload::run_upload;

#[derive(Clone, Default)]
struct MockStats {
    posts: Arc<AtomicUsize>,
    stats: Arc<AtomicUsize>,
}

#[derive(Clone, Copy)]
enum StatBehaviour {
    /// Drop the connection without a STAT response (T5).
    Drop,
    /// Reply with this 3-digit code (T6: 480/502).
    Code(&'static str),
    /// Always 430 (T11 MissingConfirmed).
    Missing,
    /// Always 223 (resume re-STAT).
    Present,
}

async fn handle_connection(
    stream: TcpStream,
    counts: MockStats,
    stat: StatBehaviour,
    post_already_exists: bool,
) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    write_half
        .write_all(b"200 pesto mock ready\r\n")
        .await
        .unwrap();

    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.unwrap() == 0 {
            return;
        }
        let command = line.trim_end();

        if command.starts_with("AUTHINFO USER") {
            write_half
                .write_all(b"381 password required\r\n")
                .await
                .unwrap();
        } else if command.starts_with("AUTHINFO PASS") {
            write_half
                .write_all(b"281 authenticated\r\n")
                .await
                .unwrap();
        } else if command == "POST" {
            write_half.write_all(b"340 send article\r\n").await.unwrap();
            let mut body = Vec::new();
            loop {
                body.clear();
                if reader.read_until(b'\n', &mut body).await.unwrap() == 0 {
                    return;
                }
                if body == b".\r\n" {
                    break;
                }
            }
            counts.posts.fetch_add(1, Ordering::Relaxed);
            if post_already_exists {
                write_half
                    .write_all(b"441 435 Already exists in history\r\n")
                    .await
                    .unwrap();
            } else {
                write_half
                    .write_all(b"240 article received\r\n")
                    .await
                    .unwrap();
            }
        } else if command.starts_with("STAT ") {
            counts.stats.fetch_add(1, Ordering::Relaxed);
            match stat {
                StatBehaviour::Drop => return,
                StatBehaviour::Code(code) => {
                    let resp = format!("{code} STAT failed\r\n");
                    write_half.write_all(resp.as_bytes()).await.unwrap();
                }
                StatBehaviour::Missing => {
                    write_half
                        .write_all(b"430 No such article\r\n")
                        .await
                        .unwrap();
                }
                StatBehaviour::Present => {
                    write_half
                        .write_all(b"223 0 article exists\r\n")
                        .await
                        .unwrap();
                }
            }
        } else if command.starts_with("MODE READER") {
            write_half.write_all(b"200 reader mode\r\n").await.unwrap();
        } else if command == "QUIT" {
            write_half.write_all(b"205 bye\r\n").await.unwrap();
            return;
        } else {
            write_half
                .write_all(b"500 unknown command\r\n")
                .await
                .unwrap();
        }
    }
}

async fn spawn_mock(stat: StatBehaviour, post_already_exists: bool) -> (u16, MockStats) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let counts = MockStats::default();
    {
        let counts = counts.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection(
                    stream,
                    counts.clone(),
                    stat,
                    post_already_exists,
                ));
            }
        });
    }
    (addr.port(), counts)
}

fn test_config(port: u16) -> Config {
    Config {
        host: "127.0.0.1".to_string(),
        port,
        ssl: false,
        connections: 2,
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 100,
        line_length: 128,
        retries: 1,
        retry_delay: 0,
        timeout: pesto::config::DEFAULT_TIMEOUT_SECS,
        proxy: None,
        proxy_check_ip: false,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
        par2: 0,
        par2_memory_limit: Some(1_000_000_000),
        memory_limit: None,
        par2_temp_dir: None,
        compress_temp_dir: None,
        par2_slice_size: None,
        par2_slice_count: None,
        par2_recovery_count: None,
        par2_only: false,
        par2_before_upload: false,
        threads: 0,
        simd: parmesan::SimdPath::Auto,
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
        tvdb_kind: None,
        mal_id: None,
        indexer_url: None,
        indexer_api_key: None,
        notify_webhook: None,
        notify_ntfy: None,
        notify: None,
        history: false,
        history_dir: None,
        nzb_dir: None,
        date: None,
        no_archive: false,
        file_counter: false,
        message_id_domain: None,
        pre_hooks: vec![],
        post_hooks: vec![],
        no_hooks: true,
        nfo: false,
        nzb_conflict: pesto::config::NzbConflict::Overwrite,
        quiet: false,
        bell: false,
        check: true,
        check_delay_secs: 0,
        check_retries: 1,
        check_connections: 1,
        check_post_retries: 0,
        allow_incomplete_nzb: false,
        check_recover_percent: 15,
        check_recover_max: 0,
        pipeline_depth: 1,
        keepalive_interval: 0,
    }
}

fn input(dir: &std::path::Path, name: &str, bytes: usize) -> pesto::walk::InputFile {
    let path = dir.join(name);
    std::fs::write(&path, vec![0xABu8; bytes]).unwrap();
    pesto::walk::InputFile {
        path,
        name: name.to_string(),
    }
}

/// T5: connection drop on STAT is Inconclusive, not MissingConfirmed; NZB
/// refused; `--resume` re-STATs the same ID with no extra POST.
#[tokio::test(flavor = "multi_thread")]
async fn t5_stat_drop_is_inconclusive_not_missing() {
    let (port, counts) = spawn_mock(StatBehaviour::Drop, false).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 80);
    let state_path = dir.path().join("movie.bin.pesto-state");
    let config = test_config(port);

    let outcome = post_files_with_progress(
        &config,
        std::slice::from_ref(&file),
        None,
        Some(&state_path),
        None,
    )
    .await
    .unwrap();

    assert!(
        outcome.still_missing.is_empty(),
        "STAT drop must not become MissingConfirmed: {:?}",
        outcome.still_missing
    );
    assert!(
        !outcome.inconclusive.is_empty(),
        "STAT drop must be Inconclusive"
    );
    assert_eq!(
        counts.posts.load(Ordering::Relaxed),
        1,
        "a STAT drop must not trigger a repost"
    );
    assert!(
        state_path.exists(),
        "Inconclusive must persist .pesto-state"
    );
    let state = ResumeState::load(&state_path).unwrap();
    let rec = state.get("movie.bin", 1).expect("id must not be stripped");
    assert!(!rec.confirmed);
    let stored_id = rec.message_id.clone();

    let (port2, counts2) = spawn_mock(StatBehaviour::Present, false).await;
    let mut config2 = test_config(port2);
    config2.resume = true;
    let outcome2 = post_files_with_progress(&config2, &[file], None, Some(&state_path), None)
        .await
        .unwrap();
    assert_eq!(
        counts2.posts.load(Ordering::Relaxed),
        0,
        "resume after Inconclusive must re-STAT the stored id, not POST"
    );
    assert!(counts2.stats.load(Ordering::Relaxed) >= 1);
    assert_eq!(outcome2.segments[0].message_id, stored_id);
    assert!(outcome2.still_missing.is_empty());
    assert!(outcome2.inconclusive.is_empty());
}

/// T5b: `--allow-incomplete-nzb` does not unblock Inconclusive.
#[tokio::test(flavor = "multi_thread")]
async fn t5b_allow_incomplete_does_not_write_nzb_for_inconclusive() {
    let (port, _) = spawn_mock(StatBehaviour::Drop, false).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 80);
    let nzb = dir.path().join("out.nzb");
    let mut config = test_config(port);
    config.allow_incomplete_nzb = true;

    let outcome = run_upload(
        &config,
        std::slice::from_ref(&file.path),
        "movie.bin",
        None,
        None,
        Some(nzb.clone()),
        false,
        None,
    )
    .await
    .unwrap();

    assert!(outcome.had_failures);
    assert!(!outcome.inconclusive.is_empty());
    assert!(
        outcome.nzb_path.is_none() && !nzb.exists(),
        "T5b: --allow-incomplete-nzb must not publish an Inconclusive NZB"
    );
}

/// T6: STAT 480/502 is the same class as T5.
#[tokio::test(flavor = "multi_thread")]
async fn t6_stat_502_is_inconclusive() {
    let (port, counts) = spawn_mock(StatBehaviour::Code("502"), false).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 80);
    let state_path = dir.path().join("movie.bin.pesto-state");
    let config = test_config(port);

    let outcome = post_files_with_progress(&config, &[file], None, Some(&state_path), None)
        .await
        .unwrap();

    assert!(
        outcome.still_missing.is_empty(),
        "502 must not become MissingConfirmed: {:?}",
        outcome.still_missing
    );
    assert!(!outcome.inconclusive.is_empty());
    assert_eq!(counts.posts.load(Ordering::Relaxed), 1);
    assert!(state_path.exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn t6_stat_480_is_inconclusive() {
    let (port, _) = spawn_mock(StatBehaviour::Code("480"), false).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 80);
    let config = test_config(port);
    let outcome = post_files_with_progress(&config, &[file], None, None, None)
        .await
        .unwrap();
    assert!(outcome.still_missing.is_empty());
    assert!(!outcome.inconclusive.is_empty());
}

/// T21 + T11 library path: `run_upload` writes the NZB on MissingConfirmed
/// + `--allow-incomplete-nzb`, keeps state, strips the missing id, and sets
/// `PESTO_INCOMPLETE=1`.
#[tokio::test(flavor = "multi_thread")]
async fn t11_t21_run_upload_allow_incomplete_keeps_state_and_sets_hook_env() {
    let (port, _) = spawn_mock(StatBehaviour::Missing, true).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 80);
    let nzb = dir.path().join("out.nzb");
    let captured = dir.path().join("incomplete.env");
    let mut config = test_config(port);
    config.allow_incomplete_nzb = true;
    config.no_hooks = true;
    config.post_hooks = vec![format!(
        "printf %s \"$PESTO_INCOMPLETE\" > {}",
        captured.display()
    )];

    let outcome = run_upload(
        &config,
        std::slice::from_ref(&file.path),
        "movie.bin",
        None,
        None,
        Some(nzb.clone()),
        false,
        None,
    )
    .await
    .unwrap();

    assert!(outcome.had_failures);
    assert!(outcome.inconclusive.is_empty());
    assert!(
        outcome.nzb_path.is_some() && nzb.exists(),
        "T11: MissingConfirmed + --allow-incomplete-nzb must write the NZB"
    );
    let state_path = nzb.with_extension("pesto-state");
    assert!(
        state_path.exists(),
        "T11: --allow-incomplete-nzb must keep .pesto-state"
    );
    let state = ResumeState::load(&state_path).unwrap();
    assert!(
        state.get("movie.bin", 1).is_none(),
        "T11: MissingConfirmed ids must be stripped from resume"
    );
    let env = std::fs::read_to_string(&captured)
        .unwrap_or_else(|e| panic!("run_upload hook never wrote {}: {e}", captured.display()));
    assert_eq!(
        env.trim(),
        "1",
        "T11: PESTO_INCOMPLETE=1 on the library hook path"
    );
}

/// T21: `run_upload` refuses the NZB on Inconclusive the same way the CLI does.
#[tokio::test(flavor = "multi_thread")]
async fn t21_run_upload_refuses_nzb_on_inconclusive() {
    let (port, _) = spawn_mock(StatBehaviour::Drop, false).await;
    let dir = tempfile::tempdir().unwrap();
    let file = input(dir.path(), "movie.bin", 80);
    let nzb = dir.path().join("out.nzb");
    let config = test_config(port);

    let outcome = run_upload(
        &config,
        std::slice::from_ref(&file.path),
        "movie.bin",
        None,
        None,
        Some(nzb.clone()),
        false,
        None,
    )
    .await
    .unwrap();

    assert!(outcome.had_failures);
    assert!(!outcome.inconclusive.is_empty());
    assert!(outcome.nzb_path.is_none());
    assert!(!nzb.exists());
    let state_path = nzb.with_extension("pesto-state");
    assert!(state_path.exists());
}
