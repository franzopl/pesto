use std::sync::{Arc, Mutex};

use pesto::config::{Config, FileConfig, ObfuscateMode, Overrides};
use pesto::poster::post_files_with_progress;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

async fn handle_connection(stream: TcpStream, captured: Arc<Mutex<Vec<Vec<u8>>>>) {
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
        if command == "POST" {
            write_half.write_all(b"340 send article\r\n").await.unwrap();
            let mut article = Vec::new();
            loop {
                let mut raw = Vec::new();
                if reader.read_until(b'\n', &mut raw).await.unwrap() == 0 {
                    return;
                }
                if raw == b".\r\n" {
                    break;
                }
                if raw.starts_with(b"..") {
                    raw.remove(0);
                }
                article.extend_from_slice(&raw);
            }
            captured.lock().unwrap().push(article);
            write_half
                .write_all(b"240 article received\r\n")
                .await
                .unwrap();
        } else if command.starts_with("STAT") {
            write_half
                .write_all(b"223 0 <id> article exists\r\n")
                .await
                .unwrap();
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

async fn spawn_mock_server() -> (u16, Arc<Mutex<Vec<Vec<u8>>>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let captured = Arc::new(Mutex::new(Vec::new()));
    let server_captured = Arc::clone(&captured);
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            tokio::spawn(handle_connection(stream, Arc::clone(&server_captured)));
        }
    });
    (port, captured)
}

fn make_config(port: u16, encrypt_password: Option<String>) -> Config {
    let mut file = FileConfig::default();
    file.server.host = Some("127.0.0.1".into());
    file.server.port = Some(port);
    file.server.ssl = Some(false);
    file.server.connections = Some(1);
    file.posting.groups = Some(vec!["alt.test".into()]);
    file.posting.article_size = Some(100);
    file.posting.check = Some(false);
    let mut config = Config::resolve(
        file,
        Overrides {
            dry_run: Some(false),
            par2: Some(0),
            encrypt_password,
            ..Default::default()
        },
    )
    .unwrap();
    config.history = false;
    config.no_hooks = true;
    config.obfuscate = ObfuscateMode::None;
    config
}

#[tokio::test]
async fn test_secret_redaction_in_tracing_logs() {
    let sentinel_pass = "SENTINEL_PASSWORD_998877";
    let (port, _captured) = spawn_mock_server().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("log_test.bin");
    let plaintext = b"SECRET_PLAINTEXT_DATA_12345";
    std::fs::write(&path, plaintext).unwrap();

    let log_buffer = Arc::new(Mutex::new(Vec::new()));
    let buffer_clone = Arc::clone(&log_buffer);

    struct BufferWriter(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for BufferWriter {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let make_writer = move || BufferWriter(Arc::clone(&buffer_clone));

    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .with_writer(make_writer)
        .finish();

    tracing::subscriber::set_global_default(subscriber)
        .expect("setting process-global default tracing subscriber");

    let config = make_config(port, Some(sentinel_pass.into()));
    let files = vec![pesto::walk::InputFile {
        path: path.clone(),
        name: "log_test.bin".into(),
    }];

    let outcome = post_files_with_progress(&config, &files, None, None, Some("redaction-capture"))
        .await
        .unwrap();
    assert!(outcome.failures.is_empty());

    let logs = String::from_utf8_lossy(&log_buffer.lock().unwrap()).to_string();

    assert!(
        !logs.contains(sentinel_pass),
        "password sentinel leaked in logs"
    );
    assert!(
        !logs.contains("SECRET_PLAINTEXT_DATA_12345"),
        "plaintext leaked in logs"
    );

    assert!(
        logs.contains("redaction-capture") && logs.contains("upload plan"),
        "logs should contain upload-specific progress context"
    );
}
