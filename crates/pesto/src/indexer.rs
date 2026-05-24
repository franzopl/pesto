//! Automatic NZB upload to a Newznab-compatible indexer.
//!
//! After a successful post, `pesto` can submit the generated `.nzb` to an
//! indexer's `t=addnzb` API endpoint. This is optional and requires
//! `[output.indexer]` to be configured.

use anyhow::{Context, Result};

/// Upload an NZB to a Newznab indexer.
///
/// `url` is the indexer base URL (e.g. `https://my.indexer.example`).
/// The NZB content is sent as a multipart `nzbfile` field via
/// `POST /api?t=addnzb&apikey=KEY&cat=CATEGORY`.
pub async fn upload_nzb(
    url: &str,
    api_key: &str,
    category: Option<&str>,
    nzb_name: &str,
    nzb_content: String,
) -> Result<()> {
    let api_url = format!(
        "{}/api?t=addnzb&apikey={}{}",
        url.trim_end_matches('/'),
        api_key,
        category
            .map(|c| format!("&cat={}", urlencoded(c)))
            .unwrap_or_default(),
    );

    let part = reqwest::multipart::Part::text(nzb_content)
        .file_name(nzb_name.to_string())
        .mime_str("application/x-nzb")
        .context("invalid MIME type")?;

    let form = reqwest::multipart::Form::new().part("nzbfile", part);

    let client = reqwest::Client::new();
    let resp = client
        .post(&api_url)
        .multipart(form)
        .send()
        .await
        .context("sending NZB to indexer")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        anyhow::bail!("indexer returned {status}: {body}");
    }

    Ok(())
}

/// Minimal percent-encoding for query parameter values.
pub(crate) fn urlencoded(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => {
                out.push('%');
                out.push(char::from_digit((b >> 4) as u32, 16).unwrap_or('0'));
                out.push(char::from_digit((b & 0xf) as u32, 16).unwrap_or('0'));
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::urlencoded;

    #[test]
    fn ascii_alphanumeric_passthrough() {
        assert_eq!(urlencoded("ABCxyz019"), "ABCxyz019");
    }

    #[test]
    fn unreserved_chars_passthrough() {
        assert_eq!(urlencoded("-_.~"), "-_.~");
    }

    #[test]
    fn space_encoded() {
        assert_eq!(urlencoded("hello world"), "hello%20world");
    }

    #[test]
    fn slash_encoded() {
        assert_eq!(urlencoded("a/b"), "a%2fb");
    }

    #[test]
    fn at_sign_encoded() {
        assert_eq!(urlencoded("user@host"), "user%40host");
    }

    #[test]
    fn ampersand_encoded() {
        assert_eq!(urlencoded("a&b=c"), "a%26b%3dc");
    }

    #[test]
    fn utf8_multibyte_encoded() {
        // "ã" is U+00E3, encoded as 0xC3 0xA3 in UTF-8.
        assert_eq!(urlencoded("ã"), "%c3%a3");
    }

    #[test]
    fn empty_string() {
        assert_eq!(urlencoded(""), "");
    }
}
