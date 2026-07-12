//! NNTP client: TLS connection, authentication and the `POST` command.
//!
//! A [`Connection`] wraps a single NNTP session. It speaks just enough of the
//! protocol (RFC 3977 / RFC 4643) to authenticate and post articles — that is
//! the whole MVP surface.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;
use tracing::{debug, trace};

/// Monotonic timer for per-command latency logging (26b).
use std::time::{Duration, Instant};

pub mod pool;

/// Read + write stream, in either plain or TLS form, behind a trait object so
/// [`Connection`] does not need to be generic.
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

/// A parsed NNTP status response: a three-digit code and the trailing text.
#[derive(Debug, Clone)]
pub struct Response {
    pub code: u16,
    pub text: String,
}

impl Response {
    /// Parse a single response line (`"code text\r\n"`).
    fn parse(line: &str) -> Result<Response> {
        let line = line.trim_end_matches(['\r', '\n']);
        let code: u16 = line
            .get(..3)
            .and_then(|c| c.parse().ok())
            .ok_or_else(|| anyhow!("malformed NNTP response: {line:?}"))?;
        Ok(Response {
            code,
            text: line.get(4..).unwrap_or("").to_string(),
        })
    }
}

/// A single NNTP session.
pub struct Connection {
    stream: BufReader<BufWriter<Box<dyn Stream>>>,
    /// Maximum time to wait for a single server response line. Guards against a
    /// silently dropped TCP connection where the peer sends neither data nor a
    /// FIN/RST, which would otherwise block until the OS keepalive fires.
    read_timeout: Duration,
}

impl Connection {
    /// Open a connection to `host:port`, performing the TLS handshake when
    /// `tls` is set, and read the server greeting.
    ///
    /// `timeout_secs` bounds how long any later [`read_response`] waits for a
    /// server reply before failing, so a silently dead socket cannot hang a
    /// worker indefinitely.
    pub async fn connect(
        host: &str,
        port: u16,
        tls: bool,
        timeout_secs: u64,
    ) -> Result<Connection> {
        debug!(host = "<redacted>", port, tls, "connecting");
        let tcp = TcpStream::connect((host, port))
            .await
            .with_context(|| format!("connecting to {host}:{port}"))?;
        tcp.set_nodelay(true).ok();

        let stream: Box<dyn Stream> = if tls {
            let connector = TlsConnector::from(Arc::new(tls_config()?));
            let server_name = ServerName::try_from(host.to_string())
                .with_context(|| format!("invalid TLS server name `{host}`"))?;
            let tls_stream = connector
                .connect(server_name, tcp)
                .await
                .context("TLS handshake failed")?;
            debug!(host = "<redacted>", "TLS handshake complete");
            Box::new(tls_stream)
        } else {
            Box::new(tcp)
        };

        let mut conn = Connection {
            stream: BufReader::new(BufWriter::new(stream)),
            read_timeout: Duration::from_secs(timeout_secs),
        };

        // 200 = posting allowed, 201 = posting prohibited.
        let greeting = conn.read_response().await?;
        if greeting.code != 200 && greeting.code != 201 {
            bail!(
                "unexpected NNTP greeting: {} {}",
                greeting.code,
                greeting.text
            );
        }
        debug!(code = greeting.code, text = "<redacted>", "server greeting");
        Ok(conn)
    }

    /// Authenticate with `AUTHINFO USER` / `AUTHINFO PASS`.
    ///
    /// Neither the username nor the password is logged or included in error messages.
    pub async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        debug!(username = "<redacted>", "authenticating");
        let resp = self.command(&format!("AUTHINFO USER {username}")).await?;
        match resp.code {
            281 => {
                debug!("authenticated (no password required)");
                return Ok(());
            }
            381 => {}
            _ => bail!("AUTHINFO USER rejected: {} {}", resp.code, resp.text),
        }

        // Password is kept out of log output; only the command prefix is logged.
        debug!("sending AUTHINFO PASS <redacted>");
        let resp = self.send_command("AUTHINFO PASS ", password).await?;
        if resp.code != 281 {
            bail!(
                "authentication rejected by server (code {}); check the configured username and password",
                resp.code
            );
        }
        debug!("authenticated");
        Ok(())
    }

    /// Post an article whose headers and yEnc body are held in separate buffers.
    ///
    /// This avoids copying the body (typically ~768 KB) into a combined buffer.
    /// The headers are dot-stuffed for RFC 3977 compliance. The body is written
    /// directly without dot-stuffing because the yEnc encoder already escapes
    /// any `'.'` that would appear at a line start (yEnc spec §4).
    pub async fn post_parts(&mut self, headers: &[u8], body: &[u8]) -> Result<()> {
        let resp = self.command("POST").await?;
        if resp.code != 340 {
            bail!("POST not permitted: {} {}", resp.code, resp.text);
        }

        let mut stuffed_headers = Vec::with_capacity(headers.len() + 4);
        dot_stuff(headers, &mut stuffed_headers);
        self.write_all_timeout(&stuffed_headers).await?;

        // Write body directly; the BufWriter coalesces this with the headers
        // before the TLS flush.
        self.write_all_timeout(body).await?;
        if !body.ends_with(b"\r\n") {
            self.write_all_timeout(b"\r\n").await?;
        }
        self.write_all_timeout(b".\r\n").await?;
        self.flush_timeout().await?;

        let resp = self.read_response().await?;
        match resp.code {
            240 => Ok(()),
            441 if already_exists(&resp.text) => {
                debug!("article already on server (441/435); treating as posted");
                Ok(())
            }
            441 => bail!("article rejected by server (441): {}", resp.text),
            _ => bail!("unexpected POST response: {} {}", resp.code, resp.text),
        }
    }

    /// Queue one article on the wire for NNTP pipelining without flushing or
    /// reading any response. After enqueueing all articles in a batch, call
    /// [`flush_pipeline`] once and then [`read_post_response`] once per article.
    ///
    /// The optimistic assumption is that the server will always respond 340 to
    /// POST, which holds for every server that allows posting. If the server
    /// rejects POST with a non-340 code, [`read_post_response`] returns an
    /// error and the caller must invalidate the connection.
    pub async fn enqueue_post(&mut self, headers: &[u8], body: &[u8]) -> Result<()> {
        self.write_all_timeout(b"POST\r\n").await?;
        let mut stuffed_headers = Vec::with_capacity(headers.len() + 4);
        dot_stuff(headers, &mut stuffed_headers);
        self.write_all_timeout(&stuffed_headers).await?;
        self.write_all_timeout(body).await?;
        if !body.ends_with(b"\r\n") {
            self.write_all_timeout(b"\r\n").await?;
        }
        self.write_all_timeout(b".\r\n").await?;
        Ok(())
    }

    /// Flush all enqueued articles to the server. Call once after all
    /// [`enqueue_post`] calls for a batch, before reading responses.
    pub async fn flush_pipeline(&mut self) -> Result<()> {
        self.flush_timeout().await
    }

    /// Read one `(340, 240)` response pair for a pipelined POST.
    ///
    /// Returns `Ok(())` on 240, or an error describing the rejection. On any
    /// error the caller should invalidate the connection.
    pub async fn read_post_response(&mut self) -> Result<()> {
        let r340 = self.read_response().await?;
        if r340.code != 340 {
            bail!(
                "POST not permitted (pipelined): {} {}",
                r340.code,
                r340.text
            );
        }
        let r240 = self.read_response().await?;
        match r240.code {
            240 => Ok(()),
            441 if already_exists(&r240.text) => {
                debug!("article already on server (441/435); treating as posted");
                Ok(())
            }
            441 => bail!("article rejected by server (441): {}", r240.text),
            _ => bail!(
                "unexpected POST response (pipelined): {} {}",
                r240.code,
                r240.text
            ),
        }
    }

    /// Post a complete article (headers, a blank line, then the yEnc body).
    ///
    /// The payload is dot-stuffed and terminated per RFC 3977.
    /// Production code uses [`post_parts`] to avoid copying the body buffer.
    pub async fn post(&mut self, article: &[u8]) -> Result<()> {
        let resp = self.command("POST").await?;
        if resp.code != 340 {
            bail!("POST not permitted: {} {}", resp.code, resp.text);
        }

        let mut payload = Vec::with_capacity(article.len() + 64);
        dot_stuff(article, &mut payload);
        if !payload.ends_with(b"\r\n") {
            payload.extend_from_slice(b"\r\n");
        }
        payload.extend_from_slice(b".\r\n");

        self.write_all_timeout(&payload).await?;
        self.flush_timeout().await?;

        let resp = self.read_response().await?;
        match resp.code {
            240 => Ok(()),
            441 if already_exists(&resp.text) => {
                debug!("article already on server (441/435); treating as posted");
                Ok(())
            }
            441 => bail!("article rejected by server (441): {}", resp.text),
            _ => bail!("unexpected POST response: {} {}", resp.code, resp.text),
        }
    }

    /// Check whether an article with `message_id` is present on the server,
    /// using the `STAT` command (RFC 3977 §6.2.4).
    ///
    /// The `message_id` may be passed with or without angle brackets; they are
    /// stripped before the command is sent.
    ///
    /// Returns `true` when the server responds 223 (article exists), `false`
    /// on 430 (not found). Any other response code is returned as an error.
    pub async fn stat(&mut self, message_id: &str) -> Result<bool> {
        let id = message_id.trim_start_matches('<').trim_end_matches('>');
        let resp = self.command(&format!("STAT <{id}>")).await?;
        match resp.code {
            223 => Ok(true),
            430 => Ok(false),
            _ => bail!("unexpected STAT response: {} {}", resp.code, resp.text),
        }
    }

    /// Send `QUIT` and let the connection drop. Errors are ignored.
    pub async fn quit(&mut self) {
        let _ = self.command("QUIT").await;
    }

    /// Send `MODE READER` as a keepalive and return `Ok(())` on success.
    ///
    /// Used to reset the server's idle timer while connections are waiting for
    /// new tasks (PAR2 computation, check-phase delays, `--each` transitions).
    /// Returns an error if the command fails or the connection is dead, in
    /// which case the caller should discard the connection.
    pub async fn mode_reader(&mut self) -> Result<()> {
        let resp = self.command("MODE READER").await?;
        match resp.code {
            200 | 201 => Ok(()),
            _ => bail!("MODE READER: {} {}", resp.code, resp.text),
        }
    }

    /// Send a command line and read its response.
    async fn command(&mut self, cmd: &str) -> Result<Response> {
        self.send_command(cmd, "").await
    }

    /// Send `prefix` + `suffix` as one command line. Splitting the line lets
    /// the caller keep secrets (such as a password) out of `prefix`, which is
    /// the part safe to mention in errors.
    async fn send_command(&mut self, prefix: &str, suffix: &str) -> Result<Response> {
        let t0 = Instant::now();
        trace!(cmd = prefix.trim_end(), "→");
        self.write_all_timeout(prefix.as_bytes()).await?;
        self.write_all_timeout(suffix.as_bytes()).await?;
        self.write_all_timeout(b"\r\n").await?;
        self.flush_timeout().await?;
        let resp = self.read_response().await?;
        let elapsed_ms = t0.elapsed().as_millis();
        trace!(
            cmd = prefix.trim_end(),
            code = resp.code,
            elapsed_ms,
            "← RTT"
        );
        Ok(resp)
    }

    /// Write `buf` to the stream, bounded by `read_timeout`.
    ///
    /// A silent connection death stalls a bare `write_all` for the OS TCP
    /// retransmission timeout (≈2 min on Windows, ≈15 min on Linux) rather
    /// than the user-configured `timeout`. Wrapping writes in the same timeout
    /// as reads ensures a dead connection is detected within `timeout` seconds
    /// regardless of which direction stalls first.
    async fn write_all_timeout(&mut self, buf: &[u8]) -> Result<()> {
        tokio::time::timeout(self.read_timeout, self.stream.write_all(buf))
            .await
            .map_err(|_| {
                anyhow!(
                    "NNTP write timed out after {}s (connection likely dead)",
                    self.read_timeout.as_secs()
                )
            })?
            .context("writing to NNTP stream")
    }

    /// Flush the stream, bounded by `read_timeout`. See [`write_all_timeout`].
    async fn flush_timeout(&mut self) -> Result<()> {
        tokio::time::timeout(self.read_timeout, self.stream.flush())
            .await
            .map_err(|_| {
                anyhow!(
                    "NNTP write timed out after {}s (connection likely dead)",
                    self.read_timeout.as_secs()
                )
            })?
            .context("flushing NNTP stream")
    }

    /// Read one response line from the server.
    ///
    /// The read is bounded by `read_timeout`: if the server sends nothing within
    /// that window the call fails instead of blocking until the OS keepalive
    /// eventually aborts the dead socket (minutes to hours).
    async fn read_response(&mut self) -> Result<Response> {
        let mut line = String::new();
        let n = tokio::time::timeout(self.read_timeout, self.stream.read_line(&mut line))
            .await
            .with_context(|| {
                format!(
                    "NNTP read timed out after {}s (connection likely dead)",
                    self.read_timeout.as_secs()
                )
            })?
            .context("reading NNTP response")?;
        if n == 0 {
            bail!("NNTP connection closed by server");
        }
        let resp = Response::parse(&line)?;
        trace!(code = resp.code, text = %resp.text, "←");
        Ok(resp)
    }
}

/// Decide whether a `441` POST rejection actually means the article is already
/// present on the server, in which case the POST has effectively succeeded.
///
/// When a connection drops after the server accepted an article but before we
/// read its `240`, a retry re-sends the same Message-ID. The server then
/// answers `441` wrapping a `435 Already exists in history` (RFC 3977 §6.2.2:
/// code 435 = "article not wanted; already have it"). Some servers instead
/// phrase the same rejection as a non-unique Message-ID (e.g. "Message-ID is
/// not unique") without the `435` code or that exact wording. Treating either
/// phrasing as success avoids a pointless retry storm over segments that are
/// already posted.
fn already_exists(text: &str) -> bool {
    let lower = text.to_ascii_lowercase();
    lower.contains("already exists") || lower.contains("435") || lower.contains("not unique")
}

/// Apply NNTP dot-stuffing: any line that begins with `.` gets an extra `.`
/// prepended, so it cannot be mistaken for the end-of-data marker.
fn dot_stuff(input: &[u8], out: &mut Vec<u8>) {
    let mut at_line_start = true;
    for &b in input {
        if at_line_start && b == b'.' {
            out.push(b'.');
        }
        out.push(b);
        at_line_start = b == b'\n';
    }
}

/// Build the rustls client configuration, trusting the bundled Mozilla roots.
fn tls_config() -> Result<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
    let config = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .context("configuring TLS protocol versions")?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(config)
}

#[cfg(test)]
impl Connection {
    /// Construct a `Connection` from any bidirectional stream. Used in tests to
    /// inject a mock transport without opening a real TCP/TLS connection.
    fn from_stream(
        s: impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
    ) -> Self {
        Connection {
            stream: BufReader::new(BufWriter::new(Box::new(s))),
            read_timeout: Duration::from_secs(crate::config::DEFAULT_TIMEOUT_SECS),
        }
    }
}

#[cfg(test)]
mod tests {
    use tokio::io::AsyncWriteExt as _;

    use super::*;

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
    async fn read_response_detects_closed_connection() {
        // An empty stream simulates a server that closes the connection.
        let (mut conn, server) = mock_conn(b"").await;
        drop(server); // close the write end
        let err = conn.stat("x@y").await.unwrap_err();
        // The STAT command writes to the stream, which may fail, or the
        // subsequent read detects EOF. Either way we get an error.
        let _ = err; // presence of an error is what we assert
    }
}
