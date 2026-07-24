//! Standalone plain-TCP mock NNTP server for manual load/memory testing.
//!
//! Accepts unlimited connections, ACKs auth and every `POST` immediately —
//! just enough protocol to let a real `pesto` binary post against it locally
//! with many concurrent connections, without touching a real Usenet server.
//!
//! Usage: `cargo run --release --example mock_nntp_server -- <port>`

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};

async fn handle_connection(stream: TcpStream) {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    if write_half
        .write_all(b"200 mock nntp ready\r\n")
        .await
        .is_err()
    {
        return;
    }

    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
        let command = line.trim_end();

        if command.starts_with("AUTHINFO USER") {
            let _ = write_half.write_all(b"381 password required\r\n").await;
        } else if command.starts_with("AUTHINFO PASS") {
            let _ = write_half.write_all(b"281 authenticated\r\n").await;
        } else if command == "POST" {
            if write_half.write_all(b"340 send article\r\n").await.is_err() {
                return;
            }
            let mut body = Vec::new();
            loop {
                body.clear();
                match reader.read_until(b'\n', &mut body).await {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {}
                }
                if body == b".\r\n" {
                    break;
                }
            }
            if write_half
                .write_all(b"240 article received\r\n")
                .await
                .is_err()
            {
                return;
            }
        } else if command.starts_with("STAT") {
            // Streaming check queue's STAT — always report present. Real
            // NNTP STAT takes the message-id as an argument (`STAT <id>`),
            // so this must be a prefix match, not an exact one.
            let _ = write_half
                .write_all(b"223 0 <fake@mock> article exists\r\n")
                .await;
        } else if command == "QUIT" {
            let _ = write_half.write_all(b"205 bye\r\n").await;
            return;
        }
    }
}

#[tokio::main]
async fn main() {
    let port: u16 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    let listener = TcpListener::bind(("127.0.0.1", port)).await.unwrap();
    println!("listening on {}", listener.local_addr().unwrap());
    loop {
        let (stream, _) = listener.accept().await.unwrap();
        tokio::spawn(handle_connection(stream));
    }
}
