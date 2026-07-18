//! yEnc decoder: turns an article body back into raw file bytes plus the
//! metadata carried in its `=ybegin`/`=ypart`/`=yend` control lines.
//!
//! Reference: the yEnc draft specification, <http://www.yenc.org/yenc-draft.1.3.txt>.
//!
//! Operates on already dot-unstuffed bytes — NNTP-level dot-stuffing (RFC
//! 3977 §3.1.1) is a transport concern handled by
//! [`crate::nntp::Connection::body`], one layer below this. yEnc separately
//! escapes a `.` at the start of a *data* line for the same reason (see
//! `scalar::encode_scalar`'s doc comment), so by the time bytes reach this
//! decoder no literal `.` can appear unescaped at a line start regardless.
//!
//! Decoding is done one line at a time: the encoder never splits an escape
//! pair (`=` + payload byte) across a line boundary (see `encode_scalar`,
//! where both escape bytes are written before the line-wrap check runs), so
//! resetting escape state at each line start is always correct.

use anyhow::{Context, Result};

use super::crc32;

/// One decoded yEnc part, with its `=ybegin`/`=ypart`/`=yend` metadata.
#[derive(Debug, Clone)]
pub struct DecodedPart {
    /// Filename from `=ybegin name=`. Not necessarily authoritative — a
    /// downloader should prefer the `.nzb`'s `file_name`, which is never
    /// obfuscated (see `pesto::nzb`), over this field.
    pub name: String,
    /// Source line length from `=ybegin line=`.
    pub line_len: usize,
    /// Whole-file size from `=ybegin size=`.
    pub file_size: u64,
    /// 1-based part number (`1` when `=ybegin` carries no `part=`, i.e. a
    /// single-part file).
    pub part: u32,
    /// Total part count (`1` for a single-part file).
    pub total: u32,
    /// 1-based offset of this part's first byte within the file.
    pub begin: u64,
    /// 1-based offset of this part's last byte within the file.
    pub end: u64,
    /// Decoded raw bytes.
    pub data: Vec<u8>,
    /// This part's CRC-32, from `=yend pcrc32=` (multi-part) or `=yend
    /// crc32=` (single-part). `None` if the sending client omitted it
    /// (allowed by the spec, though `pesto` itself always sends it).
    pub part_crc32: Option<u32>,
    /// Whole-file CRC-32, from `=yend crc32=`. Only ever present on a
    /// multi-part `=yend`, and only some encoders bother (`pesto` doesn't
    /// know it until every part has been produced).
    pub file_crc32: Option<u32>,
}

impl DecodedPart {
    /// Whether `data`'s CRC-32 matches `part_crc32`. `true` when no
    /// `part_crc32` was present to check against — the caller decides how
    /// to treat "unknown" versus "checked and matched".
    pub fn crc_matches(&self) -> bool {
        match self.part_crc32 {
            Some(expected) => crc32(&self.data) == expected,
            None => true,
        }
    }
}

/// Decode a complete yEnc article body (as returned by
/// [`crate::nntp::Connection::body`]) into a [`DecodedPart`].
///
/// Fails if the `=ybegin`/`=yend` control lines are missing or malformed, or
/// (for a multi-part body) if `=ypart` is missing. Does not fail on a CRC
/// mismatch — check [`DecodedPart::crc_matches`] for that.
pub fn decode_part(body: &[u8]) -> Result<DecodedPart> {
    let lines = split_lines(body);

    let ybegin_idx = lines
        .iter()
        .position(|l| l.starts_with(b"=ybegin"))
        .context("no =ybegin line found in article body")?;
    let ybegin = parse_ybegin(lines[ybegin_idx])?;

    let mut idx = ybegin_idx + 1;
    let (begin, end) = if ybegin.total > 1 {
        let line = *lines
            .get(idx)
            .context("multi-part article is missing its =ypart line")?;
        anyhow::ensure!(
            line.starts_with(b"=ypart"),
            "expected =ypart line after multi-part =ybegin, found something else"
        );
        let span = parse_ypart(line)?;
        idx += 1;
        span
    } else {
        (1, ybegin.size)
    };

    let yend_idx = lines[idx..]
        .iter()
        .position(|l| l.starts_with(b"=yend"))
        .map(|p| p + idx)
        .context("no =yend line found in article body")?;

    let mut data = Vec::with_capacity((end.saturating_sub(begin) + 1) as usize);
    for line in &lines[idx..yend_idx] {
        decode_data_line(&mut data, line);
    }

    let yend = parse_yend(lines[yend_idx])?;
    let (part_crc32, file_crc32) = if ybegin.total > 1 {
        (yend.pcrc32, yend.crc32)
    } else {
        (yend.crc32, yend.crc32)
    };

    Ok(DecodedPart {
        name: ybegin.name,
        line_len: ybegin.line_len,
        file_size: ybegin.size,
        part: ybegin.part,
        total: ybegin.total,
        begin,
        end,
        data,
        part_crc32,
        file_crc32,
    })
}

/// Split `body` into lines on `\n`, stripping a trailing `\r` from each and
/// dropping the trailing empty slice a final `\n` would otherwise produce.
fn split_lines(body: &[u8]) -> Vec<&[u8]> {
    let mut lines: Vec<&[u8]> = body
        .split(|&b| b == b'\n')
        .map(|l| l.strip_suffix(b"\r").unwrap_or(l))
        .collect();
    if lines.last().is_some_and(|l| l.is_empty()) {
        lines.pop();
    }
    lines
}

/// Decode one already dot-unstuffed, CRLF-stripped data line, appending the
/// result to `out`.
fn decode_data_line(out: &mut Vec<u8>, line: &[u8]) {
    let mut i = 0;
    while i < line.len() {
        if line[i] == b'=' && i + 1 < line.len() {
            out.push(line[i + 1].wrapping_sub(64).wrapping_sub(42));
            i += 2;
        } else {
            out.push(line[i].wrapping_sub(42));
            i += 1;
        }
    }
}

struct YBegin {
    part: u32,
    total: u32,
    line_len: usize,
    size: u64,
    name: String,
}

/// Parse a `=ybegin [part=N total=N] line=N size=N name=NAME` line.
///
/// `name=` always comes last and its value runs to the end of the line
/// (filenames may contain spaces), so it is split off by locating the
/// literal `name=` marker rather than by whitespace tokenizing.
fn parse_ybegin(line: &[u8]) -> Result<YBegin> {
    let rest = line
        .strip_prefix(b"=ybegin ")
        .context("malformed =ybegin line")?;
    let name_pos = find_subslice(rest, b"name=").context("=ybegin line missing name=")?;
    let (kv, name_field) = rest.split_at(name_pos);
    let name = String::from_utf8_lossy(&name_field[b"name=".len()..]).into_owned();

    let kv = std::str::from_utf8(kv).context("=ybegin line has non-ASCII key=value fields")?;
    let mut part = None;
    let mut total = None;
    let mut line_len = None;
    let mut size = None;
    for tok in kv.split_whitespace() {
        if let Some(v) = tok.strip_prefix("part=") {
            part = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("total=") {
            total = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("line=") {
            line_len = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("size=") {
            size = v.parse().ok();
        }
    }

    Ok(YBegin {
        part: part.unwrap_or(1),
        total: total.unwrap_or(1),
        line_len: line_len.context("=ybegin line missing line=")?,
        size: size.context("=ybegin line missing size=")?,
        name,
    })
}

/// Parse a `=ypart begin=N end=N` line into `(begin, end)`.
fn parse_ypart(line: &[u8]) -> Result<(u64, u64)> {
    let rest = std::str::from_utf8(line)
        .context("=ypart line is not ASCII")?
        .strip_prefix("=ypart ")
        .context("malformed =ypart line")?;
    let mut begin = None;
    let mut end = None;
    for tok in rest.split_whitespace() {
        if let Some(v) = tok.strip_prefix("begin=") {
            begin = v.parse().ok();
        } else if let Some(v) = tok.strip_prefix("end=") {
            end = v.parse().ok();
        }
    }
    Ok((
        begin.context("=ypart line missing begin=")?,
        end.context("=ypart line missing end=")?,
    ))
}

struct YEnd {
    pcrc32: Option<u32>,
    crc32: Option<u32>,
}

/// Parse a `=yend size=N [part=N] [pcrc32=HEX] [crc32=HEX]` line.
///
/// `size=` and `part=` are consumed by the caller via the surrounding
/// `=ybegin`/`=ypart` bookkeeping already collected, so only the two CRC
/// fields are extracted here.
fn parse_yend(line: &[u8]) -> Result<YEnd> {
    let rest = std::str::from_utf8(line)
        .context("=yend line is not ASCII")?
        .strip_prefix("=yend ")
        .context("malformed =yend line")?;
    let mut pcrc32 = None;
    let mut crc32 = None;
    for tok in rest.split_whitespace() {
        if let Some(v) = tok.strip_prefix("pcrc32=") {
            pcrc32 = u32::from_str_radix(v, 16).ok();
        } else if let Some(v) = tok.strip_prefix("crc32=") {
            crc32 = u32::from_str_radix(v, 16).ok();
        }
    }
    Ok(YEnd { pcrc32, crc32 })
}

/// Find the first occurrence of `needle` in `haystack`.
fn find_subslice(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yenc::{encode_part, PartSpec};

    #[test]
    fn round_trips_single_part() {
        let data: Vec<u8> = (0u8..=255).cycle().take(1000).collect();
        let encoded = encode_part(
            "movie.bin",
            data.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            &data,
            128,
            None,
        );

        let decoded = decode_part(&encoded.body).unwrap();
        assert_eq!(decoded.data, data);
        assert_eq!(decoded.name, "movie.bin");
        assert_eq!(decoded.line_len, 128);
        assert_eq!(decoded.file_size, data.len() as u64);
        assert_eq!(decoded.part, 1);
        assert_eq!(decoded.total, 1);
        assert_eq!(decoded.begin, 1);
        assert_eq!(decoded.end, data.len() as u64);
        assert!(decoded.crc_matches());
        assert_eq!(decoded.part_crc32, Some(crc32(&data)));
        assert_eq!(decoded.file_crc32, Some(crc32(&data)));
    }

    #[test]
    fn round_trips_multi_part_with_file_crc() {
        let data: Vec<u8> = (0u8..=255).cycle().take(500).collect();
        let encoded = encode_part(
            "movie.mkv",
            10_000,
            PartSpec {
                number: 2,
                total: 5,
                offset: 100,
            },
            &data,
            128,
            Some(0xDEAD_BEEF),
        );

        let decoded = decode_part(&encoded.body).unwrap();
        assert_eq!(decoded.data, data);
        assert_eq!(decoded.part, 2);
        assert_eq!(decoded.total, 5);
        assert_eq!(decoded.begin, 101);
        assert_eq!(decoded.end, 600);
        assert_eq!(decoded.part_crc32, Some(crc32(&data)));
        assert_eq!(decoded.file_crc32, Some(0xDEAD_BEEF));
        assert!(decoded.crc_matches());
    }

    #[test]
    fn round_trips_empty_part() {
        let encoded = encode_part(
            "empty.bin",
            0,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            &[],
            128,
            None,
        );
        let decoded = decode_part(&encoded.body).unwrap();
        assert!(decoded.data.is_empty());
        assert_eq!(decoded.begin, 1);
        assert_eq!(decoded.end, 0);
    }

    #[test]
    fn name_with_spaces_is_captured_in_full() {
        let data = b"hi".to_vec();
        let encoded = encode_part(
            "My Movie (2026).mkv",
            2,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            &data,
            128,
            None,
        );
        let decoded = decode_part(&encoded.body).unwrap();
        assert_eq!(decoded.name, "My Movie (2026).mkv");
    }

    #[test]
    fn crc_mismatch_is_detected_without_erroring() {
        // The body's data section is 8-bit yEnc output and not guaranteed
        // valid UTF-8, so corruption must be done at the byte level (a
        // whole-body `String` round-trip, as the control lines alone would
        // allow, is not safe here).
        let data = b"hello world".to_vec();
        let mut encoded = encode_part(
            "x.bin",
            data.len() as u64,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            &data,
            128,
            None,
        )
        .body;

        let real = format!("crc32={:08x}", crc32(&data));
        let bogus = format!("crc32={:08x}", crc32(&data) ^ 0xFFFF_FFFF);
        let pos = find_subslice(&encoded, real.as_bytes()).unwrap();
        encoded[pos..pos + real.len()].copy_from_slice(bogus.as_bytes());

        let decoded = decode_part(&encoded).unwrap();
        assert_eq!(decoded.data, data);
        assert!(!decoded.crc_matches());
    }

    #[test]
    fn missing_ybegin_is_an_error() {
        let err = decode_part(b"just some data\r\n=yend size=4 crc32=00000000\r\n").unwrap_err();
        assert!(err.to_string().contains("=ybegin"));
    }

    #[test]
    fn missing_yend_is_an_error() {
        let err = decode_part(b"=ybegin line=128 size=2 name=x\r\nhi\r\n").unwrap_err();
        assert!(err.to_string().contains("=yend"));
    }

    #[test]
    fn multipart_missing_ypart_is_an_error() {
        let err = decode_part(
            b"=ybegin part=1 total=2 line=128 size=2 name=x\r\nhi\r\n=yend size=2 part=1 pcrc32=0\r\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("=ypart"));
    }
}
