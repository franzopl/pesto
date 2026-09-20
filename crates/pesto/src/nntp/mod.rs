//! NNTP client: TLS connection, authentication and the `POST` command.
//!
//! A [`Connection`] wraps a single NNTP session. It speaks just enough of the
//! protocol (RFC 3977 / RFC 4643) to authenticate and post articles — that is
//! the whole MVP surface.

use anyhow::{anyhow, bail, Context, Result};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter};
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::TlsConnector;
use tokio_socks::tcp::Socks5Stream;
use tracing::{debug, trace};

/// Monotonic timer for per-command latency logging (26b).
use std::time::{Duration, Instant};

mod auth;
pub mod pool;
mod protocol;
mod response;
mod tls;

pub(crate) use auth::{proxy_exit_ip, validate_proxy};
pub(crate) use protocol::yenc_body_has_leading_dot;
use protocol::*;
use response::with_hint;
pub use response::{classify_error, ErrorHint, Response};
use tls::tls_config;

/// Read + write stream, in either plain or TLS form, behind a trait object so
/// [`Connection`] does not need to be generic.
trait Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send> Stream for T {}

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
    /// worker indefinitely — and, since #107, it *also* bounds the TCP
    /// connect and TLS handshake below, for the same reason. Before that
    /// fix, a peer that accepted the TCP connection (so it never looked
    /// "down" to anything watching the socket) but stalled mid-TLS-handshake
    /// hung this function forever: `read_timeout` didn't exist yet at that
    /// point, so nothing here was ever bounded by it. Observed in practice
    /// against a real provider that silently drops a long-lived connection
    /// mid-batch and then stalls the automatic reconnect's TLS handshake —
    /// every worker that hit it sat there indefinitely, at ~0% CPU, with the
    /// socket sitting `ESTABLISHED` the whole time (so nothing at the TCP
    /// layer ever signaled failure either).
    pub async fn connect(
        host: &str,
        port: u16,
        tls: bool,
        timeout_secs: u64,
    ) -> Result<Connection> {
        Self::connect_with_proxy(host, port, tls, timeout_secs, None).await
    }

    /// Open a connection and optionally route it through SOCKS5 before TLS.
    pub async fn connect_with_proxy(
        host: &str,
        port: u16,
        tls: bool,
        timeout_secs: u64,
        proxy: Option<&crate::config::Socks5Proxy>,
    ) -> Result<Connection> {
        debug!(host = "<redacted>", port, tls, "connecting");
        let read_timeout = Duration::from_secs(timeout_secs);
        let stream =
            tokio::time::timeout(read_timeout, Self::connect_stream(host, port, tls, proxy))
                .await
                .with_context(|| {
                    format!("connecting to {host}:{port} timed out after {timeout_secs}s")
                })??;
        let mut conn = Connection {
            stream: BufReader::new(BufWriter::new(stream)),
            read_timeout,
            bytes_written: 0,
            bytes_read: 0,
        };
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

    async fn connect_stream(
        host: &str,
        port: u16,
        tls: bool,
        proxy: Option<&crate::config::Socks5Proxy>,
    ) -> Result<Box<dyn Stream>> {
        let tcp = match proxy {
            Some(proxy) => {
                debug!(proxy = %proxy.address(), "connecting through SOCKS5 proxy");
                let target = format!("{host}:{port}");
                let stream = match (&proxy.username, &proxy.password) {
                    (Some(username), Some(password)) => {
                        Socks5Stream::connect_with_password(
                            proxy.address(),
                            target.as_str(),
                            username,
                            password,
                        )
                        .await
                    }
                    _ => Socks5Stream::connect(proxy.address(), target.as_str()).await,
                }
                .with_context(|| {
                    format!(
                        "SOCKS5 proxy {} refused connection to {host}:{port}",
                        proxy.address()
                    )
                })?;
                stream.into_inner()
            }
            None => TcpStream::connect((host, port))
                .await
                .with_context(|| format!("connecting to {host}:{port}"))?,
        };
        tcp.set_nodelay(true).ok();
        if tls {
            let connector = TlsConnector::from(tls_config());
            let server_name = ServerName::try_from(host.to_string())
                .with_context(|| format!("invalid TLS server name `{host}`"))?;
            let tls_stream = connector
                .connect(server_name, tcp)
                .await
                .context("TLS handshake failed")?;
            debug!(host = "<redacted>", "TLS handshake complete");
            Ok(Box::new(tls_stream))
        } else {
            Ok(Box::new(tcp))
        }
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

        // Body is *not* dot-stuffed. yEnc already escapes a '.' that would
        // start a line (draft v1.3 §4 / `=ybegin` line wrap), so stuffing
        // here would be a second pass over every article on the hot path.
        debug_assert!(
            !yenc_body_has_leading_dot(body),
            "yEnc body must not contain a line starting with '.'; \
             NNTP would treat it as end-of-article"
        );
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
        debug_assert!(
            !yenc_body_has_leading_dot(body),
            "yEnc body must not contain a line starting with '.'; \
             NNTP would treat it as end-of-article"
        );
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

#[cfg(test)]
mod tests;
