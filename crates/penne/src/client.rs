//! NNTP download client.
//!
//! Reuses [`pesto::nntp::Connection`] for the TCP/TLS handshake,
//! `AUTHINFO USER`/`PASS` authentication and now `BODY` retrieval (added to
//! `pesto::nntp` in Phase 2, since that part of the protocol is generic and
//! not specific to `penne`'s download policy) — none of it is reimplemented
//! here.
//!
//! Articles are addressed purely by Message-ID (as `.nzb` files list them),
//! so `GROUP`/`ARTICLE`-by-number are not needed: `BODY <message-id>` alone
//! is sufficient, and headers beyond what the `.nzb` already carries
//! (poster, date, subject) are of no use to a downloader.

use anyhow::Result;
use pesto::config::ServerEntry;
use pesto::nntp::Connection;

/// A single NNTP connection dedicated to downloading.
pub struct DownloadClient {
    conn: Connection,
}

impl DownloadClient {
    /// Connect and authenticate (if credentials are set) against `server`.
    pub async fn connect(server: &ServerEntry) -> Result<Self> {
        let mut conn =
            Connection::connect(&server.host, server.port, server.ssl, server.timeout).await?;
        if let (Some(user), Some(pass)) = (&server.username, &server.password) {
            conn.authenticate(user, pass).await?;
        }
        Ok(Self { conn })
    }

    /// Fetch the raw (still yEnc-encoded) body of a single article.
    ///
    /// Returns `Ok(None)` when this server does not have the article (the
    /// caller should retry against a backup server, see
    /// [`crate::download::download_queue`]).
    pub async fn body(&mut self, message_id: &str) -> Result<Option<Vec<u8>>> {
        self.conn.body(message_id).await
    }

    /// Check whether an article is present on this server via `STAT` (RFC
    /// 3977 §6.2.4) — a small existence check, not an article transfer. Used
    /// by [`crate::check::check_queue`] to verify a release is still fully
    /// grabbable without downloading it.
    pub async fn stat(&mut self, message_id: &str) -> Result<bool> {
        self.conn.stat(message_id).await
    }

    /// Cumulative bytes written to this connection over its whole life.
    pub fn bytes_written(&self) -> u64 {
        self.conn.bytes_written()
    }

    /// Cumulative bytes read from this connection over its whole life.
    pub fn bytes_read(&self) -> u64 {
        self.conn.bytes_read()
    }

    /// Send `QUIT` and close the connection.
    pub async fn quit(mut self) {
        self.conn.quit().await;
    }
}
