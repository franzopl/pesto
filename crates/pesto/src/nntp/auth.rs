//! NNTP authentication and SOCKS5 proxy validation.

use std::time::Duration;

use anyhow::{bail, Context, Result};
use tracing::debug;

use super::{with_hint, Connection};

impl Connection {
    /// Authenticate
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
}

/// Validate the SOCKS5 and NNTP authentication path before posting articles.
pub(crate) async fn validate_proxy(server: &crate::config::ServerEntry) -> Result<()> {
    let Some(proxy) = server.proxy.as_ref() else {
        return Ok(());
    };
    let mut conn = Connection::connect_with_proxy(
        &server.host,
        server.port,
        server.ssl,
        server.timeout,
        Some(proxy),
    )
    .await
    .with_context(|| format!("validating SOCKS5 proxy {}", proxy.address()))?;
    if let Some(username) = &server.username {
        conn.authenticate(username, server.password.as_deref().unwrap_or(""))
            .await
            .context("NNTP authentication through SOCKS5 proxy failed")?;
    }
    conn.quit().await;
    Ok(())
}

/// Query the public exit IP through SOCKS5. This optional check contacts api.ipify.org.
pub(crate) async fn proxy_exit_ip(proxy: &crate::config::Socks5Proxy) -> Result<String> {
    let url = match (&proxy.username, &proxy.password) {
        (Some(user), Some(password)) => format!("socks5h://{user}:{password}@{}", proxy.address()),
        _ => format!("socks5h://{}", proxy.address()),
    };
    let client = reqwest::Client::builder()
        .proxy(reqwest::Proxy::all(url)?)
        .timeout(Duration::from_secs(15))
        .build()?;
    Ok(client
        .get("https://api.ipify.org")
        .send()
        .await
        .context("checking SOCKS5 proxy exit IP")?
        .error_for_status()?
        .text()
        .await?
        .trim()
        .to_owned())
}
