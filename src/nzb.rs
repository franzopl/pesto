//! `.nzb` file generation.
//!
//! Turns the [`PostedSegment`]s collected during a posting run into a valid
//! `.nzb` XML document (newzbin NZB 1.1). Segments are expected pre-sorted by
//! file name then part number, as [`crate::poster::post_files`] returns them.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::article::default_subject;
use crate::poster::PostedSegment;

/// Generate the contents of an `.nzb` file describing the posted segments.
///
/// When `password` is `Some`, a `<meta type="password">` element is emitted
/// so NZBGet / SABnzbd can extract a password-protected archive automatically.
pub fn generate(
    poster: &str,
    groups: &[String],
    segments: &[PostedSegment],
    password: Option<&str>,
) -> String {
    let date = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE nzb PUBLIC \"-//newzBin//DTD NZB 1.1//EN\" \
         \"http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd\">\n",
    );
    out.push_str("<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n");

    if let Some(pw) = password {
        out.push_str(&format!(
            "  <head><meta type=\"password\">{}</meta></head>\n",
            escape(pw)
        ));
    }

    // Segments arrive sorted by (file_name, part); group consecutive runs.
    let mut i = 0;
    while i < segments.len() {
        let name = &segments[i].file_name;
        let count = segments[i..]
            .iter()
            .take_while(|s| &s.file_name == name)
            .count();
        write_file(&mut out, poster, groups, date, &segments[i..i + count]);
        i += count;
    }

    out.push_str("</nzb>\n");
    out
}

/// Write a single `<file>` element for one file's segments.
fn write_file(
    out: &mut String,
    poster: &str,
    groups: &[String],
    date: u64,
    segs: &[PostedSegment],
) {
    let first = &segs[0];
    // The subject reflects the posting name (random under obfuscation); the
    // real file name is preserved in the `name` attribute so a downloader can
    // restore it even when the wire artifacts are obfuscated.
    let subject = default_subject(&first.subject_name, 1, first.total);

    out.push_str(&format!(
        "  <file name=\"{}\" poster=\"{}\" date=\"{}\" subject=\"{}\">\n",
        escape(&first.file_name),
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
        }
    }

    #[test]
    fn empty_input_yields_a_well_formed_skeleton() {
        let xml = generate("p <p@x>", &["alt.test".into()], &[], None);
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
        let xml = generate("poster <p@x>", &["alt.test".into()], &segments, None);

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
        };
        let xml = generate("poster <p@x>", &["alt.test".into()], &[segment], None);
        // The subject is the obfuscated name; the real name lives in `name`.
        assert!(xml.contains("subject=\"deadbeefcafe0000\""));
        assert!(xml.contains("name=\"secret-movie.mkv\""));
        assert!(!xml.contains("subject=\"secret-movie.mkv\""));
    }

    #[test]
    fn xml_special_characters_are_escaped() {
        let segments = vec![seg("a&b<c>.bin", 1, 1, "<i@x>")];
        let xml = generate("a \"b\" & <c>", &["alt.test".into()], &segments, None);
        assert!(xml.contains("poster=\"a &quot;b&quot; &amp; &lt;c&gt;\""));
        assert!(xml.contains("a&amp;b&lt;c&gt;.bin"));
    }
}
