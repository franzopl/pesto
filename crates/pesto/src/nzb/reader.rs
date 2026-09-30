//! NZB parsing: reconstructing posted segments from NZB 1.1 XML.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::poster::{PostedSegment, SegmentIdentity};

use super::{NzbMeta, ParsedNzb};

/// Parse a string containing a segmentIndex attribute into a canonical 32-bit unsigned integer.
///
/// Enforces Section 8 strict syntax rules:
/// - non-empty
/// - strictly ASCII digits `0..=9`
/// - non-zero
/// - no leading zeros (unless length is 1 and it's handled, but 0 is forbidden so no leading zeros at all)
/// - no signs (+, -)
/// - no whitespace
/// - bounds: `1..=4294967295`
pub fn parse_segment_index(val_str: &str) -> Result<u32> {
    if val_str.is_empty() {
        bail!("INVALID_SEGMENT_INDEX_EMPTY");
    }
    if val_str == "0" {
        bail!("INVALID_SEGMENT_INDEX_ZERO");
    }
    if val_str.starts_with('+') || val_str.starts_with('-') {
        bail!("INVALID_SEGMENT_INDEX_SIGN");
    }
    if val_str != val_str.trim()
        || val_str
            .bytes()
            .any(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
    {
        bail!("INVALID_SEGMENT_INDEX_WHITESPACE");
    }
    if val_str.len() > 1 && val_str.starts_with('0') && val_str.bytes().all(|b| b.is_ascii_digit())
    {
        bail!("INVALID_SEGMENT_INDEX_LEADING_ZERO");
    }
    if !val_str.bytes().all(|b| b.is_ascii_digit()) {
        bail!("INVALID_SEGMENT_INDEX_NON_DIGIT");
    }
    let parsed: u64 = val_str
        .parse()
        .map_err(|_| anyhow::anyhow!("INVALID_SEGMENT_INDEX_OVERFLOW"))?;
    if parsed > u32::MAX as u64 {
        bail!("INVALID_SEGMENT_INDEX_OVERFLOW");
    }
    Ok(parsed as u32)
}

struct RawSegment {
    file_name: String,
    subject_name: String,
    poster: String,
    date: Option<u64>,
    bytes: u64,
    part: u32,
    total: u32,
    message_id: String,
    file_ordinal: u32,
    total_files: u32,
    raw_segment_index: Option<String>,
}

/// Parse a `.nzb` document and reconstruct its [`PostedSegment`] list.
///
/// Unencrypted releases and archive-password-only releases parse with `segment_identities: None`.
/// Encrypted releases (indicated by `<meta type="yenc_encrypted">true</meta>` or explicit
/// `segmentIndex` attributes with `<meta type="password">`) undergo strict Section 8 validation.
pub fn parse(content: &str) -> Result<ParsedNzb> {
    parse_internal(content, false)
}

/// Parse an encrypted `.nzb` document, strictly enforcing explicit `segmentIndex` on every segment.
///
/// Fails closed if any segment lacks `segmentIndex` or carries invalid index formatting.
pub fn parse_encrypted(content: &str) -> Result<ParsedNzb> {
    parse_internal(content, true)
}

fn parse_internal(content: &str, force_encrypted: bool) -> Result<ParsedNzb> {
    let mut poster = String::new();
    let mut groups: Vec<String> = Vec::new();
    let mut meta = NzbMeta::default();
    let mut explicit_yenc_encrypted = false;

    let mut current_file_name = String::new();
    let mut current_subject_name = String::new();
    let mut current_poster = String::new();
    let mut current_date: Option<u64> = None;
    let mut current_file_ordinal = 0u32;
    let mut current_total_files = 0u32;
    let mut file_segment_start: usize = 0;
    let mut in_groups = false;
    let mut in_file = false;

    let mut raw_segments: Vec<RawSegment> = Vec::new();

    let mut pending = String::new();
    let mut tags: Vec<String> = Vec::new();

    for line in content.lines() {
        if pending.is_empty() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            pending.push_str(trimmed);
        } else {
            pending.push('\n');
            pending.push_str(line.trim_end());
        }

        if pending.starts_with('<') {
            let quotes = pending.bytes().filter(|&b| b == b'"').count();
            if quotes % 2 != 0 || !pending.contains('>') {
                continue;
            }
        }

        tags.push(std::mem::take(&mut pending));
    }
    if !pending.is_empty() {
        tags.push(pending);
    }

    for t in &tags {
        let t = t.trim();
        if t.starts_with("<file ") {
            in_file = true;
            in_groups = false;
            current_poster = xml_attr(t, "poster").unwrap_or_default();
            if poster.is_empty() {
                poster = current_poster.clone();
            }
            current_date = xml_attr(t, "date").and_then(|s| s.parse().ok());
            let subject = xml_attr(t, "subject").unwrap_or_default();

            match parse_file_counter(&subject) {
                Some((n, m, _residual)) => {
                    current_file_ordinal = n;
                    current_total_files = m;
                }
                None => {
                    current_file_ordinal = 0;
                    current_total_files = 0;
                }
            }

            current_subject_name = strip_part_suffix(&subject);
            current_file_name = current_subject_name.clone();
            file_segment_start = raw_segments.len();
        } else if t == "</file>" {
            let total = (raw_segments.len() - file_segment_start) as u32;
            for seg in &mut raw_segments[file_segment_start..] {
                seg.total = total;
            }
            in_file = false;
        } else if t == "<groups>" {
            in_groups = true;
        } else if t == "</groups>" {
            in_groups = false;
        } else if in_groups {
            if let Some(g) = xml_text(t, "group") {
                if !groups.contains(&g) {
                    groups.push(g);
                }
            }
        } else if in_file && t.starts_with("<segment ") {
            let bytes: u64 = xml_attr(t, "bytes")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let part: u32 = xml_attr(t, "number")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0);
            let raw_id = xml_text(t, "segment").unwrap_or_default();
            let message_id = if raw_id.starts_with('<') {
                raw_id
            } else {
                format!("<{raw_id}>")
            };
            let raw_segment_index = xml_attr(t, "segmentIndex");

            raw_segments.push(RawSegment {
                file_name: current_file_name.clone(),
                subject_name: current_subject_name.clone(),
                poster: current_poster.clone(),
                date: current_date,
                bytes,
                part,
                total: 0,
                message_id,
                file_ordinal: current_file_ordinal,
                total_files: current_total_files,
                raw_segment_index,
            });
        } else if t.starts_with("<meta ") {
            let kind = xml_attr(t, "type").unwrap_or_default();
            let value = xml_text(t, "meta").unwrap_or_default();
            match kind.as_str() {
                "title" => meta.name = Some(value),
                "password" => meta.password = Some(value),
                "category" => meta.category = Some(value),
                "tag" => meta.tags.push(value),
                "yenc_encrypted" if value == "true" => {
                    explicit_yenc_encrypted = true;
                }
                "yenc_version" => {
                    meta.yenc_version = Some(value.clone());
                }
                "yenc_cipher" => {
                    meta.yenc_cipher = Some(value.clone());
                }
                _ => {}
            }
        }
    }

    let any_segment_has_index = raw_segments.iter().any(|s| s.raw_segment_index.is_some());
    let is_encrypted = force_encrypted
        || explicit_yenc_encrypted
        || (meta.password.is_some() && any_segment_has_index);

    if is_encrypted {
        if let Some(ref ver) = meta.yenc_version {
            if ver != "1.0" {
                bail!("unsupported yenc_version in nzb: {ver}");
            }
        }
        if let Some(ref cipher) = meta.yenc_cipher {
            if cipher != "XChaCha20-Poly1305" {
                bail!("unsupported yenc_cipher in nzb: {cipher}");
            }
        }
        meta.yenc_encrypted = true;
        let mut segment_identities = HashMap::new();
        let mut seen_indices = HashSet::new();
        let mut seen_message_ids: HashMap<String, u32> = HashMap::new();
        let mut segments = Vec::with_capacity(raw_segments.len());

        for raw in raw_segments {
            let seg_idx_str = raw
                .raw_segment_index
                .as_deref()
                .context("MISSING_SEGMENT_INDEX")?;
            let seg_idx = parse_segment_index(seg_idx_str)?;

            if let Some(&existing_idx) = seen_message_ids.get(&raw.message_id) {
                if existing_idx != seg_idx {
                    bail!("CONFLICTING_MESSAGE_ID_INDEX");
                }
            } else {
                seen_message_ids.insert(raw.message_id.clone(), seg_idx);
            }

            if !seen_indices.insert(seg_idx) {
                bail!("DUPLICATE_SEGMENT_INDEX");
            }

            let identity = SegmentIdentity::explicit(0, 0, raw.part, seg_idx)
                .context("failed to construct explicit segment identity")?;

            segment_identities.insert(raw.message_id.clone(), identity);

            segments.push(PostedSegment {
                file_name: raw.file_name.clone(),
                file_path: Arc::from(Path::new(&raw.file_name)),
                subject_name: Arc::from(raw.subject_name.as_str()),
                wire_name: Arc::from(""),
                wire_yenc_name: Arc::from(""),
                file_size: 0,
                part: raw.part,
                total: raw.total,
                message_id: raw.message_id,
                bytes: raw.bytes,
                from: Arc::from(raw.poster.as_str()),
                date: (None, raw.date),
                full_crc32: 0,
                server_idx: 0,
                file_index: raw.file_ordinal,
                total_files: raw.total_files,
                segment_identity: Some(identity),
            });
        }

        segments.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.part.cmp(&b.part)));

        Ok(ParsedNzb {
            poster,
            groups,
            segments,
            meta,
            segment_identities: Some(segment_identities),
        })
    } else {
        meta.yenc_encrypted = false;
        let mut segments = Vec::with_capacity(raw_segments.len());
        for raw in raw_segments {
            segments.push(PostedSegment {
                file_name: raw.file_name.clone(),
                file_path: Arc::from(Path::new(&raw.file_name)),
                subject_name: Arc::from(raw.subject_name.as_str()),
                wire_name: Arc::from(""),
                wire_yenc_name: Arc::from(""),
                file_size: 0,
                part: raw.part,
                total: raw.total,
                message_id: raw.message_id,
                bytes: raw.bytes,
                from: Arc::from(raw.poster.as_str()),
                date: (None, raw.date),
                full_crc32: 0,
                server_idx: 0,
                file_index: raw.file_ordinal,
                total_files: raw.total_files,
                segment_identity: None,
            });
        }

        segments.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.part.cmp(&b.part)));

        Ok(ParsedNzb {
            poster,
            groups,
            segments,
            meta,
            segment_identities: None,
        })
    }
}

/// Extract the value of `name="..."` from an XML tag string.
fn xml_attr(tag: &str, name: &str) -> Option<String> {
    let key = format!("{name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(xml_unescape(&tag[start..end]))
}

/// Extract text content from `<tag ...>text</tag>` on a single line.
fn xml_text(line: &str, tag: &str) -> Option<String> {
    let open_end = line.find('>')?;
    let close = format!("</{tag}>");
    let close_start = line.rfind(&close)?;
    if close_start < open_end + 1 {
        return None;
    }
    Some(xml_unescape(&line[open_end + 1..close_start]))
}

/// Strip the `"name" yEnc (N/M)` or `"name" yEnc` wrapper from a subject line.
///
/// Handles both the current yEnc-spec format and the legacy `name (N/M)` format.
/// Backward compatibility is needed because `pesto --merge-season` can read
/// NZBs created before the yEnc spec fix (e.g. subjects like `movie.mkv (1/3)`).
pub(super) fn strip_part_suffix(subject: &str) -> String {
    let unquote = |s: &str| {
        if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
            s[1..s.len() - 1].to_string()
        } else {
            s.to_string()
        }
    };

    // New format: "name" yEnc (N/M)  →  strip " yEnc (N/M)"
    // New format: "name" yEnc        →  strip " yEnc"
    if let Some(pos) = subject.rfind(" yEnc") {
        let tail = &subject[pos + 5..];
        if tail.is_empty() || tail.starts_with(" (") {
            return unquote(strip_filenum_prefix(&subject[..pos]));
        }
    }

    // Legacy format: name (N/M)
    if let Some(pos) = subject.rfind(" (") {
        let tail = &subject[pos..];
        if tail.contains('/') && tail.ends_with(')') {
            return subject[..pos].to_string();
        }
    }
    subject.to_string()
}

/// Parse a leading `[N/M]` file counter from a subject string.
///
/// Returns `Some((file_ordinal, total_files, residual))` when the subject
/// begins with a well-formed `[N/M] - ...` counter where both `N` and `M`
/// are non-zero ASCII decimal integers that fit in `u32`. The residual is
/// the part of the subject after the `] - ` separator.
///
/// Returns `None` when the prefix is absent, malformed, or contains zero
/// values — the subject is then treated as ordinary with no counter.
pub fn parse_file_counter(s: &str) -> Option<(u32, u32, &str)> {
    let rest = s.strip_prefix('[')?;
    let (counter, after) = rest.split_once(']')?;
    let (n_str, m_str) = counter.split_once('/')?;
    // Both parts must be non-empty pure ASCII digits.
    if n_str.is_empty()
        || m_str.is_empty()
        || !n_str.bytes().all(|b| b.is_ascii_digit())
        || !m_str.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    // Reject counters with more than one `/` (e.g. `[1/2/3]`).
    if counter.matches('/').count() != 1 {
        return None;
    }
    let n: u32 = n_str.parse().ok()?;
    let m: u32 = m_str.parse().ok()?;
    if n == 0 || m == 0 {
        return None;
    }
    let residual = after.strip_prefix(" - ")?;
    Some((n, m, residual))
}

/// Strip a leading `[filenum/files] - ` counter, if present, returning only
/// the residual. This is the legacy compatibility wrapper around
/// [`parse_file_counter`].
fn strip_filenum_prefix(s: &str) -> &str {
    match parse_file_counter(s) {
        Some((_, _, residual)) => residual,
        None => s,
    }
}

/// Reverse the XML entity escaping applied by [`escape`].
fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}
