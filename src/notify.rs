//! Completion notifications: webhook (Discord / Slack / Telegram / generic)
//! and ntfy.sh topics.
//!
//! Payloads mirror the upapasta `_webhook.py` format so both tools produce
//! identical notifications. All errors are non-fatal — a notification failure
//! never aborts the upload.

use std::time::Duration;

use anyhow::Result;

/// Configuration for one notification run.
pub struct NotifyConfig<'a> {
    pub webhook_url: Option<&'a str>,
    pub ntfy_topic: Option<&'a str>,
    /// Name of the uploaded content.
    pub name: &'a str,
    /// Total bytes (data + PAR2).
    pub total_bytes: u64,
    /// Newsgroup(s) the content was posted to.
    pub group: Option<&'a str>,
    /// Detected or configured category.
    pub category: Option<&'a str>,
    /// Whether the upload finished without errors.
    pub ok: bool,
}

/// Fire all configured notifications for one completed upload.
/// Errors are logged to stderr and swallowed.
pub async fn send_all(cfg: &NotifyConfig<'_>) {
    if let Some(url) = cfg.webhook_url {
        if let Err(e) = send_webhook(url, cfg).await {
            eprintln!("notify: webhook failed: {e:#}");
        }
    }
    if let Some(topic) = cfg.ntfy_topic {
        if let Err(e) = send_ntfy(topic, cfg).await {
            eprintln!("notify: ntfy failed: {e:#}");
        }
    }
}

/// POST a JSON notification to `url`, adapting the payload for Discord, Slack,
/// Telegram, and generic webhooks — same logic as upapasta `_webhook.py`.
async fn send_webhook(url: &str, cfg: &NotifyConfig<'_>) -> Result<()> {
    let gb = cfg.total_bytes as f64 / (1024.0_f64.powi(3));
    let group_str = cfg.group.map(|g| format!(" → {g}")).unwrap_or_default();
    let cat_str = cfg.category.map(|c| format!(" [{c}]")).unwrap_or_default();
    let status = if cfg.ok { "✅" } else { "❌" };
    let msg = format!(
        "{status} Upload {}: {name}{cat_str} ({gb:.2} GB){group_str}",
        if cfg.ok { "concluído" } else { "com falhas" },
        name = cfg.name,
    );

    let body = if url.contains("discord.com/api/webhooks")
        || url.contains("discordapp.com/api/webhooks")
    {
        format!(r#"{{"content":{}}}"#, json_string(&msg))
    } else if url.contains("hooks.slack.com") || url.contains("api.telegram.org") {
        // Slack and Telegram both accept `{"text": "..."}`.
        format!(r#"{{"text":{}}}"#, json_string(&msg))
    } else {
        // Generic rich payload.
        format!(
            r#"{{"message":{msg_j},"nome":{nome_j},"tamanho_bytes":{bytes},"grupo":{group_j},"categoria":{cat_j},"ok":{ok}}}"#,
            msg_j = json_string(&msg),
            nome_j = json_string(cfg.name),
            bytes = cfg.total_bytes,
            group_j = json_opt(cfg.group),
            cat_j = json_opt(cfg.category),
            ok = cfg.ok,
        )
    };

    http_post(url, &body, "application/json").await
}

/// POST a plain-text notification to an ntfy.sh topic URL.
///
/// The title and priority headers follow ntfy conventions:
/// <https://docs.ntfy.sh/publish/>
async fn send_ntfy(topic: &str, cfg: &NotifyConfig<'_>) -> Result<()> {
    let url = if topic.starts_with("http://") || topic.starts_with("https://") {
        topic.to_string()
    } else {
        format!("https://ntfy.sh/{topic}")
    };

    let gb = cfg.total_bytes as f64 / (1024.0_f64.powi(3));
    let status = if cfg.ok { "✅" } else { "❌" };
    let title = format!(
        "{status} pesto: {}",
        if cfg.ok {
            "upload concluído"
        } else {
            "upload com falhas"
        }
    );
    let body = format!(
        "{name} ({gb:.2} GB){group}",
        name = cfg.name,
        group = cfg.group.map(|g| format!(" → {g}")).unwrap_or_default(),
    );
    let priority = if cfg.ok { "default" } else { "high" };

    http_post_with_headers(
        &url,
        body.as_bytes(),
        "text/plain",
        &[
            ("Title", title.as_str()),
            ("Priority", priority),
            ("Tags", if cfg.ok { "white_check_mark" } else { "x" }),
        ],
    )
    .await
}

/// Perform an HTTP POST with a JSON body using only `tokio` + std TCP.
///
/// No external HTTP crate is used: we open a plain TCP (or TLS via `rustls`)
/// connection and write a minimal HTTP/1.1 request. This keeps the dependency
/// tree small while supporting the most common webhook endpoints.
async fn http_post(url: &str, body: &str, content_type: &str) -> Result<()> {
    http_post_with_headers(url, body.as_bytes(), content_type, &[]).await
}

async fn http_post_with_headers(
    url: &str,
    body: &[u8],
    content_type: &str,
    extra_headers: &[(&str, &str)],
) -> Result<()> {
    use anyhow::{bail, Context};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let (scheme, rest) = url
        .split_once("://")
        .context("invalid URL: missing scheme")?;
    let tls = match scheme {
        "https" => true,
        "http" => false,
        other => anyhow::bail!("unsupported scheme `{other}`"),
    };

    let (host_port, path) = rest
        .split_once('/')
        .map(|(h, p)| (h, format!("/{p}")))
        .unwrap_or((rest, "/".to_string()));

    let (host, port) = if let Some((h, p)) = host_port.split_once(':') {
        let port: u16 = p.parse().context("invalid port")?;
        (h, port)
    } else {
        (host_port, if tls { 443u16 } else { 80u16 })
    };

    let addr = format!("{host}:{port}");
    let tcp = tokio::net::TcpStream::connect(&addr)
        .await
        .with_context(|| format!("connecting to {addr}"))?;
    tcp.set_nodelay(true).ok();

    let mut extra = String::new();
    for (k, v) in extra_headers {
        extra.push_str(k);
        extra.push_str(": ");
        extra.push_str(v);
        extra.push_str("\r\n");
    }

    let request = format!(
        "POST {path} HTTP/1.1\r\nHost: {host}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\n{extra}\r\n",
        len = body.len(),
    );

    // We only need to read enough of the response to know the status code.
    let mut response = Vec::with_capacity(512);

    if tls {
        use std::sync::Arc;
        use tokio_rustls::rustls::pki_types::ServerName;
        use tokio_rustls::rustls::{ClientConfig, RootCertStore};
        use tokio_rustls::TlsConnector;

        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let provider = Arc::new(tokio_rustls::rustls::crypto::ring::default_provider());
        let tls_cfg = ClientConfig::builder_with_provider(provider)
            .with_safe_default_protocol_versions()
            .context("TLS config")?
            .with_root_certificates(roots)
            .with_no_client_auth();
        let connector = TlsConnector::from(Arc::new(tls_cfg));
        let server_name = ServerName::try_from(host.to_string())
            .with_context(|| format!("invalid TLS server name `{host}`"))?;
        let mut stream = connector
            .connect(server_name, tcp)
            .await
            .context("TLS handshake")?;
        stream.write_all(request.as_bytes()).await?;
        stream.write_all(body).await?;
        stream.flush().await?;
        tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
            .await
            .ok();
    } else {
        let mut stream = tcp;
        stream.write_all(request.as_bytes()).await?;
        stream.write_all(body).await?;
        stream.flush().await?;
        tokio::time::timeout(Duration::from_secs(10), stream.read_to_end(&mut response))
            .await
            .ok();
    }

    // Parse the HTTP status line.
    let header = std::str::from_utf8(&response).unwrap_or("");
    let status: u16 = header
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    if status == 0 || status >= 400 {
        bail!("HTTP {status} from {url}");
    }
    Ok(())
}

/// Wrap `s` as a JSON string literal with escaping.
fn json_string(s: &str) -> String {
    let escaped = s
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
        .replace('\r', "\\r");
    format!("\"{escaped}\"")
}

/// Render `Option<&str>` as a JSON value (`null` or `"value"`).
fn json_opt(v: Option<&str>) -> String {
    match v {
        None => "null".to_string(),
        Some(s) => json_string(s),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_string_escapes_special_chars() {
        assert_eq!(json_string(r#"say "hi""#), r#""say \"hi\"""#);
        assert_eq!(json_string("a\\b"), r#""a\\b""#);
    }

    #[test]
    fn json_opt_none_is_null() {
        assert_eq!(json_opt(None), "null");
        assert_eq!(json_opt(Some("foo")), r#""foo""#);
    }
}
