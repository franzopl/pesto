use super::*;
use std::sync::Arc;

impl Connection {
    /// Construct a `Connection` from any bidirectional stream. Used in tests to
    /// inject a mock transport without opening a real TCP/TLS connection.
    fn from_stream(
        s: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        Connection {
            stream: BufReader::new(BufWriter::new(Box::new(s))),
            read_timeout: Duration::from_secs(crate::config::DEFAULT_TIMEOUT_SECS),
            bytes_written: 0,
            bytes_read: 0,
        }
    }
}

// ── Response::parse ───────────────────────────────────────────────────────

#[test]
fn parses_well_formed_responses() {
    let r = Response::parse("200 posting allowed\r\n").unwrap();
    assert_eq!(r.code, 200);
    assert_eq!(r.text, "posting allowed");

    let r = Response::parse("381\r\n").unwrap();
    assert_eq!(r.code, 381);
    assert_eq!(r.text, "");
}

#[test]
fn rejects_malformed_responses() {
    assert!(Response::parse("xx oops\r\n").is_err());
    assert!(Response::parse("\r\n").is_err());
}

// ── classify_error ───────────────────────────────────────────────────────

#[test]
fn classifies_too_many_connections() {
    assert_eq!(
        classify_error(502, "Too many connections from your account"),
        Some(ErrorHint::TooManyConnections)
    );
    assert_eq!(
        classify_error(400, "Connection limit exceeded"),
        Some(ErrorHint::TooManyConnections)
    );
}

#[test]
fn too_many_connections_excludes_download_and_byte_quota_wording() {
    // "exceed"/"limit" alone would false-positive on a byte-quota message;
    // sabnzbd's own clue explicitly carves this out.
    assert_eq!(
        classify_error(502, "Download limit exceeded for this month"),
        None
    );
    assert_eq!(classify_error(502, "Byte limit exceeded"), None);
}

#[test]
fn classifies_too_many_ip_addresses() {
    assert_eq!(
        classify_error(481, "Login denied, simultaneous IP addresses detected"),
        Some(ErrorHint::TooManyIpAddresses)
    );
}

#[test]
fn classifies_login_failure() {
    assert_eq!(
        classify_error(481, "Authentication failed"),
        Some(ErrorHint::LoginFailed)
    );
    assert_eq!(
        classify_error(452, "Authorization required"),
        Some(ErrorHint::LoginFailed)
    );
    assert_eq!(
        classify_error(502, "Invalid username or password"),
        Some(ErrorHint::LoginFailed)
    );
}

#[test]
fn classifies_payment_required() {
    assert_eq!(
        classify_error(502, "Account expired, please renew"),
        Some(ErrorHint::PaymentRequired)
    );
    // Not 482 here: that code is already caught unconditionally by
    // `LoginFailed`'s code-only branch (see `classify_error`'s comment) —
    // `PaymentRequired` is only ever reachable via 502.
    assert_eq!(
        classify_error(502, "Insufficient credits remaining"),
        Some(ErrorHint::PaymentRequired)
    );
}

#[test]
fn code_482_always_classifies_as_login_failed_regardless_of_text() {
    // 452/481/482/381 are treated as login failure unconditionally,
    // matching sabnzbd's own elif chain — a provider using 482 for an
    // unrelated reason (e.g. quota) still gets the login hint, since
    // that code is reserved for authentication problems by RFC 4643.
    assert_eq!(
        classify_error(482, "Insufficient credits remaining"),
        Some(ErrorHint::LoginFailed)
    );
}

#[test]
fn unrecognized_error_text_classifies_to_none() {
    assert_eq!(classify_error(502, "Service temporarily unavailable"), None);
}

#[test]
fn hint_message_never_contains_the_classified_text() {
    // ErrorHint::message() is a fixed string per variant; guard against a
    // future edit accidentally interpolating the server's own text into
    // it, which would defeat the AUTHINFO PASS credential-safety guarantee.
    let secret = "hunter2-the-actual-password";
    let hint = classify_error(481, &format!("Authentication failed for {secret}")).unwrap();
    assert!(!hint.message().contains(secret));
}

#[test]
fn parses_response_with_no_trailing_text() {
    let r = Response::parse("240\r\n").unwrap();
    assert_eq!(r.code, 240);
    assert_eq!(r.text, "");
}

#[test]
fn parses_response_without_line_ending() {
    // Bare response line (no \r\n) should still parse.
    let r = Response::parse("200 ok").unwrap();
    assert_eq!(r.code, 200);
    assert_eq!(r.text, "ok");
}

// ── already_exists ────────────────────────────────────────────────────────

#[test]
fn already_exists_recognizes_435_rejection() {
    // The real-world 441 text wraps the 435 code (issue #23).
    assert!(already_exists(
        "Article posting failed (posting error: article rejected: 435 Already exists in history)"
    ));
    // Either signal alone is enough, case-insensitively.
    assert!(already_exists("435"));
    assert!(already_exists("Already Exists"));
}

#[test]
fn already_exists_rejects_genuine_failures() {
    assert!(!already_exists("No such group"));
    assert!(!already_exists("posting not permitted"));
    assert!(!already_exists(""));
}

// ── dot_stuff ─────────────────────────────────────────────────────────────

#[test]
fn yenc_body_has_leading_dot_detects_first_and_later_lines() {
    assert!(yenc_body_has_leading_dot(b".hidden\r\n"));
    assert!(yenc_body_has_leading_dot(b"ok\r\n.bad\r\n"));
    assert!(!yenc_body_has_leading_dot(b"ok\r\na.b\r\n"));
    assert!(!yenc_body_has_leading_dot(b""));
}

#[test]
fn yenc_encoder_never_starts_a_body_line_with_dot() {
    // Raw 0x04 + 42 == 0x2E ('.'). At line start the encoder must escape it.
    let mut crafted = vec![0x04u8; 512];
    crafted.extend((0u8..=255).cycle().take(1024));
    for line_len in [1usize, 2, 4, 16, 128] {
        let mut body = Vec::new();
        crate::yenc::encode(&mut body, &crafted, line_len);
        assert!(
            !yenc_body_has_leading_dot(&body),
            "encoder produced a leading-dot line at line_len={line_len}"
        );
    }
}

#[test]
fn dot_stuffs_lines_starting_with_dot() {
    let mut out = Vec::new();
    dot_stuff(b".hello\r\nworld\r\n.dot\r\n", &mut out);
    assert_eq!(out, b"..hello\r\nworld\r\n..dot\r\n");
}

#[test]
fn dot_stuff_leaves_other_lines_untouched() {
    let mut out = Vec::new();
    dot_stuff(b"a.b\r\nc\r\n", &mut out);
    assert_eq!(out, b"a.b\r\nc\r\n");
}

#[test]
fn dot_stuff_empty_input_produces_empty_output() {
    let mut out = Vec::new();
    dot_stuff(b"", &mut out);
    assert!(out.is_empty());
}

#[test]
fn dot_stuff_single_dot_at_buffer_start() {
    // A single `.` with no newline — the very first byte is at line start.
    let mut out = Vec::new();
    dot_stuff(b".", &mut out);
    assert_eq!(out, b"..");
}

#[test]
fn dot_stuff_consecutive_dot_lines() {
    let mut out = Vec::new();
    dot_stuff(b".a\r\n.b\r\n", &mut out);
    assert_eq!(out, b"..a\r\n..b\r\n");
}

// ── Connection protocol (mock stream) ─────────────────────────────────────

/// Write `responses` to the server half of a duplex pair before the test
/// begins so they are immediately available for the `Connection` to read.
async fn mock_conn(responses: &[u8]) -> (Connection, tokio::io::DuplexStream) {
    let (client, mut server) = tokio::io::duplex(4096);
    server.write_all(responses).await.unwrap();
    (Connection::from_stream(client), server)
}

#[tokio::test]
async fn authenticate_accepted_without_password() {
    // Server grants access on AUTHINFO USER alone (code 281).
    let (mut conn, _server) = mock_conn(b"281 Authentication accepted\r\n").await;
    conn.authenticate("user", "pass").await.unwrap();
}

#[tokio::test]
async fn authenticate_requires_and_then_accepts_password() {
    // Server sends 381, then 281 after the password.
    let (mut conn, _server) = mock_conn(
        b"381 Password required\r\n\
              281 Authentication accepted\r\n",
    )
    .await;
    conn.authenticate("user", "s3cr3t").await.unwrap();
}

#[tokio::test]
async fn authenticate_user_rejected_returns_error() {
    // Any code other than 281/381 on AUTHINFO USER is an error.
    let (mut conn, _server) = mock_conn(b"502 Service permanently unavailable\r\n").await;
    let err = conn.authenticate("user", "pass").await.unwrap_err();
    assert!(err.to_string().contains("AUTHINFO USER rejected"));
}

#[tokio::test]
async fn authenticate_password_rejected_returns_error() {
    let (mut conn, _server) = mock_conn(
        b"381 Password required\r\n\
              481 Authentication failed\r\n",
    )
    .await;
    let err = conn.authenticate("user", "wrong").await.unwrap_err();
    assert!(err.to_string().contains("authentication rejected"));
}

#[tokio::test]
async fn post_succeeds_on_240() {
    // Server responds 340 (send article), then 240 (article received).
    let (mut conn, _server) = mock_conn(
        b"340 Send article\r\n\
              240 Article received\r\n",
    )
    .await;
    conn.post(b"From: x\r\n\r\nbody\r\n").await.unwrap();
}

#[tokio::test]
async fn post_not_permitted_returns_error() {
    // Server responds to POST with something other than 340.
    let (mut conn, _server) = mock_conn(b"440 Posting not permitted\r\n").await;
    let err = conn.post(b"article").await.unwrap_err();
    assert!(err.to_string().contains("POST not permitted"));
}

#[tokio::test]
async fn post_rejected_441_returns_error() {
    let (mut conn, _server) = mock_conn(
        b"340 Send article\r\n\
              441 Posting failed\r\n",
    )
    .await;
    let err = conn.post(b"article").await.unwrap_err();
    assert!(err.to_string().contains("441"));
}

#[tokio::test]
async fn post_441_already_exists_is_treated_as_success() {
    // A 441 wrapping a 435 "already exists" means the article is already on
    // the server — a retry after a dropped connection. Not a failure (#23).
    let (mut conn, _server) = mock_conn(
            b"340 Send article\r\n\
              441 Article posting failed (posting error: article rejected: 435 Already exists in history)\r\n",
        )
        .await;
    conn.post(b"article").await.unwrap();
}

#[tokio::test]
async fn pipelined_post_441_already_exists_is_treated_as_success() {
    // Same idempotency rule on the pipelined response path.
    let (mut conn, _server) = mock_conn(
        b"340 Send article\r\n\
              441 435 Already exists in history\r\n",
    )
    .await;
    conn.read_post_response().await.unwrap();
}

#[tokio::test]
async fn post_441_not_unique_is_treated_as_success() {
    // Some servers phrase the same "already have it" rejection without the
    // 435 code, as a non-unique Message-ID instead.
    let (mut conn, _server) = mock_conn(
        b"340 Send article\r\n\
              441 Posting Failed. Message-ID is not unique\r\n",
    )
    .await;
    conn.post(b"article").await.unwrap();
}

#[tokio::test]
async fn stat_article_found_returns_true() {
    let (mut conn, _server) = mock_conn(b"223 0 <mid@host> Article exists\r\n").await;
    assert!(conn.stat("mid@host").await.unwrap());
}

#[tokio::test]
async fn stat_article_not_found_returns_false() {
    let (mut conn, _server) = mock_conn(b"430 No such article\r\n").await;
    assert!(!conn.stat("missing@host").await.unwrap());
}

#[tokio::test]
async fn stat_tracks_exact_bytes_written_and_read() {
    let response = b"223 0 <mid@host> Article exists\r\n";
    let (mut conn, _server) = mock_conn(response).await;
    assert!(conn.stat("mid@host").await.unwrap());

    // Wire request is exactly "STAT <mid@host>\r\n" — `command()` writes
    // the command text and "\r\n" as two separate `write_all_timeout`
    // calls, both counted.
    let expected_written = "STAT <mid@host>".len() as u64 + 2;
    assert_eq!(conn.bytes_written(), expected_written);
    // `read_line`'s returned count (and so `bytes_read`) includes the
    // trailing "\r\n".
    assert_eq!(conn.bytes_read(), response.len() as u64);
}

#[tokio::test]
async fn pipelined_stat_sends_batch_then_reads_responses_in_order() {
    // Three responses queued up ahead of time, as if the server
    // answered all three `STAT`s before the client read any of them —
    // exactly what pipelining is for. `read_stat_response` must return
    // them in the same order the commands were enqueued.
    let responses = b"223 0 <a@x>\r\n430 No such article\r\n223 0 <c@x>\r\n";
    let (mut conn, _server) = mock_conn(responses).await;

    conn.enqueue_stat("a@x").await.unwrap();
    conn.enqueue_stat("b@x").await.unwrap();
    conn.enqueue_stat("c@x").await.unwrap();
    conn.flush_pipeline().await.unwrap();

    assert!(conn.read_stat_response().await.unwrap());
    assert!(!conn.read_stat_response().await.unwrap());
    assert!(conn.read_stat_response().await.unwrap());
}

#[tokio::test]
async fn pipelined_stat_tracks_exact_bytes_written_and_read() {
    let responses = b"223 0 <a@x>\r\n223 0 <b@x>\r\n";
    let (mut conn, _server) = mock_conn(responses).await;

    conn.enqueue_stat("a@x").await.unwrap();
    conn.enqueue_stat("b@x").await.unwrap();
    conn.flush_pipeline().await.unwrap();
    conn.read_stat_response().await.unwrap();
    conn.read_stat_response().await.unwrap();

    let expected_written = ("STAT <a@x>\r\n".len() + "STAT <b@x>\r\n".len()) as u64;
    assert_eq!(conn.bytes_written(), expected_written);
    assert_eq!(conn.bytes_read(), responses.len() as u64);
}

#[tokio::test]
async fn pipelined_stat_unexpected_code_returns_error() {
    let (mut conn, _server) = mock_conn(b"503 Program fault\r\n").await;
    conn.enqueue_stat("a@x").await.unwrap();
    conn.flush_pipeline().await.unwrap();
    let err = conn.read_stat_response().await.unwrap_err();
    assert!(err
        .to_string()
        .contains("unexpected STAT response (pipelined)"));
}

#[test]
fn tls_config_is_built_once_and_shared() {
    // Building the root cert store + crypto provider from scratch is
    // real, non-yielding CPU work; opening many concurrent connections
    // (e.g. a `--stat` check with a high `connections` count) must not
    // pay that cost more than once, or it can visibly stall the async
    // runtime before any actual NNTP traffic starts.
    let a = tls_config();
    let b = tls_config();
    assert!(
        Arc::ptr_eq(&a, &b),
        "tls_config() should return the same cached Arc on every call"
    );
}

#[tokio::test]
async fn stat_unexpected_code_returns_error() {
    let (mut conn, _server) = mock_conn(b"503 Program fault\r\n").await;
    let err = conn.stat("mid@host").await.unwrap_err();
    assert!(err.to_string().contains("unexpected STAT response"));
}

#[tokio::test]
async fn stat_accepts_message_id_with_angle_brackets() {
    // Caller passes "<mid@host>" — brackets must not be doubled on the wire.
    let (mut conn, mut server) = mock_conn(b"223 0 <mid@host> Article exists\r\n").await;
    assert!(conn.stat("<mid@host>").await.unwrap());

    // Verify the command sent to the server did not contain double brackets.
    let mut buf = vec![0u8; 64];
    let n = tokio::io::AsyncReadExt::read(&mut server, &mut buf)
        .await
        .unwrap();
    let sent = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        sent.contains("STAT <mid@host>"),
        "unexpected command: {sent}"
    );
    assert!(!sent.contains("<<"), "double brackets in: {sent}");
}

#[tokio::test]
async fn stat_accepts_message_id_without_angle_brackets() {
    // Caller passes bare "mid@host" — brackets must still be added correctly.
    let (mut conn, mut server) = mock_conn(b"223 0 <mid@host> Article exists\r\n").await;
    assert!(conn.stat("mid@host").await.unwrap());

    let mut buf = vec![0u8; 64];
    let n = tokio::io::AsyncReadExt::read(&mut server, &mut buf)
        .await
        .unwrap();
    let sent = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        sent.contains("STAT <mid@host>"),
        "unexpected command: {sent}"
    );
}

// ── is_dot_terminator ─────────────────────────────────────────────────────

#[test]
fn dot_terminator_recognizes_crlf_and_bare_lf() {
    assert!(is_dot_terminator(b".\r\n"));
    assert!(is_dot_terminator(b".\n"));
    assert!(!is_dot_terminator(b"..\r\n"));
    assert!(!is_dot_terminator(b"..hello\r\n"));
    assert!(!is_dot_terminator(b"hello\r\n"));
}

// ── body ──────────────────────────────────────────────────────────────────

#[tokio::test]
async fn body_returns_decoded_bytes_on_222() {
    let (mut conn, _server) = mock_conn(
        b"222 0 <mid@host> body\r\n\
              line one\r\n\
              ..dot-stuffed line\r\n\
              line three\r\n\
              .\r\n",
    )
    .await;
    let body = conn.body("mid@host").await.unwrap().unwrap();
    assert_eq!(
        body,
        b"line one\r\n.dot-stuffed line\r\nline three\r\n".to_vec()
    );
}

#[tokio::test]
async fn body_returns_none_on_430() {
    let (mut conn, _server) = mock_conn(b"430 No such article\r\n").await;
    assert!(conn.body("missing@host").await.unwrap().is_none());
}

#[tokio::test]
async fn head_returns_header_block_on_221() {
    let (mut conn, _server) = mock_conn(
        b"221 0 <mid@host> headers follow\r\n\
              From: poster@example.com\r\n\
              Subject: test\r\n\
              .\r\n",
    )
    .await;
    let headers = conn.head("mid@host").await.unwrap().unwrap();
    assert_eq!(
        headers,
        b"From: poster@example.com\r\nSubject: test\r\n".to_vec()
    );
}

#[tokio::test]
async fn head_returns_none_on_430() {
    let (mut conn, _server) = mock_conn(b"430 No such article\r\n").await;
    assert!(conn.head("missing@host").await.unwrap().is_none());
}

#[tokio::test]
async fn head_unexpected_code_returns_error() {
    let (mut conn, _server) = mock_conn(b"503 Program fault\r\n").await;
    let err = conn.head("mid@host").await.unwrap_err();
    assert!(err.to_string().contains("unexpected HEAD response"));
}

#[tokio::test]
async fn head_accepts_message_id_with_angle_brackets() {
    let (mut conn, mut server) =
        mock_conn(b"221 0 <mid@host> headers follow\r\nSubject: x\r\n.\r\n").await;
    let headers = conn.head("<mid@host>").await.unwrap().unwrap();
    assert_eq!(headers, b"Subject: x\r\n".to_vec());

    let mut buf = vec![0u8; 64];
    let n = tokio::io::AsyncReadExt::read(&mut server, &mut buf)
        .await
        .unwrap();
    let sent = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        sent.contains("HEAD <mid@host>"),
        "unexpected command: {sent}"
    );
    assert!(!sent.contains("<<"), "double brackets in: {sent}");
}

#[tokio::test]
async fn body_unexpected_code_returns_error() {
    let (mut conn, _server) = mock_conn(b"503 Program fault\r\n").await;
    let err = conn.body("mid@host").await.unwrap_err();
    assert!(err.to_string().contains("unexpected BODY response"));
}

#[tokio::test]
async fn body_accepts_message_id_with_angle_brackets() {
    let (mut conn, mut server) = mock_conn(b"222 0 <mid@host> body\r\ndata\r\n.\r\n").await;
    let body = conn.body("<mid@host>").await.unwrap().unwrap();
    assert_eq!(body, b"data\r\n".to_vec());

    let mut buf = vec![0u8; 64];
    let n = tokio::io::AsyncReadExt::read(&mut server, &mut buf)
        .await
        .unwrap();
    let sent = std::str::from_utf8(&buf[..n]).unwrap();
    assert!(
        sent.contains("BODY <mid@host>"),
        "unexpected command: {sent}"
    );
    assert!(!sent.contains("<<"), "double brackets in: {sent}");
}

#[tokio::test]
async fn body_handles_non_utf8_bytes() {
    // yEnc bodies are 8-bit data; a byte sequence that is not valid UTF-8
    // must still round-trip untouched.
    let mut wire = b"222 0 <mid@host> body\r\n".to_vec();
    wire.extend_from_slice(&[0xFF, 0xFE, b'a', b'\r', b'\n']);
    wire.extend_from_slice(b".\r\n");
    let (mut conn, _server) = mock_conn(&wire).await;
    let body = conn.body("mid@host").await.unwrap().unwrap();
    assert_eq!(body, vec![0xFF, 0xFE, b'a', b'\r', b'\n']);
}

#[tokio::test]
async fn read_response_times_out_on_silent_connection() {
    // Server stays open but never replies — the silent-death scenario (#23).
    // Without a read timeout this would block until the OS keepalive fires.
    let (mut conn, _server) = mock_conn(b"").await;
    conn.read_timeout = Duration::from_millis(50);
    let err = conn.stat("x@y").await.unwrap_err();
    assert!(
        format!("{err:#}").contains("timed out"),
        "expected timeout error, got: {err:#}"
    );
}

#[tokio::test]
async fn connect_times_out_when_tls_handshake_stalls() {
    // Regression test for #107: a peer that accepts the TCP connection
    // but never even starts (let alone completes) the TLS handshake
    // must not hang `Connection::connect` forever. Before the fix,
    // `read_timeout` didn't exist yet at this point in `connect` — it's
    // only set on the `Connection` *after* the handshake — so nothing
    // bounded this wait. `mock_conn`'s in-memory duplex stream can't
    // reproduce this: it bypasses `TcpStream::connect`/the TLS layer
    // entirely, so a real listener is needed here.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    // Accept the connection and then simply never speak TLS — held open
    // for as long as the accept task lives (dropped when the test ends)
    // so the client doesn't fail early on an EOF/RST for the wrong
    // reason; the point is a peer that looks alive but never answers.
    tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        std::future::pending::<()>().await;
        drop(socket);
    });

    let started = Instant::now();
    let result = Connection::connect(&addr.ip().to_string(), addr.port(), true, 1).await;
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(5),
        "connect() took {elapsed:?} to fail — should be bounded by the 1s timeout, not hang"
    );
    // `Connection` isn't `Debug` (holds a `Box<dyn Stream>`), so match
    // manually instead of `unwrap_err`/`expect_err`.
    match result {
        Ok(_) => panic!("expected a timeout error, got Ok"),
        Err(e) => assert!(
            format!("{e:#}").contains("timed out"),
            "expected a 'timed out' error, got: {e:#}"
        ),
    }
}

#[tokio::test]
async fn read_response_detects_closed_connection() {
    // An empty stream simulates a server that closes the connection.
    let (mut conn, server) = mock_conn(b"").await;
    drop(server); // close the write end
    let err = conn.stat("x@y").await.unwrap_err();
    // The STAT command writes to the stream, which may fail, or the
    // subsequent read detects EOF. Either way we get an error.
    let _ = err; // presence of an error is what we assert
}
