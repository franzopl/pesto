//! NNTP client: TLS connection, authentication and the `POST` command.
//!
//! A [`Connection`] wraps a single NNTP session. It speaks just enough of the
//! protocol (RFC 3977 / RFC 4643) to authenticate and post articles — that is
//! the whole MVP surface.

use std::sync::Arc;

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

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
    stream: BufReader<Box<dyn Stream>>,
}

impl Connection {
    /// Open a connection to `host:port`, performing the TLS handshake when
    /// `tls` is set, and read the server greeting.
    pub async fn connect(host: &str, port: u16, tls: bool) -> Result<Connection> {
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
            Box::new(tls_stream)
        } else {
            Box::new(tcp)
        };

        let mut conn = Connection {
            stream: BufReader::new(stream),
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
        Ok(conn)
    }

    /// Authenticate with `AUTHINFO USER` / `AUTHINFO PASS`.
    ///
    /// The password is never logged or included in error messages.
    pub async fn authenticate(&mut self, username: &str, password: &str) -> Result<()> {
        let resp = self.command(&format!("AUTHINFO USER {username}")).await?;
        match resp.code {
            281 => return Ok(()), // accepted without a password
            381 => {}             // password required
            _ => bail!("AUTHINFO USER rejected: {} {}", resp.code, resp.text),
        }

        let resp = self.send_command("AUTHINFO PASS ", password).await?;
        if resp.code != 281 {
            bail!(
                "authentication rejected by server (code {}); check the configured username and password",
                resp.code
            );
        }
        Ok(())
    }

    /// Post a complete article (headers, a blank line, then the yEnc body).
    ///
    /// The payload is dot-stuffed and terminated per RFC 3977.
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

        self.stream.write_all(&payload).await?;
        self.stream.flush().await?;

        let resp = self.read_response().await?;
        match resp.code {
            240 => Ok(()),
            441 => bail!("article rejected by server (441): {}", resp.text),
            _ => bail!("unexpected POST response: {} {}", resp.code, resp.text),
        }
    }

    /// Check whether an article with `message_id` (without angle brackets) is
    /// present on the server, using the `STAT` command (RFC 3977 §6.2.4).
    ///
    /// Returns `true` when the server responds 223 (article exists), `false`
    /// on 430 (not found). Any other response code is returned as an error.
    pub async fn stat(&mut self, message_id: &str) -> Result<bool> {
        let resp = self.command(&format!("STAT <{message_id}>")).await?;
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

    /// Send a command line and read its response.
    async fn command(&mut self, cmd: &str) -> Result<Response> {
        self.send_command(cmd, "").await
    }

    /// Send `prefix` + `suffix` as one command line. Splitting the line lets
    /// the caller keep secrets (such as a password) out of `prefix`, which is
    /// the part safe to mention in errors.
    async fn send_command(&mut self, prefix: &str, suffix: &str) -> Result<Response> {
        self.stream.write_all(prefix.as_bytes()).await?;
        self.stream.write_all(suffix.as_bytes()).await?;
        self.stream.write_all(b"\r\n").await?;
        self.stream.flush().await?;
        self.read_response().await
    }

    /// Read one response line from the server.
    async fn read_response(&mut self) -> Result<Response> {
        let mut line = String::new();
        let n = self
            .stream
            .read_line(&mut line)
            .await
            .context("reading NNTP response")?;
        if n == 0 {
            bail!("NNTP connection closed by server");
        }
        Response::parse(&line)
    }
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
            stream: BufReader::new(Box::new(s)),
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
