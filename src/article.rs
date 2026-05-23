//! Article assembly: the Usenet headers (`From`, `Newsgroups`, `Subject`,
//! `Message-ID`, `Date`) wrapped around a yEnc-encoded body, plus unique
//! `Message-ID` generation.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Process-wide counter ensuring two `Message-ID`s from the same process never
/// collide, even within the same nanosecond.
static MESSAGE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Generate a `Message-ID` of the form `<timestamp.seq.random@domain>`.
///
/// When `fixed_domain` is `Some`, that value is used as the domain; otherwise
/// a freshly randomised label is generated so no fixed identifier leaks through
/// the Message-ID header.
pub fn generate_message_id(fixed_domain: Option<&str>) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = MESSAGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let random = RandomState::new().build_hasher().finish();
    let domain = match fixed_domain {
        Some(d) => d.to_string(),
        None => {
            let label = random_alpha(8 + rand_u64() as usize % 8); // 8..=15 chars
            let tld = ["com", "net", "org"][rand_u64() as usize % 3];
            format!("{label}.{tld}")
        }
    };
    format!("<{nanos:x}.{seq:x}.{random:x}@{domain}>")
}

/// Format a `SystemTime` as an RFC 2822 date string (e.g.
/// `Mon, 01 Jan 2024 12:00:00 +0000`).
///
/// Implemented without external crates using integer arithmetic on the Unix
/// timestamp. Only supports UTC (+0000) since that is always valid for Usenet.
pub fn format_rfc2822(t: SystemTime) -> String {
    let secs = t
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs();

    // Gregorian calendar decomposition.
    let (y, mut doy) = days_to_ymd(secs / 86400);
    let h = (secs % 86400) / 3600;
    let m = (secs % 3600) / 60;
    let s = secs % 60;

    // Month lengths (non-leap; adjust Feb for leap years).
    let month_days: [u64; 12] = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let is_leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let mut month = 0usize;
    for (i, &md) in month_days.iter().enumerate() {
        let days = if i == 1 && is_leap { md + 1 } else { md };
        if doy < days {
            month = i;
            break;
        }
        doy -= days;
    }
    let day = doy + 1;

    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    // Day-of-week: 1970-01-01 was a Thursday (4).
    let dow = (secs / 86400 + 4) % 7;
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];

    format!(
        "{}, {:02} {} {} {:02}:{:02}:{:02} +0000",
        DAYS[dow as usize], day, MONTHS[month], y, h, m, s
    )
}

/// Decompose a day count since 1970-01-01 into (year, day-of-year-0-indexed).
fn days_to_ymd(mut days: u64) -> (u64, u64) {
    let mut y = 1970u64;
    loop {
        let leap = (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400);
        let dy = if leap { 366 } else { 365 };
        if days < dy {
            return (y, days);
        }
        days -= dy;
        y += 1;
    }
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
    /// RFC 2822 `Date:` header value. When `None` the header is omitted and
    /// the server fills it in — equivalent to `date = "now"` on most servers.
    pub date: Option<String>,
    /// When true, add `X-No-Archive: yes` to suppress archiving.
    pub no_archive: bool,
}

impl Article {
    /// Build the RFC 2822 header block (including the trailing blank line).
    /// The returned bytes are ready to be written directly to the NNTP stream.
    pub fn build_headers(&self) -> Vec<u8> {
        let mut h = format!(
            "From: {}\r\nNewsgroups: {}\r\nSubject: {}\r\nMessage-ID: {}\r\n",
            self.from,
            self.newsgroups.join(","),
            self.subject,
            self.message_id,
        );
        if let Some(date) = &self.date {
            h.push_str("Date: ");
            h.push_str(date);
            h.push_str("\r\n");
        }
        if self.no_archive {
            h.push_str("X-No-Archive: yes\r\n");
        }
        h.push_str("\r\n");
        h.into_bytes()
    }

    /// Serialize the article for posting: header lines, a blank line, then the
    /// already-encoded `body`. Kept for tests; production code calls
    /// [`build_headers`] and posts headers + body separately to avoid copying
    /// the body.
    pub fn serialize(&self, body: &[u8]) -> Vec<u8> {
        let mut out = self.build_headers();
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

/// A fresh source of randomness. `RandomState` is seeded by the OS on every
/// construction, so each call yields an unrelated 64-bit value — the same
/// trick used for `Message-ID`s, which keeps `pesto` free of an RNG crate.
fn rand_u64() -> u64 {
    RandomState::new().build_hasher().finish()
}

/// Build a string of `len` random lowercase ASCII letters.
fn random_alpha(len: usize) -> String {
    const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    let mut s = String::with_capacity(len);
    let (mut bits, mut left) = (0u64, 0u32);
    for _ in 0..len {
        if left < 8 {
            bits = rand_u64();
            left = 64;
        }
        s.push(ALPHA[(bits & 0xff) as usize % ALPHA.len()] as char);
        bits >>= 8;
        left -= 8;
    }
    s
}

/// Generate a random `From` header of the form `Name <local@domain.tld>`.
///
/// Both the display name and the address use a randomised character count so
/// no two posts share an obvious fingerprint. Used whenever the user has not
/// pinned a fixed `from` in the config or via `--from`.
pub fn random_from() -> String {
    let name = random_alpha(5 + (rand_u64() as usize % 8)); // 5..=12 chars
    let local = random_alpha(6 + (rand_u64() as usize % 9)); // 6..=14 chars
    let domain = random_alpha(5 + (rand_u64() as usize % 8)); // 5..=12 chars
    let tld = ["com", "net", "org"][rand_u64() as usize % 3];
    let mut display = name;
    display[..1].make_ascii_uppercase();
    format!("{display} <{local}@{domain}.{tld}>")
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
        let id = generate_message_id(None);
        assert!(id.starts_with('<') && id.ends_with('>'));
        assert!(id.contains('@'));
    }

    #[test]
    fn message_id_uses_fixed_domain_when_provided() {
        let id = generate_message_id(Some("example.com"));
        assert!(id.ends_with("@example.com>"));
    }

    #[test]
    fn message_ids_are_unique() {
        let a = generate_message_id(None);
        let b = generate_message_id(None);
        assert_ne!(a, b);
    }

    #[test]
    fn serialize_emits_headers_then_blank_line_then_body() {
        let article = Article {
            message_id: "<id@pesto>".into(),
            from: "poster <p@example.com>".into(),
            newsgroups: vec!["alt.binaries.test".into(), "alt.binaries.misc".into()],
            subject: "file.bin (1/2)".into(),
            date: None,
            no_archive: false,
        };
        let serialized = String::from_utf8(article.serialize(b"BODY")).unwrap();
        assert!(serialized.contains("From: poster <p@example.com>\r\n"));
        assert!(serialized.contains("Newsgroups: alt.binaries.test,alt.binaries.misc\r\n"));
        assert!(serialized.contains("Subject: file.bin (1/2)\r\n"));
        assert!(serialized.contains("Message-ID: <id@pesto>\r\n"));
        assert!(serialized.ends_with("\r\n\r\nBODY"));
    }

    #[test]
    fn serialize_includes_date_when_set() {
        let article = Article {
            message_id: "<id@pesto>".into(),
            from: "p <p@x.com>".into(),
            newsgroups: vec!["a.b.test".into()],
            subject: "s".into(),
            date: Some("Mon, 01 Jan 2024 00:00:00 +0000".into()),
            no_archive: false,
        };
        let serialized = String::from_utf8(article.serialize(b"")).unwrap();
        assert!(serialized.contains("Date: Mon, 01 Jan 2024 00:00:00 +0000\r\n"));
    }

    #[test]
    fn serialize_includes_x_no_archive_when_set() {
        let article = Article {
            message_id: "<id@pesto>".into(),
            from: "p <p@x.com>".into(),
            newsgroups: vec!["a.b.test".into()],
            subject: "s".into(),
            date: None,
            no_archive: true,
        };
        let serialized = String::from_utf8(article.serialize(b"")).unwrap();
        assert!(serialized.contains("X-No-Archive: yes\r\n"));
    }

    #[test]
    fn format_rfc2822_epoch() {
        // 1970-01-01 00:00:00 UTC was a Thursday.
        let t = UNIX_EPOCH;
        let s = format_rfc2822(t);
        assert_eq!(s, "Thu, 01 Jan 1970 00:00:00 +0000");
    }

    #[test]
    fn format_rfc2822_known_date() {
        // 2024-01-15 11:30:45 UTC  (verified: 1705318245 % 86400 = 41445 s = 11h30m45s)
        let t = UNIX_EPOCH + Duration::from_secs(1705318245);
        let s = format_rfc2822(t);
        assert_eq!(s, "Mon, 15 Jan 2024 11:30:45 +0000");
    }

    #[test]
    fn default_subject_handles_single_and_multi_part() {
        assert_eq!(default_subject("file.bin", 1, 1), "file.bin");
        assert_eq!(default_subject("file.bin", 2, 5), "file.bin (2/5)");
    }

    #[test]
    fn random_from_is_address_shaped_and_varied() {
        let a = random_from();
        let b = random_from();
        // `Name <local@domain.tld>` — must carry an `@` so the domain can be
        // extracted for `Message-ID`s.
        assert!(a.contains(" <") && a.ends_with('>') && a.contains('@'));
        assert_ne!(a, b);
    }

    #[test]
    fn obfuscated_name_is_unique_32_hex_chars() {
        let a = obfuscated_name();
        let b = obfuscated_name();
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, b);
    }

    #[test]
    fn serialize_empty_body_produces_valid_structure() {
        let article = Article {
            message_id: "<id@pesto>".into(),
            from: "p <p@x.com>".into(),
            newsgroups: vec!["alt.test".into()],
            subject: "empty.bin".into(),
            date: None,
            no_archive: false,
        };
        let serialized = String::from_utf8(article.serialize(b"")).unwrap();
        // Header block ends with \r\n\r\n; body is empty.
        assert!(serialized.ends_with("\r\n\r\n"));
        // All four mandatory headers are present.
        assert!(serialized.contains("From:"));
        assert!(serialized.contains("Newsgroups:"));
        assert!(serialized.contains("Subject:"));
        assert!(serialized.contains("Message-ID:"));
    }

    #[test]
    fn serialize_omits_date_and_x_no_archive_when_not_set() {
        let article = Article {
            message_id: "<id@pesto>".into(),
            from: "p <p@x.com>".into(),
            newsgroups: vec!["alt.test".into()],
            subject: "f".into(),
            date: None,
            no_archive: false,
        };
        let serialized = String::from_utf8(article.serialize(b"")).unwrap();
        assert!(!serialized.contains("Date:"));
        assert!(!serialized.contains("X-No-Archive"));
    }

    #[test]
    fn format_rfc2822_leap_day() {
        // 2024-02-29 00:00:00 UTC — verified: days from epoch = 19782.
        // 19782 * 86400 = 1709164800
        let t = UNIX_EPOCH + Duration::from_secs(1709164800);
        let s = format_rfc2822(t);
        assert_eq!(s, "Thu, 29 Feb 2024 00:00:00 +0000");
    }

    #[test]
    fn default_subject_single_part_has_no_parens() {
        let s = default_subject("movie.mkv", 1, 1);
        assert!(!s.contains('('));
        assert_eq!(s, "movie.mkv");
    }

    #[test]
    fn default_subject_last_part() {
        assert_eq!(default_subject("f.bin", 10, 10), "f.bin (10/10)");
    }

    // ── format_rfc2822 additional edge cases ──────────────────────────────────

    #[test]
    fn format_rfc2822_year_end() {
        // 2023-12-31 23:59:59 UTC — verified: 1704067199
        let t = UNIX_EPOCH + Duration::from_secs(1704067199);
        let s = format_rfc2822(t);
        assert_eq!(s, "Sun, 31 Dec 2023 23:59:59 +0000");
    }

    #[test]
    fn format_rfc2822_non_leap_year_feb28() {
        // 2023-02-28 00:00:00 UTC — verified: 1677542400
        let t = UNIX_EPOCH + Duration::from_secs(1677542400);
        let s = format_rfc2822(t);
        assert_eq!(s, "Tue, 28 Feb 2023 00:00:00 +0000");
    }

    #[test]
    fn format_rfc2822_midnight_fields_are_zero_padded() {
        // Any midnight timestamp — hours, minutes, seconds must be "00".
        let t = UNIX_EPOCH + Duration::from_secs(86400); // 1970-01-02 00:00:00
        let s = format_rfc2822(t);
        assert!(s.ends_with("00:00:00 +0000"), "got: {s}");
    }

    // ── serialize edge cases ──────────────────────────────────────────────────

    #[test]
    fn serialize_preserves_binary_body_verbatim() {
        let article = Article {
            message_id: "<id@x>".into(),
            from: "p <p@x.com>".into(),
            newsgroups: vec!["alt.test".into()],
            subject: "f".into(),
            date: None,
            no_archive: false,
        };
        let body: Vec<u8> = (0u8..=255).collect();
        let out = article.serialize(&body);
        // The headers end with \r\n\r\n; everything after is the raw body.
        let sep = b"\r\n\r\n";
        let body_start = out.windows(sep.len()).position(|w| w == sep).unwrap() + sep.len();
        assert_eq!(&out[body_start..], body.as_slice());
    }

    #[test]
    fn serialize_single_newsgroup_has_no_comma() {
        let article = Article {
            message_id: "<id@x>".into(),
            from: "p <p@x.com>".into(),
            newsgroups: vec!["alt.binaries.test".into()],
            subject: "f".into(),
            date: None,
            no_archive: false,
        };
        let out = String::from_utf8(article.serialize(b"")).unwrap();
        let ng_line = out.lines().find(|l| l.starts_with("Newsgroups:")).unwrap();
        assert!(!ng_line.contains(','));
        assert!(ng_line.contains("alt.binaries.test"));
    }

    #[test]
    fn serialize_zero_length_body_header_ends_with_double_crlf() {
        // Zero-length file: the body after encoding is empty. The serialized
        // article must still end with \r\n\r\n (blank line after headers).
        let article = Article {
            message_id: "<id@x>".into(),
            from: "p <p@x.com>".into(),
            newsgroups: vec!["alt.test".into()],
            subject: "empty.bin".into(),
            date: None,
            no_archive: false,
        };
        let out = article.serialize(b"");
        assert!(out.ends_with(b"\r\n\r\n"));
    }
}
