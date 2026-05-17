//! End-to-end test: post a file through `post_files` against an in-process
//! mock NNTP server speaking just enough of the protocol.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

use pesto::config::{Config, ObfuscateMode};
use pesto::poster::post_files;

/// Handle one mock NNTP connection: greet, accept auth, ack each `POST`.
async fn handle_connection(stream: TcpStream, posts: Arc<AtomicUsize>) {
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
            // Consume the article up to the terminating ".\r\n". The yEnc body
            // is not valid UTF-8, so it must be read as raw bytes.
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
            posts.fetch_add(1, Ordering::Relaxed);
            write_half
                .write_all(b"240 article received\r\n")
                .await
                .unwrap();
        } else if command == "QUIT" {
            write_half.write_all(b"205 bye\r\n").await.unwrap();
            return;
        }
    }
}

#[tokio::test]
async fn posts_every_segment_to_a_mock_server() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let posts = Arc::new(AtomicUsize::new(0));

    {
        let posts = posts.clone();
        tokio::spawn(async move {
            loop {
                let (stream, _) = listener.accept().await.unwrap();
                tokio::spawn(handle_connection(stream, posts.clone()));
            }
        });
    }

    // A 250-byte file with a 100-byte article size yields three segments.
    let path = std::env::temp_dir().join(format!("pesto_it_{}.bin", std::process::id()));
    std::fs::write(&path, vec![0xABu8; 250]).unwrap();

    let config = Config {
        host: "127.0.0.1".to_string(),
        port: addr.port(),
        ssl: false,
        connections: 2,
        username: Some("user".to_string()),
        password: Some("pass".to_string()),
        from: "tester <t@pesto.test>".to_string(),
        groups: vec!["alt.binaries.test".to_string()],
        article_size: 100,
        obfuscate: ObfuscateMode::None,
        dry_run: false,
    };

    let outcome = post_files(&config, std::slice::from_ref(&path))
        .await
        .unwrap();
    std::fs::remove_file(&path).ok();

    assert!(
        outcome.failures.is_empty(),
        "unexpected failures: {:?}",
        outcome.failures
    );
    assert!(!outcome.cancelled);
    assert_eq!(outcome.segments.len(), 3);
    assert_eq!(posts.load(Ordering::Relaxed), 3);

    // The collected segments must be enough to build a valid .nzb.
    let nzb = pesto::nzb::generate(&config.from, &config.groups, &outcome.segments);
    assert_eq!(nzb.matches("<segment ").count(), 3);
    assert!(nzb.contains("<file "));
}
