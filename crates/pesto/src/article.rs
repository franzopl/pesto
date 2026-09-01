//! Article assembly: the Usenet headers (`From`, `Newsgroups`, `Subject`,
//! `Message-ID`, `Date`) wrapped around a yEnc-encoded body, plus unique
//! `Message-ID` generation.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rand::Rng;

/// Generate an opaque `Message-ID` with a 128-bit random local-part.
///
/// When `fixed_domain` is `Some`, that value is used as the domain; otherwise
/// a freshly randomised label is generated so no fixed identifier leaks through
/// the Message-ID header. The local-part deliberately contains no clock or
/// counter: download clients treat it as opaque, while exposing either value
/// gives a passive observer a reliable posting-order fingerprint.
pub fn generate_message_id(fixed_domain: Option<&str>) -> String {
    let random: u128 = rand::random();
    let domain = match fixed_domain {
        Some(d) => d.to_string(),
        None => {
            let mut rng = rand::rng();
            let label = random_alpha(rng.random_range(8..=15));
            let tld = ["com", "net", "org"][rng.random_range(0..3)];
            format!("{label}.{tld}")
        }
    };
    format!("<{random:032x}@{domain}>")
}

pub(crate) fn valid_message_id_domain(domain: &str) -> bool {
    !domain.is_empty()
        && domain.len() <= 253
        && domain.split('.').all(|label| {
            !label.is_empty()
                && label.len() <= 63
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
                && label
                    .as_bytes()
                    .first()
                    .is_some_and(u8::is_ascii_alphanumeric)
                && label
                    .as_bytes()
                    .last()
                    .is_some_and(u8::is_ascii_alphanumeric)
        })
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

/// Neutralize CR/LF (and other C0) in a header value so a file name cannot
/// inject extra header lines. Space is the replacement so the line stays one
/// token-ish field rather than vanishing.
fn sanitize_header_value(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c == '\r' || c == '\n' || c == '\0' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

impl Article {
    /// Build the RFC 2822 header block (including the trailing blank line).
    /// The returned bytes are ready to be written directly to the NNTP stream.
    pub fn build_headers(&self) -> Vec<u8> {
        let mut h = format!(
            "From: {}\r\nNewsgroups: {}\r\nSubject: {}\r\nMessage-ID: {}\r\n",
            sanitize_header_value(&self.from),
            sanitize_header_value(&self.newsgroups.join(",")),
            sanitize_header_value(&self.subject),
            sanitize_header_value(&self.message_id),
        );
        if let Some(date) = &self.date {
            h.push_str("Date: ");
            h.push_str(&sanitize_header_value(date));
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
    /// [`Self::build_headers`] and posts headers + body separately to avoid copying
    /// the body.
    pub fn serialize(&self, body: &[u8]) -> Vec<u8> {
        let mut out = self.build_headers();
        out.extend_from_slice(body);
        out
    }
}

/// Generate a random 32-hex-character name, used to obfuscate the subject and
/// yEnc file name when posting. Each call yields a fresh value.
/// Generate a random obfuscated name for use as a subject or yEnc `name=`.
///
/// Uses a variable length (10–30 chars) and an alphanumeric charset
/// (`[A-Za-z0-9]`) so the output has no fixed-length or hex-only fingerprint
/// that would identify it as pesto-generated. Inspired by juicenet's schizo
/// mode variable-length randomisation.
pub fn obfuscated_name() -> String {
    let len = rand::rng().random_range(10..=30);
    random_alnum(len)
}

/// Generate an obfuscated name that starts with `prefix` followed by a random
/// alphanumeric suffix, e.g. `obfuscated_name_with_prefix("aB3xY9")` ->
/// `"aB3xY9-k2Qmz8Fh"`. Used by `ObfuscateMode::FullShared`'s yEnc `name=` so
/// an indexer can still recognise every article of a release as belonging
/// together by the shared leading token, without the exact Subject/yEnc match
/// that `obfuscated_name()` alone avoids (see pesto issue #106 — a plain
/// independent yEnc name left indexers with no wire-visible signal at all to
/// group the release by, once the Subject/yEnc match was removed).
pub fn obfuscated_name_with_prefix(prefix: &str) -> String {
    let len = rand::rng().random_range(8..=22);
    format!("{prefix}-{}", random_alnum(len))
}

/// A fresh 64-bit value from rand's thread-local cryptographic generator.
pub(crate) fn rand_u64() -> u64 {
    rand::random()
}

/// Build a string of `len` random alphanumeric characters (`[A-Za-z0-9]`).
fn random_alnum(len: usize) -> String {
    const ALNUM: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let mut rng = rand::rng();
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        s.push(ALNUM[rng.random_range(0..ALNUM.len())] as char);
    }
    s
}

/// Build a string of `len` random lowercase ASCII letters.
fn random_alpha(len: usize) -> String {
    const ALPHA: &[u8] = b"abcdefghijklmnopqrstuvwxyz";
    let mut rng = rand::rng();
    let mut s = String::with_capacity(len);
    for _ in 0..len {
        s.push(ALPHA[rng.random_range(0..ALPHA.len())] as char);
    }
    s
}

/// Generate a random `From` header of the form `Name <local@domain.tld>`.
///
/// All components use variable lengths and alphanumeric charsets. The TLD is
/// a random 2–5 char string instead of a real TLD, matching juicenet's schizo
/// mode to avoid fingerprinting by TLD pattern.
pub fn random_from() -> String {
    let mut rng = rand::rng();
    let name = random_alpha(rng.random_range(5..=12));
    let local = random_alnum(rng.random_range(10..=20));
    let domain = random_alnum(rng.random_range(5..=10));
    let tld = random_alpha(rng.random_range(2..=5));
    let mut display = name;
    display[..1].make_ascii_uppercase();
    format!("{display} <{local}@{domain}.{tld}>")
}

/// Build a default subject line for one part of a file.
///
/// Loosely follows the yEnc draft v1.3 specification:
/// - Primary: <http://www.yenc.org/yenc-draft.1.3.txt>
/// - Mirror:  <https://github.com/caronc/newsreap/blob/master/docs/yenc-draft.1.3.txt>
///
/// Format: `"name" yEnc (part/total)`, always — including `(1/1)` for a file
/// that fits in a single segment. The spec itself allows omitting the
/// `(part/total)` trailer in that case, and pesto used to; a small file (the
/// bare PAR2 index in a `--compress`/`--par2` release is the common case, but
/// any file — `.nfo`, `.sfv`, a loose small upload — posted single-segment
/// alongside multi-segment siblings hits the same thing) then had a subject
/// shaped differently from the rest of the release. Confirmed live against
/// Binsearch (issue #68): several indexers' "collection cleaning" regexes
/// key off that trailer to recover a release's shared base name, so the
/// single-segment file's differently-shaped subject hashed it into a
/// separate collection — it never joined the rest of the release, even
/// though every file shared the same name/prefix otherwise. Always emitting
/// `(1/1)` gives every file in a release the same subject shape and fixed
/// that grouping gap in a real repro; no known downside, since `(1/1)` is
/// still a spec-valid subject.
///
/// `file_counter`, when `Some((filenum, total_files))`, prepends a
/// `[filenum/total_files] ` release-wide file counter — `filenum` is this
/// file's 1-based position among every file in the release (data files plus
/// the PAR2 index and volumes), `total_files` the release's grand total. This
/// is the `--file-counter` opt-in (see `Config::file_counter`); off by
/// default. See ROADMAP.md "Subject file counter" for why pesto doesn't
/// compute this by default and how the geometry is known ahead of encoding.
pub fn default_subject(
    name: &str,
    part: u32,
    total: u32,
    file_counter: Option<(u32, u32)>,
) -> String {
    let name = name.replace('"', "'");
    let base = format!("\"{name}\" yEnc ({part}/{total})");
    match file_counter {
        Some((filenum, total_files)) => format!("[{filenum}/{total_files}] - {base}"),
        None => base,
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
    fn message_id_domain_validation_rejects_header_injection_and_bad_labels() {
        for valid in ["example.com", "news-01.example", "localhost"] {
            assert!(valid_message_id_domain(valid), "{valid}");
        }
        for invalid in [
            "",
            ".example",
            "example.",
            "-bad.example",
            "bad-.example",
            "a b",
            "x\r\nSubject: leak",
        ] {
            assert!(!valid_message_id_domain(invalid), "{invalid}");
        }
    }

    #[test]
    fn message_ids_are_unique() {
        let a = generate_message_id(None);
        let b = generate_message_id(None);
        assert_ne!(a, b);
    }

    #[test]
    fn message_id_local_part_is_opaque_128_bit_hex() {
        let id = generate_message_id(Some("example.com"));
        let local = id
            .strip_prefix('<')
            .and_then(|id| id.strip_suffix("@example.com>"))
            .unwrap();
        assert_eq!(local.len(), 32);
        assert!(local
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase()));
        assert!(
            !local.contains('.'),
            "legacy clock/counter separators leaked"
        );
    }

    #[test]
    fn message_ids_remain_unique_across_a_large_batch() {
        let mut ids = std::collections::HashSet::with_capacity(10_000);
        for _ in 0..10_000 {
            assert!(ids.insert(generate_message_id(Some("example.com"))));
        }
    }

    #[test]
    fn serialize_emits_headers_then_blank_line_then_body() {
        let article = Article {
            message_id: "<id@pesto>".into(),
            from: "poster <p@example.com>".into(),
            newsgroups: vec!["alt.binaries.test".into(), "alt.binaries.misc".into()],
            subject: "\"file.bin\" yEnc (1/2)".into(),
            date: None,
            no_archive: false,
        };
        let serialized = String::from_utf8(article.serialize(b"BODY")).unwrap();
        assert!(serialized.contains("From: poster <p@example.com>\r\n"));
        assert!(serialized.contains("Newsgroups: alt.binaries.test,alt.binaries.misc\r\n"));
        assert!(serialized.contains("Subject: \"file.bin\" yEnc (1/2)\r\n"));
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
        assert_eq!(
            default_subject("file.bin", 1, 1, None),
            "\"file.bin\" yEnc (1/1)"
        );
        assert_eq!(
            default_subject("file.bin", 2, 5, None),
            "\"file.bin\" yEnc (2/5)"
        );
    }

    #[test]
    fn default_subject_with_file_counter_prefixes_release_position() {
        assert_eq!(
            default_subject("file.bin", 2, 5, Some((3, 15))),
            "[3/15] - \"file.bin\" yEnc (2/5)"
        );
        assert_eq!(
            default_subject("file.bin", 1, 1, Some((1, 1))),
            "[1/1] - \"file.bin\" yEnc (1/1)"
        );
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
    fn obfuscated_name_is_variable_length_alnum() {
        for _ in 0..50 {
            let s = obfuscated_name();
            assert!(
                (10..=30).contains(&s.len()),
                "expected length 10..=30, got {}",
                s.len()
            );
            assert!(
                s.chars().all(|c| c.is_ascii_alphanumeric()),
                "non-alphanumeric char in `{s}`"
            );
        }
        // uniqueness
        assert_ne!(obfuscated_name(), obfuscated_name());
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
    fn default_subject_single_part_still_carries_1_of_1() {
        // Regression for issue #68: a single-segment file (e.g. the bare
        // PAR2 index) must carry the same `(part/total)` shape as every
        // multi-segment sibling in the release, or some indexers' subject
        // parsing hashes it into a separate collection instead of grouping
        // it with the rest — confirmed live against Binsearch.
        let s = default_subject("movie.mkv", 1, 1, None);
        assert_eq!(s, "\"movie.mkv\" yEnc (1/1)");
    }

    #[test]
    fn default_subject_replaces_embedded_quotes() {
        assert_eq!(
            default_subject("say \"hi\".bin", 1, 1, None),
            "\"say 'hi'.bin\" yEnc (1/1)"
        );
    }

    #[test]
    fn build_headers_neutralizes_crlf_in_subject() {
        let article = Article {
            message_id: "<id@pesto>".into(),
            from: "p <p@x.com>".into(),
            newsgroups: vec!["a.b.test".into()],
            subject: "ok\r\nX-Injected: yes".into(),
            date: None,
            no_archive: false,
        };
        let headers = String::from_utf8(article.build_headers()).unwrap();
        assert!(!headers.lines().any(|l| l.starts_with("X-Injected")));
        assert!(headers.contains("Subject: ok  X-Injected: yes\r\n"));
    }

    #[test]
    fn default_subject_last_part() {
        assert_eq!(
            default_subject("f.bin", 10, 10, None),
            "\"f.bin\" yEnc (10/10)"
        );
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
