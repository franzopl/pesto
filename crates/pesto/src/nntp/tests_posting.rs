//! Posting-path tests split out of `tests.rs` to stay under the source-size
//! baseline: the ordinary body write must stay scan-free, and only the
//! encrypted path may dot-stuff.

use super::tests::mock_conn_shared as mock_conn;
use crate::nntp::protocol::yenc_body_has_leading_dot;

#[tokio::test]
async fn post_parts_encrypted_dot_stuffs_leading_dot_lines_by_wire_bytes() {
    // Dot-stuffing is decided by the actual wire bytes (Body Std v1.2 §9 /
    // RFC 3977 §3.1.1), never by a prefix shortcut: a leading-dot line is
    // stuffed whether it follows headers (encrypted control line) or a
    // `=ybegin` first line, while a non-dot first line stays unstuffed.
    // Only the `*_encrypted` posting path scans for this — the ordinary
    // `post_parts` writes the body directly (see
    // `post_parts_ordinary_writes_leading_dot_free_body_unstuffed`).
    for (headers, body, stuffed, unstuffed) in [
        (
            &b"Subject: test\r\n\r\n"[..],
            &b"header\r\n.encrypted-control\r\npayload\r\n"[..],
            &b"\r\n..encrypted-control\r\n"[..],
            None,
        ),
        (
            &b"Subject: t\r\n\r\n"[..],
            &b"=ybegin line=128 size=8 name=t.bin\r\n.leading\r\npayload!\r\n=yend size=8\r\n"[..],
            &b"\r\n..leading\r\n"[..],
            Some(&b"\r\n\r\n=ybegin"[..]),
        ),
    ] {
        let (mut conn, mut server) =
            mock_conn(b"340 Send article\r\n240 Article received\r\n").await;
        conn.post_parts_encrypted(headers, body).await.unwrap();
        let mut sent = vec![0u8; 512];
        let count = tokio::io::AsyncReadExt::read(&mut server, &mut sent)
            .await
            .unwrap();
        sent.truncate(count);
        assert!(
            sent.windows(stuffed.len()).any(|w| w == stuffed),
            "leading-dot line must be dot-stuffed, got: {}",
            String::from_utf8_lossy(&sent)
        );
        if let Some(plain) = unstuffed {
            assert!(
                sent.windows(plain.len()).any(|w| w == plain),
                "first line must not be stuffed"
            );
        }
    }
}

#[tokio::test]
async fn post_parts_ordinary_writes_leading_dot_free_body_unstuffed() {
    // The ordinary posting path must not scan or stuff the body: it writes
    // the encoder's output directly, exactly as before `--encrypt` existed.
    // A normal yEnc body (no leading-dot line — the encoder escapes those)
    // goes out byte-for-byte on both the direct and the pipelined entry
    // points.
    let body: &[u8] = b"=ybegin line=128 size=8 name=t.bin\r\npayload!\r\n=yend size=8\r\n";

    let (mut conn, mut server) = mock_conn(b"340 Send article\r\n240 Article received\r\n").await;
    conn.post_parts(b"Subject: t\r\n\r\n", body).await.unwrap();
    let mut sent = vec![0u8; 256];
    let count = tokio::io::AsyncReadExt::read(&mut server, &mut sent)
        .await
        .unwrap();
    sent.truncate(count);
    assert!(
        sent.windows(body.len()).any(|w| w == body),
        "ordinary body must be written byte-for-byte, got: {}",
        String::from_utf8_lossy(&sent)
    );

    let (mut conn, mut server) = mock_conn(b"").await;
    conn.enqueue_post(b"Subject: t\r\n\r\n", body)
        .await
        .unwrap();
    conn.flush_pipeline().await.unwrap();
    let mut sent = vec![0u8; 256];
    let count = tokio::io::AsyncReadExt::read(&mut server, &mut sent)
        .await
        .unwrap();
    sent.truncate(count);
    assert!(
        sent.windows(body.len()).any(|w| w == body),
        "pipelined ordinary body must be written byte-for-byte, got: {}",
        String::from_utf8_lossy(&sent)
    );
}

// ── Dot-stuffing round-trip — salt[0]==0x2E Line 1 bootstrap ──────────

#[tokio::test]
async fn salt_leading_dot_line1_bootstrap_survives_stuffed_round_trip() {
    // Body Std v1.2 §9 (transport boundary): Line 1 of an
    // encrypted article begins with the bootstrap salt; salt byte 0x2E ('.')
    // is legal in the 253-Alphabet, so Line 1 can start with a dot and a
    // conformant server strips/stuffs it on the wire. The posting path must
    // dot-stuff such a line and the receiving path must dot-unstuff it back,
    // byte-identically, BEFORE any line splitting or bootstrap extraction.
    //
    // Round-trip: build an encrypted-style body whose first line starts with
    // '.', post it through `post_parts_encrypted` (producer side), read the
    // raw wire bytes off the server half (a conformant server just stores
    // what it got after stripping per RFC 3977 — here the duplex carries
    // what pesto sent), then feed it back through the receive path
    // (`body`-style read_dot_terminated_block via `mock_conn` framing) and
    // assert the original line content is recovered exactly.
    // 16-byte salt whose FIRST byte is 0x2E; remaining bytes Alphabet-clean.
    let mut salt = [0x41u8; 16];
    salt[0] = 0x2E;
    // 20-byte bootstrap: salt + uint32_be(1) = 0x00000001 (no forbidden bytes).
    let mut line1_prefix = salt.to_vec();
    line1_prefix.extend_from_slice(&1u32.to_be_bytes());

    // Encrypted-style body: line 1 = bootstrap + ciphertext-looking content,
    // plus data and footer lines. Line 1 begins with '.' (0x2E).
    let original_body = {
        let mut b = line1_prefix.clone();
        b.extend_from_slice(b"=ybegin-stuffed-content-sim\r\n");
        b.extend_from_slice(b"payload line\r\n");
        b.extend_from_slice(b"=yend size=29\r\n");
        b
    };

    // ── Producer side: post_parts_encrypted must dot-stuff the leading-dot
    // Line 1 (the ordinary `post_parts` never scans — encrypted bodies are
    // the only ones that can start a line with '.').
    let (mut conn, mut server) = mock_conn(b"340 Send article\r\n240 Article received\r\n").await;
    let headers = b"Subject: t9\r\n\r\n";
    conn.post_parts_encrypted(headers, &original_body)
        .await
        .unwrap();

    // Read what the poster wrote to the wire (bounded read: the duplex half
    // stays open until dropped, so read_to_end would block).
    let mut wire = vec![0u8; 4096];
    let mut wire_len = 0usize;
    loop {
        let n = tokio::io::AsyncReadExt::read(&mut server, &mut wire[wire_len..])
            .await
            .unwrap();
        if n == 0 {
            break;
        }
        wire_len += n;
        // The article POST ends with the "." terminator line; once seen,
        // stop reading (the 240 response write-back follows later).
        if wire[..wire_len].ends_with(b".\r\n") {
            break;
        }
    }
    wire.truncate(wire_len);

    // The headers portion ends with the CRLFCRLF separator; the article
    // body follows. Because Line 1 starts with '.', the wire must carry it
    // dot-stuffed as '..' (RFC 3977 §3.1.1).
    let marker = b"Subject: t9\r\n\r\n";
    let sep = wire
        .windows(marker.len())
        .position(|w| w == marker)
        .expect("headers separator present")
        + marker.len();
    let wire_body = &wire[sep..];
    assert!(
        wire_body.starts_with(b".."),
        "wire Line 1 must be dot-stuffed (leading '..'), got: {:02x?}",
        &wire_body[..8.min(wire_body.len())]
    );
    assert!(yenc_body_has_leading_dot(&original_body));

    // ── Conformant transport simulation: server stores the article with
    // dot-stuffing intact; a receiving client reads the dot-terminated block
    // and unstuffs. Feed the wire article (headers + stuffed body) through a
    // fresh Connection's receive path by replaying it as a BODY response.
    let mut response = b"222 0 <t9> body\r\n".to_vec();
    response.extend_from_slice(wire_body);
    if !wire_body.ends_with(b"\r\n") {
        response.extend_from_slice(b"\r\n");
    }
    response.extend_from_slice(b".\r\n");
    let (mut rx_conn, _rx_server) = mock_conn(&response).await;
    // `body` drives `read_dot_terminated_block` (unstuffing included).
    let received = rx_conn
        .body("<t9>")
        .await
        .unwrap()
        .expect("222 must yield body");

    // The received body must be the ORIGINAL body byte-identically: the
    // stuffed '..' was unstuffed back to '.', and the bootstrap salt is
    // intact — Line 1 still begins with the 0x2E salt byte.
    assert_eq!(received, original_body, "round-trip must be byte-identical");
    assert_eq!(&received[0], &0x2E, "salt[0] must be recovered as 0x2E");

    // Bootstrap extraction (which MUST run after unstuffing) recovers the
    // exact salt and segment index.
    let first_line_end = received
        .windows(2)
        .position(|w| w == b"\r\n")
        .expect("line 1 terminator");
    let line1 = &received[..first_line_end];
    let (extracted_salt, extracted_index) =
        crate::crypto::control::extract_bootstrap_from_line1(line1).unwrap();
    assert_eq!(&extracted_salt[..], &salt[..]);
    assert_eq!(extracted_index, 1);
}
