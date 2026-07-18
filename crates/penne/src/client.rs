//! NNTP download client.
//!
//! Reuses [`pesto::nntp::Connection`] for the TCP/TLS handshake and
//! `AUTHINFO USER`/`PASS` authentication — that part of the protocol is
//! identical for posting and downloading, so it is not reimplemented here.
//!
//! `ARTICLE`/`BODY` retrieval is not implemented yet: `pesto::nntp` only
//! speaks the `POST`/`STAT` commands it needs for uploading. Adding
//! `GROUP`/`ARTICLE`/`BODY` (to `pesto::nntp::Connection` or a sibling
//! module) is Phase 2 of `ROADMAP.md`.

use anyhow::Result;
use pesto::config::ServerEntry;
use pesto::nntp::Connection;

/// A single NNTP connection dedicated to downloading.
pub struct DownloadClient {
    #[allow(dead_code)] // wired up once article retrieval lands (Phase 2)
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

    /// Fetch the raw (yEnc-encoded) body of a single article.
    ///
    /// Not implemented yet — see the module docs and `ROADMAP.md` Phase 2.
    pub async fn body(&mut self, message_id: &str) -> Result<Vec<u8>> {
        anyhow::bail!(
            "ARTICLE/BODY retrieval not implemented yet (message-id {message_id}); \
             see ROADMAP.md Phase 2"
        )
    }
}
