//! `.nzb` file generation.
//!
//! Turns the [`PostedSegment`]s collected during a posting run into a valid
//! `.nzb` XML document (newzbin NZB 1.1). Segments are expected pre-sorted by
//! file name then part number, as [`crate::poster::post_files`] returns them.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::article::default_subject;
use crate::poster::PostedSegment;

/// NZB `<head>` metadata fields emitted as `<meta type="...">` elements.
///
/// All fields are optional. NZBGet and SABnzbd recognise `name`, `password`
/// and `category` natively; other values are ignored by those clients but
/// kept in the XML for informational use.
#[derive(Debug, Default, Clone)]
pub struct NzbMeta {
    /// Friendly display name for the download (`<meta type="name">`).
    pub name: Option<String>,
    /// Extraction password (`<meta type="password">`).
    /// Set this from the archive password when `--nzb-password` is absent.
    pub password: Option<String>,
    /// Indexer / downloader category (`<meta type="category">`).
    pub category: Option<String>,
    /// Arbitrary tags emitted as multiple `<meta type="tag">` elements.
    pub tags: Vec<String>,
}

/// Generate the contents of an `.nzb` file describing the posted segments.
///
/// [`NzbMeta`] fields are emitted as `<meta>` elements in the `<head>` block.
///
/// The NZB always carries the real filename regardless of obfuscation mode.
/// Only what goes on the wire (subject + yEnc `name=`) is obfuscated.
///
/// NZB 1.1 has one `poster` and one `date` per `<file>` element. When
/// obfuscation rotates these per article (paranoid mode) the first segment's
/// values are used as the file-level representative.
pub fn generate(groups: &[String], segments: &[PostedSegment], meta: &NzbMeta) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE nzb PUBLIC \"-//newzBin//DTD NZB 1.1//EN\" \
         \"http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd\">\n",
    );
    out.push_str("<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n");

    // Collect only the meta fields that are set.
    let metas: Vec<(&str, &str)> = [
        ("name", meta.name.as_deref()),
        ("password", meta.password.as_deref()),
        ("category", meta.category.as_deref()),
    ]
    .into_iter()
    .filter_map(|(k, v)| v.map(|s| (k, s)))
    .collect();

    out.push_str("  <head>\n");
    for (k, v) in &metas {
        out.push_str(&format!("    <meta type=\"{}\">{}</meta>\n", k, escape(v)));
    }
    for tag in &meta.tags {
        out.push_str(&format!("    <meta type=\"tag\">{}</meta>\n", escape(tag)));
    }
    out.push_str("  </head>\n");

    // Segments arrive sorted by (file_name, part); group consecutive runs.
    let mut i = 0;
    while i < segments.len() {
        let name = &segments[i].file_name;
        let count = segments[i..]
            .iter()
            .take_while(|s| &s.file_name == name)
            .count();
        write_file(&mut out, groups, &segments[i..i + count]);
        i += count;
    }

    out.push_str("</nzb>\n");
    out
}

/// Write a single `<file>` element for one file's segments.
fn write_file(out: &mut String, groups: &[String], segs: &[PostedSegment]) {
    let first = &segs[0];
    // The NZB always carries the real filename so that download clients can
    // restore it correctly. Only what goes on the wire (subject + yEnc name=)
    // is obfuscated — see ObfuscateMode::Full in poster/mod.rs.
    let file_name = &first.file_name;
    let subject = default_subject(&first.subject_name, 1, first.total);
    let poster = &first.from;
    let (_rfc_date, unix_date) = &first.date;
    let date = unix_date.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });

    out.push_str(&format!(
        "  <file name=\"{}\" poster=\"{}\" date=\"{}\" subject=\"{}\">\n",
        escape(file_name),
        escape(poster),
        date,
        escape(&subject),
    ));

    out.push_str("    <groups>\n");
    for group in groups {
        out.push_str(&format!("      <group>{}</group>\n", escape(group)));
    }
    out.push_str("    </groups>\n");

    out.push_str("    <segments>\n");
    for seg in segs {
        // NZB segment bodies carry the Message-ID without angle brackets.
        let id = seg.message_id.trim_start_matches('<').trim_end_matches('>');
        out.push_str(&format!(
            "      <segment bytes=\"{}\" number=\"{}\">{}</segment>\n",
            seg.bytes,
            seg.part,
            escape(id),
        ));
    }
    out.push_str("    </segments>\n");
    out.push_str("  </file>\n");
}

// ── NZB parser ──────────────────────────────────────────────────────────────

/// The contents of a parsed `.nzb` file.
pub struct ParsedNzb {
    /// `From` header found in the first `<file>` element.
    pub poster: String,
    /// Newsgroups listed in `<groups>` (deduplicated, first file wins).
    pub groups: Vec<String>,
    /// All segments, sorted by `(file_name, part)`.
    pub segments: Vec<PostedSegment>,
    /// `<head>` metadata (`name`, `password`, `category`, `tags`).
    pub meta: NzbMeta,
}

/// Parse a `.nzb` document and reconstruct its [`PostedSegment`] list.
///
/// The parser targets the format produced by [`generate`] but tolerates minor
/// whitespace variation.  Attributes must use double quotes. Segments are
/// sorted by `(file_name, part)` before returning so they can be passed
/// directly to [`generate`].
pub fn parse(content: &str) -> anyhow::Result<ParsedNzb> {
    use anyhow::Context as _;

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
            current_file_name = xml_attr(t, "name").context("<file> missing name attribute")?;
            current_poster = xml_attr(t, "poster").unwrap_or_default();
            if poster.is_empty() {
                poster = current_poster.clone();
            }
            current_date = xml_attr(t, "date").and_then(|s| s.parse().ok());
            let subject = xml_attr(t, "subject").unwrap_or_default();
            current_subject_name = strip_part_suffix(&subject);
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
                subject_name: current_subject_name.clone(),
                file_size: 0,
                part,
                total: 0, // fixed up when </file> is seen
                message_id,
                bytes,
                from: current_poster.clone(),
                date: (None, current_date),
            });
        } else if t.starts_with("<meta ") {
            let kind = xml_attr(t, "type").unwrap_or_default();
            let value = xml_text(t, "meta").unwrap_or_default();
            match kind.as_str() {
                "name" => meta.name = Some(value),
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
fn strip_part_suffix(subject: &str) -> String {
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
            return unquote(&subject[..pos]);
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

/// Reverse the XML entity escaping applied by [`escape`].
fn xml_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

// ── XML escaping ─────────────────────────────────────────────────────────────

/// Escape the five XML predefined entities.
fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(name: &str, part: u32, total: u32, id: &str) -> PostedSegment {
        PostedSegment {
            file_name: name.to_string(),
            subject_name: name.to_string(),
            file_size: 1000,
            part,
            total,
            message_id: id.to_string(),
            bytes: 500,
            from: "poster <p@x>".to_string(),
            date: (None, None),
        }
    }

    fn no_meta() -> NzbMeta {
        NzbMeta::default()
    }

    #[test]
    fn empty_input_yields_a_well_formed_skeleton() {
        let xml = generate(&["alt.test".into()], &[], &no_meta());
        assert!(xml.starts_with("<?xml version=\"1.0\""));
        assert!(xml.contains("<nzb xmlns="));
        assert!(xml.trim_end().ends_with("</nzb>"));
        assert!(!xml.contains("<file"));
    }

    #[test]
    fn groups_segments_per_file_and_strips_brackets() {
        let segments = vec![
            seg("a.bin", 1, 2, "<id-a1@pesto>"),
            seg("a.bin", 2, 2, "<id-a2@pesto>"),
            seg("b.bin", 1, 1, "<id-b1@pesto>"),
        ];
        let xml = generate(&["alt.test".into()], &segments, &no_meta());

        assert_eq!(xml.matches("<file ").count(), 2);
        assert_eq!(xml.matches("<segment ").count(), 3);
        // Message-IDs appear without angle brackets.
        assert!(xml.contains(">id-a1@pesto</segment>"));
        assert!(!xml.contains("<id-a1@pesto>"));
        assert!(xml.contains("<group>alt.test</group>"));
        assert!(xml.contains("bytes=\"500\" number=\"2\""));
    }

    #[test]
    fn obfuscated_subject_keeps_real_name_in_attribute() {
        let segment = PostedSegment {
            file_name: "secret-movie.mkv".to_string(),
            subject_name: "deadbeefcafe0000".to_string(),
            file_size: 1000,
            part: 1,
            total: 1,
            message_id: "<id@x>".to_string(),
            bytes: 500,
            from: String::new(),
            date: (None, None),
        };
        let xml = generate(&["alt.test".into()], &[segment], &no_meta());
        // The wire subject is obfuscated; the NZB always carries the real filename.
        assert!(xml.contains("subject=\"&quot;deadbeefcafe0000&quot; yEnc\""));
        assert!(xml.contains("name=\"secret-movie.mkv\""));
        assert!(!xml.contains("subject=\"secret-movie.mkv\""));
    }

    #[test]
    fn full_obfuscation_preserves_real_name_in_nzb() {
        let segment = PostedSegment {
            file_name: "secret-movie.mkv".to_string(),
            subject_name: "deadbeefcafe0000".to_string(),
            file_size: 1000,
            part: 1,
            total: 1,
            message_id: "<id@x>".to_string(),
            bytes: 500,
            from: String::new(),
            date: (None, None),
        };
        let xml = generate(&["alt.test".into()], &[segment], &no_meta());
        // Subject on the wire is obfuscated; NZB name= always uses the real filename.
        assert!(xml.contains("subject=\"&quot;deadbeefcafe0000&quot; yEnc\""));
        assert!(xml.contains("name=\"secret-movie.mkv\""));
        assert!(!xml.contains("name=\"deadbeefcafe0000\""));
    }

    #[test]
    fn xml_special_characters_are_escaped() {
        let mut s = seg("a&b<c>.bin", 1, 1, "<i@x>");
        s.from = "a \"b\" & <c>".to_string();
        let segments = vec![s];
        let xml = generate(&["alt.test".into()], &segments, &no_meta());
        assert!(xml.contains("poster=\"a &quot;b&quot; &amp; &lt;c&gt;\""));
        assert!(xml.contains("a&amp;b&lt;c&gt;.bin"));
    }

    #[test]
    fn meta_fields_emitted_in_head() {
        let meta = NzbMeta {
            name: Some("My Upload".into()),
            password: Some("s3cr3t".into()),
            category: Some("TV > HD".into()),
            tags: Vec::new(),
        };
        let xml = generate(&["alt.test".into()], &[], &meta);
        assert!(xml.contains("<meta type=\"name\">My Upload</meta>"));
        assert!(xml.contains("<meta type=\"password\">s3cr3t</meta>"));
        assert!(xml.contains("<meta type=\"category\">TV &gt; HD</meta>"));
    }

    #[test]
    fn head_block_always_present() {
        // <head> is emitted even when no meta fields are set, for maximum
        // compatibility with strict NZB parsers.
        let xml = generate(&["alt.test".into()], &[], &no_meta());
        assert!(xml.contains("<head>"));
        assert!(xml.contains("</head>"));
        assert!(!xml.contains("<meta"));
    }

    #[test]
    fn multi_file_multi_segment_with_par2() {
        let groups = vec!["alt.test".into()];
        let segments = vec![
            seg("movie.mkv", 1, 3, "<a1@x>"),
            seg("movie.mkv", 2, 3, "<a2@x>"),
            seg("movie.mkv", 3, 3, "<a3@x>"),
            seg("movie.par2", 1, 1, "<p1@x>"),
            seg("movie.vol00+01.par2", 1, 1, "<p2@x>"),
        ];
        let xml = generate(&groups, &segments, &no_meta());

        // Three distinct <file> blocks.
        assert_eq!(xml.matches("<file ").count(), 3);
        // Five <segment> entries total.
        assert_eq!(xml.matches("<segment ").count(), 5);
        // PAR2 files appear.
        assert!(xml.contains("name=\"movie.par2\""));
        assert!(xml.contains("name=\"movie.vol00+01.par2\""));
        // Multi-part subject rendered correctly for movie.mkv.
        assert!(xml.contains("subject=\"&quot;movie.mkv&quot; yEnc (1/3)\""));
    }

    #[test]
    fn multiple_groups_all_emitted() {
        let groups = vec!["alt.binaries.a".into(), "alt.binaries.b".into()];
        let xml = generate(&groups, &[seg("f.bin", 1, 1, "<id@x>")], &no_meta());
        assert!(xml.contains("<group>alt.binaries.a</group>"));
        assert!(xml.contains("<group>alt.binaries.b</group>"));
        assert_eq!(xml.matches("<group>").count(), 2);
    }

    #[test]
    fn single_part_subject_has_no_part_indicator() {
        let xml = generate(
            &["alt.test".into()],
            &[seg("file.bin", 1, 1, "<id@x>")],
            &no_meta(),
        );
        assert!(xml.contains("subject=\"&quot;file.bin&quot; yEnc\""));
        assert!(!xml.contains("(1/1)"));
    }

    #[test]
    fn escape_apostrophe() {
        let segments = vec![seg("it's.bin", 1, 1, "<id@x>")];
        let xml = generate(&["alt.test".into()], &segments, &no_meta());
        assert!(xml.contains("it&apos;s.bin"), "apostrophe must be escaped");
        assert!(!xml.contains("it's.bin"));
    }

    #[test]
    fn file_name_with_slash_is_not_escaped() {
        // A relative path like "Season01/ep01.mkv" — forward slash is not an
        // XML entity and must appear verbatim in the output.
        let mut s = seg("Season01/ep01.mkv", 1, 1, "<id@x>");
        s.file_name = "Season01/ep01.mkv".into();
        s.subject_name = "Season01/ep01.mkv".into();
        let xml = generate(&["alt.test".into()], &[s], &no_meta());
        assert!(xml.contains("name=\"Season01/ep01.mkv\""));
    }

    #[test]
    fn subject_always_shows_part_one_of_total() {
        // write_file always emits "(1/N)" regardless of which parts are present.
        // This is intentional — the subject describes the file, not a segment.
        let segments = vec![
            seg("big.bin", 2, 5, "<a2@x>"),
            seg("big.bin", 3, 5, "<a3@x>"),
        ];
        let xml = generate(&["alt.test".into()], &segments, &no_meta());
        assert!(xml.contains("subject=\"&quot;big.bin&quot; yEnc (1/5)\""));
        assert!(!xml.contains("(2/5)"));
    }

    #[test]
    fn segment_bytes_attribute_is_exact() {
        let mut s = seg("f.bin", 1, 1, "<id@x>");
        s.bytes = 123_456;
        let xml = generate(&["alt.test".into()], &[s], &no_meta());
        assert!(xml.contains("bytes=\"123456\""));
    }

    #[test]
    fn date_attribute_is_a_nonzero_number() {
        let xml = generate(
            &["alt.test".into()],
            &[seg("f.bin", 1, 1, "<id@x>")],
            &no_meta(),
        );
        // Extract the date="..." value from the <file> element.
        let date_str = xml
            .lines()
            .find(|l| l.contains("<file "))
            .and_then(|l| l.split("date=\"").nth(1))
            .and_then(|l| l.split('"').next())
            .unwrap();
        let date: u64 = date_str.parse().expect("date should be a number");
        assert!(date > 0, "date timestamp should be non-zero");
    }

    #[test]
    fn tags_are_emitted_in_order() {
        let meta = NzbMeta {
            tags: vec!["hd".into(), "2024".into(), "dts".into()],
            ..Default::default()
        };
        let xml = generate(&["alt.test".into()], &[], &meta);
        assert!(xml.contains("<meta type=\"tag\">hd</meta>"));
        assert!(xml.contains("<meta type=\"tag\">2024</meta>"));
        assert!(xml.contains("<meta type=\"tag\">dts</meta>"));
        let hd = xml.find("hd").unwrap();
        let y2024 = xml.find("2024").unwrap();
        let dts = xml.find("dts").unwrap();
        assert!(hd < y2024 && y2024 < dts);
    }

    #[test]
    fn tag_xml_special_characters_are_escaped() {
        let meta = NzbMeta {
            tags: vec!["a&b<c>".into()],
            ..Default::default()
        };
        let xml = generate(&["alt.test".into()], &[], &meta);
        assert!(xml.contains("<meta type=\"tag\">a&amp;b&lt;c&gt;</meta>"));
    }

    #[test]
    fn empty_tags_emit_no_tag_meta() {
        let xml = generate(&["alt.test".into()], &[], &no_meta());
        assert!(!xml.contains("type=\"tag\""));
    }

    #[test]
    fn only_password_meta_emits_head_without_name_or_category() {
        let meta = NzbMeta {
            name: None,
            password: Some("hunter2".into()),
            category: None,
            tags: Vec::new(),
        };
        let xml = generate(&["alt.test".into()], &[], &meta);
        assert!(xml.contains("<meta type=\"password\">hunter2</meta>"));
        assert!(!xml.contains("type=\"name\""));
        assert!(!xml.contains("type=\"category\""));
    }

    // ── parse() round-trip tests ─────────────────────────────────────────────

    #[test]
    fn parse_round_trips_generate() {
        let groups = vec!["alt.binaries.test".into()];
        let segs = vec![
            seg("ep01.mkv", 1, 3, "<a1@x>"),
            seg("ep01.mkv", 2, 3, "<a2@x>"),
            seg("ep01.mkv", 3, 3, "<a3@x>"),
        ];
        let meta = NzbMeta {
            name: Some("Test Show S01".into()),
            password: None,
            category: Some("TV".into()),
            tags: vec!["hd".into(), "2024".into()],
        };
        let xml = generate(&groups, &segs, &meta);
        let parsed = parse(&xml).expect("parse must succeed");

        assert_eq!(parsed.poster, "poster <p@x>");
        assert_eq!(parsed.groups, vec!["alt.binaries.test"]);
        assert_eq!(parsed.meta.name.as_deref(), Some("Test Show S01"));
        assert_eq!(parsed.meta.category.as_deref(), Some("TV"));
        assert_eq!(parsed.meta.tags, vec!["hd", "2024"]);
        assert_eq!(parsed.segments.len(), 3);
        assert_eq!(parsed.segments[0].file_name, "ep01.mkv");
        assert_eq!(parsed.segments[0].part, 1);
        assert_eq!(parsed.segments[0].total, 3);
        assert!(parsed.segments[0].message_id.starts_with('<'));
    }

    #[test]
    fn parse_multi_file_nzb_preserves_all_segments() {
        let groups = vec!["alt.binaries.test".into()];
        let segs = vec![
            seg("ep01.mkv", 1, 2, "<e1p1@x>"),
            seg("ep01.mkv", 2, 2, "<e1p2@x>"),
            seg("ep02.mkv", 1, 1, "<e2p1@x>"),
        ];
        let xml = generate(&groups, &segs, &no_meta());
        let parsed = parse(&xml).expect("parse must succeed");

        assert_eq!(parsed.segments.len(), 3);
        // After sort: ep01 parts then ep02.
        assert_eq!(parsed.segments[0].file_name, "ep01.mkv");
        assert_eq!(parsed.segments[2].file_name, "ep02.mkv");
        assert_eq!(parsed.segments[0].total, 2);
        assert_eq!(parsed.segments[2].total, 1);
    }

    #[test]
    fn parse_strips_angle_brackets_and_re_adds_them() {
        let segs = vec![seg("f.bin", 1, 1, "<msgid@host>")];
        let xml = generate(&["alt.test".into()], &segs, &no_meta());
        let parsed = parse(&xml).expect("parse must succeed");
        // message_id must carry angle brackets.
        assert_eq!(parsed.segments[0].message_id, "<msgid@host>");
    }

    #[test]
    fn strip_part_suffix_new_format_multi_part() {
        assert_eq!(strip_part_suffix("\"name\" yEnc (1/3)"), "name");
    }

    #[test]
    fn strip_part_suffix_new_format_single_part() {
        assert_eq!(strip_part_suffix("\"name\" yEnc"), "name");
    }

    #[test]
    fn strip_part_suffix_legacy_format() {
        assert_eq!(strip_part_suffix("name (1/3)"), "name");
    }
}
