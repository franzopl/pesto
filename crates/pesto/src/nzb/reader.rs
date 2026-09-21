//! NZB parsing: reconstructing posted segments from NZB 1.1 XML.

use std::path::Path;
use std::sync::Arc;

use crate::poster::PostedSegment;

use super::{NzbMeta, ParsedNzb};

/// Parse a `.nzb` document and reconstruct its [`PostedSegment`] list.
///
/// The parser targets the format produced by [`crate::nzb::generate`] but
/// tolerates minor whitespace variation. Attributes must use double quotes.
/// Segments are sorted by `(file_name, part)` before returning so they can be
/// passed directly to [`crate::nzb::generate`].
pub fn parse(content: &str) -> anyhow::Result<ParsedNzb> {
    let mut poster = String::new();
    let mut groups: Vec<String> = Vec::new();
    let mut meta = NzbMeta::default();
    let mut segments: Vec<PostedSegment> = Vec::new();

    let mut current_file_name = String::new();
    let mut current_subject_name = String::new();
    let mut file_segment_start: usize = 0;
    let mut in_groups = false;
    let mut in_file = false;

    let mut current_poster = String::new();
    let mut current_date: Option<u64> = None;

    for line in content.lines() {
        let t = line.trim();

        if t.starts_with("<file ") {
            in_file = true;
            in_groups = false;
            current_poster = xml_attr(t, "poster").unwrap_or_default();
            if poster.is_empty() {
                poster = current_poster.clone();
            }
            current_date = xml_attr(t, "date").and_then(|s| s.parse().ok());
            let subject = xml_attr(t, "subject").unwrap_or_default();
            current_subject_name = strip_part_suffix(&subject);
            // Standard NZB 1.1 has no `name` attribute on `<file>` — only
            // `subject`, with the real name as the quoted string inside it,
            // exactly what `strip_part_suffix` already extracted into
            // `current_subject_name`. A fully obfuscated subject (no
            // quotes) yields the raw hash-like text here instead — not the
            // real name, but a valid starting point for `penne::
            // deobfuscate` to recover the true one from PAR2.
            current_file_name = current_subject_name.clone();
            file_segment_start = segments.len();
        } else if t == "</file>" {
            // Back-fill `total` now that we know how many segments this file has.
            let total = (segments.len() - file_segment_start) as u32;
            for seg in &mut segments[file_segment_start..] {
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
            segments.push(PostedSegment {
                file_name: current_file_name.clone(),
                // Segments parsed back from an existing `.nzb` (e.g. for
                // `--merge-season`) have no known source file on this
                // machine; `file_path` is only meaningful for segments
                // produced by a live upload, which is the only place a
                // post-check repost can use it.
                file_path: Arc::from(Path::new(&current_file_name)),
                subject_name: Arc::from(current_subject_name.as_str()),
                // Not recoverable from an .nzb (only `subject_name`, always
                // the real name, is written) — harmless for the same reason
                // as `full_crc32` below: never re-encoded once parsed back.
                wire_name: Arc::from(""),
                wire_yenc_name: Arc::from(""),
                file_size: 0,
                part,
                total: 0, // fixed up when </file> is seen
                message_id,
                bytes,
                from: Arc::from(current_poster.as_str()),
                date: (None, current_date),
                // Not recoverable from an .nzb (it only exists on the
                // `=yend` line of the last segment's body, which the .nzb
                // never carries) — harmless, since a segment parsed back
                // from an .nzb (--merge-season) is never re-encoded.
                full_crc32: 0,
                // A segment parsed back from an .nzb never re-enters the
                // check queue (see `PostedSegment::server_idx`).
                server_idx: 0,
                // `current_subject_name` already had any `[filenum/total]`
                // prefix stripped by `strip_filenum_prefix` — this run has no
                // way to know the original numbering, so it's simply absent
                // (see `default_subject`'s `None` case) rather than guessed.
                file_index: 0,
                total_files: 0,
            });
        } else if t.starts_with("<meta ") {
            let kind = xml_attr(t, "type").unwrap_or_default();
            let value = xml_text(t, "meta").unwrap_or_default();
            match kind.as_str() {
                "title" => meta.name = Some(value),
                "password" => meta.password = Some(value),
                "category" => meta.category = Some(value),
                "tag" => meta.tags.push(value),
                _ => {}
            }
        }
    }

    segments.sort_by(|a, b| a.file_name.cmp(&b.file_name).then(a.part.cmp(&b.part)));

    Ok(ParsedNzb {
        poster,
        groups,
        segments,
        meta,
    })
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

/// Strip a leading `[filenum/files] - ` counter, if present.
///
/// `nyuu`'s default subject template is
/// `[{filenum}/{files}] - "{filename}" yEnc ({part}/{parts})` — that
/// `[N/M] - ` counter sits *outside* the quoted real name, so without this,
/// `strip_part_suffix`'s `unquote` never fires (the string starts with `[`,
/// not `"`) and the counter ends up baked into the "real" file name
/// `penne::assemble` later joins onto a directory path — a `/` inside
/// `[01/14]` then splits into a bogus nested directory instead of staying
/// part of one path component. Falls back to `s` unchanged for any subject
/// that isn't actually this shape (including `pesto`'s own, which never
/// emits this prefix — see `ROADMAP.md` "Subject file counter").
fn strip_filenum_prefix(s: &str) -> &str {
    (|| {
        let rest = s.strip_prefix('[')?;
        let (counter, after) = rest.split_once(']')?;
        let is_counter = !counter.is_empty()
            && counter
                .split('/')
                .all(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
        is_counter
            .then_some(())
            .and_then(|()| after.strip_prefix(" - "))
    })()
    .unwrap_or(s)
}

/// Reverse the XML entity escaping applied by [`escape`].
fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}
