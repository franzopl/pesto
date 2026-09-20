//! NZB generation: rendering posted segments to the NZB 1.1 XML format.

use std::time::{SystemTime, UNIX_EPOCH};

use crate::article::default_subject;
use crate::config::ObfuscateMode;
use crate::poster::PostedSegment;

use super::{escape, NzbMeta};

/// Generate the contents of an `.nzb` file describing the posted segments.
///
/// [`NzbMeta`] fields are emitted as `<meta>` elements in the `<head>` block.
///
/// `<file>` only carries the standard `poster`, `date` and `subject`
/// attributes — no non-standard `name=` — matching the real NZB 1.1 DTD,
/// which every other posting/downloading tool follows. The `.nzb` carries
/// the canonical client path in `subject`'s quoted string for every
/// `--obfuscate` mode except `light`: obfuscation normally only scrambles the `Subject:`
/// header of the actual NNTP article posted to the server (a separate,
/// transient value — see `ObfuscateMode` in `poster/mod.rs`), so that
/// header-scraping on the newsgroup can't identify the release, while
/// anyone holding the `.nzb` itself — already through a private channel —
/// gets the real name straight away, same as any other obfuscated
/// scene/P2P release.
///
/// `light` exists so a recipient can search an indexer using the same opaque
/// Subject carried by the `.nzb`; its generated NZB therefore mirrors the
/// wire Subject. For a password-protected compressed upload, that opaque name
/// is also the archive and PAR2 FileDesc name, while the encrypted archive
/// retains the real payload names.
///
/// NZB 1.1 has one `poster` and one `date` per `<file>` element. When
/// obfuscation rotates these per article (`article` mode) the first segment's
/// values are used as the file-level representative.
pub fn generate(
    groups: &[String],
    segments: &[PostedSegment],
    meta: &NzbMeta,
    _obfuscate: ObfuscateMode,
) -> String {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(
        "<!DOCTYPE nzb PUBLIC \"-//newzBin//DTD NZB 1.1//EN\" \
         \"http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd\">\n",
    );
    out.push_str("<nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n");

    // Collect only the meta fields that are set.
    // "title" is SABnzbd's documented meta type for a human-readable name;
    // plain "name" isn't part of the NZB spec (see NzbMeta::name's doc).
    let metas: Vec<(&str, &str)> = [
        ("title", meta.name.as_deref()),
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
    if let Some(tmdb_id) = &meta.tmdb_id {
        out.push_str(&format!(
            "    <meta type=\"tag\">tmdb:{}</meta>\n",
            escape(&tmdb_id.replace('/', ":"))
        ));
    }
    if let Some(imdb_id) = &meta.imdb_id {
        out.push_str(&format!(
            "    <meta type=\"tag\">imdb:{}</meta>\n",
            escape(imdb_id)
        ));
    }
    if let Some(tvdb_id) = &meta.tvdb_id {
        let tag = match tvdb_id.split_once('/') {
            Some((kind @ ("movie" | "series"), id)) => format!("tvdb:{kind}:{id}"),
            Some(("tv", id)) => format!("tvdb:series:{id}"),
            _ => format!("tvdb:series:{tvdb_id}"),
        };
        out.push_str(&format!("    <meta type=\"tag\">{}</meta>\n", escape(&tag)));
    }
    if let Some(mal_id) = &meta.mal_id {
        out.push_str(&format!(
            "    <meta type=\"tag\">mal:{}</meta>\n",
            escape(mal_id)
        ));
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
        write_file(&mut out, groups, &segments[i..i + count], _obfuscate);
        i += count;
    }

    out.push_str("</nzb>\n");
    out
}

/// Reconstruct the exact `Subject:` header that was actually sent to the
/// NNTP server for the first posted file, using its wire identity
/// (`PostedSegment::wire_name`) rather than the real filename that
/// [`generate`] always writes into the `.nzb` (see its doc comment).
///
/// Under `--obfuscate=full`/`article` every file draws its own independent
/// wire identity, so this is only representative of one file, not the whole
/// release — same scope as the `entry_label` already surfaced to hooks and
/// history. `None` when there are no segments, or the first one's
/// `wire_name` is empty (segments reconstructed from a parsed `.nzb` via
/// [`parse`], which never re-encode and so never had a wire identity).
pub fn wire_subject(segments: &[PostedSegment]) -> Option<String> {
    let first = segments.first()?;
    if first.wire_name.is_empty() {
        return None;
    }
    let file_counter = (first.total_files > 0).then_some((first.file_index, first.total_files));
    Some(default_subject(
        &first.wire_name,
        1,
        first.total,
        file_counter,
    ))
}

/// Like [`wire_subject`], but for every file in the release rather than just
/// the first — returns `(file_name, wire_subject)` pairs. Under
/// `--obfuscate=full`/`article` each file draws an independent wire
/// identity, so a multi-file release (e.g. a video plus its PAR2 volumes)
/// needs one entry per file, not one for the whole run — that's what feeds
/// `history::UploadRecord::wire_subjects`. Files whose first segment has an
/// empty `wire_name` (segments reconstructed from a parsed `.nzb`, which
/// never re-encode) are skipped.
pub fn wire_subjects(segments: &[PostedSegment]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < segments.len() {
        let name = &segments[i].file_name;
        let count = segments[i..]
            .iter()
            .take_while(|s| &s.file_name == name)
            .count();
        if let Some(subject) = wire_subject(&segments[i..i + count]) {
            out.push((name.clone(), subject));
        }
        i += count;
    }
    out
}

/// Write a single `<file>` element for one file's segments.
pub(super) fn write_file(
    out: &mut String,
    groups: &[String],
    segs: &[PostedSegment],
    _obfuscate: ObfuscateMode,
) {
    let first = &segs[0];
    let file_counter = (first.total_files > 0).then_some((first.file_index, first.total_files));
    // `light` deliberately makes the NZB's subject search token identical to
    // the actual wire Subject. Parsed NZBs have no wire identity, so retain
    // their recorded subject name instead of inventing one.
    let name = if _obfuscate == ObfuscateMode::Light && !first.wire_name.is_empty() {
        &first.wire_name
    } else {
        &first.subject_name
    };
    let subject = default_subject(name, 1, first.total, file_counter);
    let poster = &first.from;
    let (_rfc_date, unix_date) = &first.date;
    let date = unix_date.unwrap_or_else(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });

    // Standard NZB 1.1 `<file>` attributes only — poster, date, subject.
    // No non-standard `name=`; `subject`'s quoted string already carries
    // whichever name was chosen above.
    out.push_str(&format!(
        "  <file poster=\"{}\" date=\"{}\" subject=\"{}\">\n",
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
