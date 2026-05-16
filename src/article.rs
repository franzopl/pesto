//! Article assembly: the Usenet headers (`From`, `Newsgroups`, `Subject`,
//! `Message-ID`) wrapped around a yEnc-encoded body, plus unique `Message-ID`
//! generation.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Process-wide counter ensuring two `Message-ID`s from the same process never
/// collide, even within the same nanosecond.
static MESSAGE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a `Message-ID` of the form `<timestamp.seq.random@domain>`.
///
/// Uniqueness comes from three parts: a nanosecond timestamp, a monotonic
/// per-process counter, and a random value seeded by [`RandomState`].
pub fn generate_message_id(domain: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = MESSAGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let random = RandomState::new().build_hasher().finish();
    format!("<{nanos:x}.{seq:x}.{random:x}@{domain}>")
}

/// The headers of a single Usenet article.
#[derive(Debug, Clone)]
pub struct Article {
    /// `Message-ID` value, including the surrounding angle brackets.
    pub message_id: String,
    /// `From` header value.
    pub from: String,
    /// Newsgroups this article is posted to.
    pub newsgroups: Vec<String>,
    /// `Subject` header value.
    pub subject: String,
}

impl Article {
    /// Serialize the article for posting: header lines, a blank line, then the
    /// already-encoded `body`.
    pub fn serialize(&self, body: &[u8]) -> Vec<u8> {
        let header = format!(
            "From: {}\r\nNewsgroups: {}\r\nSubject: {}\r\nMessage-ID: {}\r\n\r\n",
            self.from,
            self.newsgroups.join(","),
            self.subject,
            self.message_id,
        );
        let mut out = Vec::with_capacity(header.len() + body.len());
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(body);
        out
    }
}

/// Generate a random 32-hex-character name, used to obfuscate the subject and
/// yEnc file name when posting. Each call yields a fresh value.
pub fn obfuscated_name() -> String {
    let high = RandomState::new().build_hasher().finish();
    let low = RandomState::new().build_hasher().finish();
    format!("{high:016x}{low:016x}")
}

/// Build a default subject line for one part of a file.
///
/// Single-part files use just the name; multi-part files append `(part/total)`.
pub fn default_subject(name: &str, part: u32, total: u32) -> String {
    if total > 1 {
        format!("{name} ({part}/{total})")
    } else {
        name.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn message_id_is_bracketed_and_domain_qualified() {
        let id = generate_message_id("pesto");
        assert!(id.starts_with('<') && id.ends_with('>'));
        assert!(id.contains("@pesto"));
    }

    #[test]
    fn message_ids_are_unique() {
        let a = generate_message_id("pesto");
        let b = generate_message_id("pesto");
        assert_ne!(a, b);
    }

    #[test]
    fn serialize_emits_headers_then_blank_line_then_body() {
        let article = Article {
            message_id: "<id@pesto>".into(),
            from: "poster <p@example.com>".into(),
            newsgroups: vec!["alt.binaries.test".into(), "alt.binaries.misc".into()],
            subject: "file.bin (1/2)".into(),
        };
        let serialized = String::from_utf8(article.serialize(b"BODY")).unwrap();
        assert!(serialized.contains("From: poster <p@example.com>\r\n"));
        assert!(serialized.contains("Newsgroups: alt.binaries.test,alt.binaries.misc\r\n"));
        assert!(serialized.contains("Subject: file.bin (1/2)\r\n"));
        assert!(serialized.contains("Message-ID: <id@pesto>\r\n"));
        assert!(serialized.ends_with("\r\n\r\nBODY"));
    }

    #[test]
    fn default_subject_handles_single_and_multi_part() {
        assert_eq!(default_subject("file.bin", 1, 1), "file.bin");
        assert_eq!(default_subject("file.bin", 2, 5), "file.bin (2/5)");
    }

    #[test]
    fn obfuscated_name_is_unique_32_hex_chars() {
        let a = obfuscated_name();
        let b = obfuscated_name();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }
}
