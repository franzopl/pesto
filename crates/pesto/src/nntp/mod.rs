//! NNTP client: TLS connection, authentication and the `POST` command.
//!
//! A [`Connection`] wraps a single NNTP session. It speaks just enough of the
//! protocol (RFC 3977 / RFC 4643) to authenticate and post articles — that is
//! the whole MVP surface.

use std::sync::{Arc, OnceLock};

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

/// A likely cause for an NNTP server error, classified from its response
/// code and text. Mirrors `sabnzbd`'s `clues_login`/`clues_too_many`/
/// `clues_too_many_ip`/`clues_pay` (`sabnzbd/downloader.py`), which turn a
/// raw provider error into a specific, actionable diagnosis instead of just
/// forwarding the response text verbatim — the same "fail clearly, not with
/// a bare code" spirit this project's own design principles already call
/// for. Checked in the same priority `sabnzbd` uses in
/// `Downloader.finish_connect_nw`: a connection-limit clue is checked
/// before the generic login-failure clue, since a provider phrasing a
/// connection limit with a word like "access denied" would otherwise be
/// misclassified as a bad-credentials problem.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorHint {
    TooManyConnections,
    TooManyIpAddresses,
    LoginFailed,
    PaymentRequired,
}

impl ErrorHint {
    /// A short, actionable message for this hint. Never includes any part
    /// of the server's own response text, so it is safe to attach even to
    /// errors from commands that may carry credentials (`AUTHINFO PASS`).
    pub fn message(self) -> &'static str {
        match self {
            ErrorHint::TooManyConnections => {
                "the server is rejecting new connections — lower `connections` for it in your config"
            }
            ErrorHint::TooManyIpAddresses => {
                "the server reports logins from too many different IP addresses for this account — it may be shared or already in use elsewhere"
            }
            ErrorHint::LoginFailed => {
                "check the username/password for this server in your config"
            }
            ErrorHint::PaymentRequired => {
                "this account may need renewal or has exceeded its quota with this provider"
            }
        }
    }
}

/// Classify an NNTP error response into a likely cause. `text` is only
/// pattern-matched here — never echoed back by [`ErrorHint::message`] — so
/// this is safe to call even on a response that might carry credentials.
pub fn classify_error(code: u16, text: &str) -> Option<ErrorHint> {
    let lower = text.to_lowercase();
    let has_any = |clues: &[&str]| clues.iter().any(|c| lower.contains(c));

    if matches!(code, 502 | 400 | 481 | 482)
        && has_any(&["exceed", "connections", "too many", "threads", "limit"])
        && !lower.contains("download")
        && !lower.contains("byte")
    {
        return Some(ErrorHint::TooManyConnections);
    }
    if matches!(code, 502 | 481 | 482) && has_any(&["simultaneous ip", "multiple ip"]) {
        return Some(ErrorHint::TooManyIpAddresses);
    }
    if matches!(code, 452 | 481 | 482 | 381)
        || (matches!(code, 500 | 502)
            && has_any(&["username", "password", "invalid", "authen", "access denied"]))
    {
        return Some(ErrorHint::LoginFailed);
    }
    // Note: 482 is deliberately absent here (unlike the `TooManyConnections`/
    // `LoginFailed` code sets above) — it's already caught unconditionally by
    // `LoginFailed`'s code-only branch, so by the time a response reaches
    // this check its code can only ever be 502. "exceeded" is deliberately
    // *not* a clue here (unlike `sabnzbd`'s `clues_pay`, which included it):
    // it collides with byte/download-quota wording ("download limit
    // exceeded") that has nothing to do with account payment status, and
    // `sabnzbd` gets away with the overlap only because `clues_pay` there
    // picks a retry penalty, never a distinct user-facing message the way
    // `ErrorHint::PaymentRequired` is used here.
    if code == 502 && has_any(&["credits", "paym", "expired"]) {
        return Some(ErrorHint::PaymentRequired);
    }
    None
}

/// Appends a [`classify_error`] hint to `base`, if one applies.
fn with_hint(code: u16, text: &str, base: String) -> String {
    match classify_error(code, text) {
        Some(hint) => format!("{base} — {}", hint.message()),
        None => base,
    }
}

/// A single NNTP session.
pub struct Connection {
    stream: BufReader<BufWriter<Box<dyn Stream>>>,
    /// Maximum time to wait for a single server response line. Guards against a
    /// silently dropped TCP connection where the peer sends neither data nor a
    /// FIN/RST, which would otherwise block until the OS keepalive fires.
    read_timeout: Duration,
    /// Cumulative bytes written to the stream over this connection's whole
    /// life (command lines, article bodies, everything). Lets a caller
    /// report how much traffic a run actually used — e.g. `penne`'s
    /// `--stat` check, where the whole point is that it's cheap.
    bytes_written: u64,
    /// Cumulative bytes read from the stream, mirroring [`Self::bytes_written`].
    bytes_read: u64,
}

impl Connection {
    /// Open a connection to `host:port`, performing the TLS handshake when
    /// `tls` is set, and read the server greeting.
    ///
    /// `timeout_secs` bounds how long any later `read_response` call waits for a
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
            let connector = TlsConnector::from(tls_config());
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
            bytes_written: 0,
            bytes_read: 0,
        };

        // 200 = posting allowed, 201 = posting prohibited.
        let greeting = conn.read_response().await?;
        if greeting.code != 200 && greeting.code != 201 {
            let base = format!(
                "unexpected NNTP greeting: {} {}",
                greeting.code, greeting.text
            );
            bail!(with_hint(greeting.code, &greeting.text, base));
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
            _ => {
                let base = format!("AUTHINFO USER rejected: {} {}", resp.code, resp.text);
                bail!(with_hint(resp.code, &resp.text, base));
            }
        }

        // Password is kept out of log output; only the command prefix is logged.
        debug!("sending AUTHINFO PASS <redacted>");
        let resp = self.send_command("AUTHINFO PASS ", password).await?;
        if resp.code != 281 {
            // resp.text is only pattern-matched inside with_hint/classify_error,
            // never included verbatim in the message — see that fn's doc comment.
            let base = format!(
                "authentication rejected by server (code {}); check the configured username and password",
                resp.code
            );
            bail!(with_hint(resp.code, &resp.text, base));
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
    ///
    /// A `441` duplicate rejection (`already_exists`) is treated as success —
    /// see `already_exists` for why that's the right call when we don't yet
    /// know whether the article actually reached the server.
    ///
    /// Returns `Some(message_id)` when the server echoed a (possibly
    /// different) Message-ID in its `240` response — see
    /// `extract_returned_message_id` — or `None` when it didn't say.
    pub async fn post_parts(&mut self, headers: &[u8], body: &[u8]) -> Result<Option<String>> {
        self.post_parts_inner(headers, body, true).await
    }

    /// Like [`Self::post_parts`], but for re-posting an article a STAT pass already
    /// *confirmed* missing.
    ///
    /// In that situation a `441` duplicate rejection must **not** be trusted
    /// as proof the article is now retrievable: it only proves the
    /// Message-ID is present in the server's dedup history, which can happen
    /// even when the underlying article was never actually committed (a
    /// "ghost" article — e.g. the frontend registered the ID but the backend
    /// spool write never completed). Since the repost reuses the same ID as
    /// the confirmed-missing original, a same-ID repost genuinely cannot
    /// distinguish "already have it, ID is fine" from "ID is poisoned,
    /// content still isn't there" — treating either as success would let a
    /// poisoned ID report itself as "reposted" every round forever without
    /// ever becoming readable. Only an explicit `240` accept counts here;
    /// the caller's own STAT re-verification remains the real arbiter.
    pub async fn repost_parts_confirmed(
        &mut self,
        headers: &[u8],
        body: &[u8],
    ) -> Result<Option<String>> {
        self.post_parts_inner(headers, body, false).await
    }

    async fn post_parts_inner(
        &mut self,
        headers: &[u8],
        body: &[u8],
        dedup_as_success: bool,
    ) -> Result<Option<String>> {
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
            240 => Ok(extract_returned_message_id(&resp.text)),
            441 if dedup_as_success && already_exists(&resp.text) => {
                debug!("article already on server (441/435); treating as posted");
                Ok(None)
            }
            441 => bail!("article rejected by server (441): {}", resp.text),
            _ => bail!("unexpected POST response: {} {}", resp.code, resp.text),
        }
    }

    /// Queue one article on the wire for NNTP pipelining without flushing or
    /// reading any response. After enqueueing all articles in a batch, call
    /// [`Self::flush_pipeline`] once and then [`Self::read_post_response`] once per article.
    ///
    /// The optimistic assumption is that the server will always respond 340 to
    /// POST, which holds for every server that allows posting. If the server
    /// rejects POST with a non-340 code, [`Self::read_post_response`] returns an
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
    /// [`Self::enqueue_post`] calls for a batch, before reading responses.
    pub async fn flush_pipeline(&mut self) -> Result<()> {
        self.flush_timeout().await
    }

    /// Read one `(340, 240)` response pair for a pipelined POST.
    ///
    /// Returns `Ok(Some(message_id))` on a `240` that echoes a (possibly
    /// different) Message-ID — see `extract_returned_message_id` — `Ok(None)`
    /// on a `240` that doesn't say, or an error describing the rejection. On
    /// any error the caller should invalidate the connection.
    pub async fn read_post_response(&mut self) -> Result<Option<String>> {
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
            240 => Ok(extract_returned_message_id(&r240.text)),
            441 if already_exists(&r240.text) => {
                debug!("article already on server (441/435); treating as posted");
                Ok(None)
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
    /// Production code uses [`Self::post_parts`] to avoid copying the body buffer.
    pub async fn post(&mut self, article: &[u8]) -> Result<Option<String>> {
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
            240 => Ok(extract_returned_message_id(&resp.text)),
            441 if already_exists(&resp.text) => {
                debug!("article already on server (441/435); treating as posted");
                Ok(None)
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

    /// Queue one `STAT` command on the wire for NNTP pipelining, without
    /// flushing or reading a response. After enqueueing a batch, call
    /// [`Self::flush_pipeline`] once, then [`Self::read_stat_response`] once per
    /// command — in the same order they were enqueued, since NNTP is a
    /// strict request/response protocol over one connection: the server's
    /// answers arrive in the order the requests did.
    ///
    /// Unlike [`Self::enqueue_post`], `STAT` needs no analogous "optimistic
    /// assumption" about the first response code — it's already a single
    /// request/single response command (see [`Self::read_stat_response`]).
    /// Pipelining pays off enormously here specifically because a `STAT`
    /// carries no payload at all (a POST's pipeline depth is capped low by
    /// how much article data is worth buffering ahead of encode/read
    /// speed; a `STAT` command is a few dozen bytes with nothing to
    /// balance against), so hiding round-trip latency is the entire
    /// benefit and a much higher depth is both safe and effective.
    pub async fn enqueue_stat(&mut self, message_id: &str) -> Result<()> {
        let id = message_id.trim_start_matches('<').trim_end_matches('>');
        self.write_all_timeout(format!("STAT <{id}>\r\n").as_bytes())
            .await
    }

    /// Read one `STAT` response for a pipelined batch queued via
    /// [`Self::enqueue_stat`]. Same semantics as [`Self::stat`]: `Ok(true)` on `223`,
    /// `Ok(false)` on `430`.
    pub async fn read_stat_response(&mut self) -> Result<bool> {
        let resp = self.read_response().await?;
        match resp.code {
            223 => Ok(true),
            430 => Ok(false),
            _ => bail!(
                "unexpected STAT response (pipelined): {} {}",
                resp.code,
                resp.text
            ),
        }
    }

    /// Fetch the raw body of an article by Message-ID, using the `BODY`
    /// command (RFC 3977 §6.2.3). Used by download-side clients (`penne`);
    /// posting never needs to read an article back.
    ///
    /// The message-id may be passed with or without angle brackets. NNTP
    /// dot-stuffing is undone and the terminating `.\r\n` line is not
    /// included in the returned bytes.
    ///
    /// Returns `Ok(None)` on `430` (no such article on this server — the
    /// caller should try a backup server); any other non-`222` code is
    /// returned as an error.
    pub async fn body(&mut self, message_id: &str) -> Result<Option<Vec<u8>>> {
        let id = message_id.trim_start_matches('<').trim_end_matches('>');
        let resp = self.command(&format!("BODY <{id}>")).await?;
        match resp.code {
            222 => Ok(Some(self.read_dot_terminated_block().await?)),
            430 => Ok(None),
            _ => bail!("unexpected BODY response: {} {}", resp.code, resp.text),
        }
    }

    /// Fetch just the headers of an article by Message-ID, using the `HEAD`
    /// command (RFC 3977 §6.2.2) — much cheaper than [`Self::body`] since
    /// only the header block is transferred, not the (often much larger)
    /// article body. Unlike [`Self::stat`] (a bare existence check against
    /// the server's index), `HEAD` reads from the same underlying article
    /// storage `BODY` does, so it can catch a server whose `STAT` index has
    /// drifted out of sync with what it can actually deliver — observed in
    /// the wild: a provider reporting `223` (present) via `STAT` for an
    /// article its `BODY`/`HEAD` then reports `430` (no such article) for.
    ///
    /// The message-id may be passed with or without angle brackets.
    ///
    /// Returns `Ok(None)` on `430`; any other non-`221` code is returned as
    /// an error.
    pub async fn head(&mut self, message_id: &str) -> Result<Option<Vec<u8>>> {
        let id = message_id.trim_start_matches('<').trim_end_matches('>');
        let resp = self.command(&format!("HEAD <{id}>")).await?;
        match resp.code {
            221 => Ok(Some(self.read_dot_terminated_block().await?)),
            430 => Ok(None),
            _ => bail!("unexpected HEAD response: {} {}", resp.code, resp.text),
        }
    }

    /// Read a multi-line, dot-terminated block (as returned by `BODY`/`ARTICLE`)
    /// and undo NNTP dot-stuffing (RFC 3977 §3.1.1).
    ///
    /// Reads raw bytes rather than UTF-8 lines: yEnc article bodies are
    /// 8-bit data and are not guaranteed to be valid UTF-8, so
    /// [`AsyncBufReadExt::read_line`] (which requires valid UTF-8) would be
    /// wrong here.
    async fn read_dot_terminated_block(&mut self) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            let mut line = Vec::new();
            let n =
                tokio::time::timeout(self.read_timeout, self.stream.read_until(b'\n', &mut line))
                    .await
                    .map_err(|_| {
                        anyhow!(
                            "NNTP read timed out after {}s (connection likely dead)",
                            self.read_timeout.as_secs()
                        )
                    })?
                    .context("reading NNTP body")?;
            if n == 0 {
                bail!("NNTP connection closed by server while reading body");
            }
            if is_dot_terminator(&line) {
                break;
            }
            if let Some(rest) = line.strip_prefix(b"..") {
                out.push(b'.');
                out.extend_from_slice(rest);
            } else {
                out.extend_from_slice(&line);
            }
        }
        Ok(out)
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
            .context("writing to NNTP stream")?;
        self.bytes_written += buf.len() as u64;
        Ok(())
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
        self.bytes_read += n as u64;
        let resp = Response::parse(&line)?;
        trace!(code = resp.code, text = %resp.text, "←");
        Ok(resp)
    }

    /// Cumulative bytes written to this connection over its whole life
    /// (every command line and article body sent).
    pub fn bytes_written(&self) -> u64 {
        self.bytes_written
    }

    /// Cumulative bytes read from this connection over its whole life
    /// (every response line and article body received).
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
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

/// Extract a `<message-id>` echoed at the start of a successful POST
/// response's text, if present.
///
/// RFC 3977 §6.3.1.3 does not require a server to echo a Message-ID in its
/// `240` response, but some do — and at least some of those substitute a
/// *different* ID than the one the client sent, at their own discretion
/// (e.g. deduplication or canonicalization applied at accept time). A client
/// that keeps tracking the ID it generated in that case will never find the
/// article again via `STAT`, because that ID was never the one actually
/// used to store it — the server's response is the only place this shows
/// up. `nyuu` has handled this since 2016 (its `RE_POST` matcher); this
/// mirrors that behavior so `pesto` trusts whichever ID the server says it
/// used.
fn extract_returned_message_id(text: &str) -> Option<String> {
    let text = text.trim_start();
    if !text.starts_with('<') {
        return None;
    }
    let end = text.find('>')?;
    Some(text[..=end].to_string())
}

/// Whether `line` is the dot-terminated block's end-of-data marker: a line
/// that is exactly `.`, with either CRLF or bare LF termination.
fn is_dot_terminator(line: &[u8]) -> bool {
    let trimmed = line
        .strip_suffix(b"\r\n")
        .or_else(|| line.strip_suffix(b"\n"))
        .unwrap_or(line);
    trimmed == b"."
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
/// Build (once) and share the TLS client config across every connection.
///
/// Building this from scratch — populating a `RootCertStore` with 100+
/// webpki root certificates and constructing a fresh crypto provider — is
/// synchronous, non-trivial CPU work with no `.await` point in it. Doing
/// that on *every* `connect()` call is harmless for one connection at a
/// time, but opening many connections concurrently (e.g. `penne --stat`
/// with a large `connections` count) used to mean that many threads all
/// doing this rebuild at once, blocking the tokio runtime's worker threads
/// long enough to visibly stall progress reporting before any actual NNTP
/// traffic had even started. Building it once and sharing the `Arc` makes
/// every connection after the first pay only a refcount bump.
fn tls_config() -> Arc<ClientConfig> {
    static CONFIG: OnceLock<Arc<ClientConfig>> = OnceLock::new();
    CONFIG
        .get_or_init(|| {
            let mut roots = RootCertStore::empty();
            roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

            let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
            let config = ClientConfig::builder_with_provider(provider)
                .with_safe_default_protocol_versions()
                .expect("TLS protocol version configuration is static and always valid")
                .with_root_certificates(roots)
                .with_no_client_auth();
            Arc::new(config)
        })
        .clone()
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
            bytes_written: 0,
            bytes_read: 0,
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
