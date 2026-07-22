//! `.nzb` file generation.
//!
//! Turns the [`PostedSegment`]s collected during a posting run into a valid
//! `.nzb` XML document (newzbin NZB 1.1). Segments are expected pre-sorted by
//! file name then part number, as [`crate::poster::post_files`] returns them.

use std::path::PathBuf;
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
    /// TMDb reference (`<meta type="tmdbid">`), formatted as `movie/<id>` or
    /// `tv/<id>` — see [`parse_tmdb_ref`].
    pub tmdb_id: Option<String>,
    /// IMDb ID (`<meta type="imdbid">`), e.g. `tt1234567` — see [`parse_imdb_ref`].
    pub imdb_id: Option<String>,
    /// TheTVDB ID (`<meta type="tvdbid">`) — see [`parse_tvdb_ref`].
    pub tvdb_id: Option<String>,
    /// MyAnimeList ID (`<meta type="malid">`) — see [`parse_mal_ref`].
    pub mal_id: Option<String>,
    /// Arbitrary tags emitted as multiple `<meta type="tag">` elements.
    pub tags: Vec<String>,
}

/// Media type of a [`parse_tmdb_ref`] result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TmdbKind {
    Movie,
    Tv,
}

impl TmdbKind {
    fn as_str(self) -> &'static str {
        match self {
            TmdbKind::Movie => "movie",
            TmdbKind::Tv => "tv",
        }
    }

    /// `.nzb` category to fall back to when the user hasn't set one explicitly.
    pub fn default_category(self) -> &'static str {
        match self {
            TmdbKind::Movie => "movies",
            TmdbKind::Tv => "tv",
        }
    }
}

/// Parse a `--tmdb` value into its media kind and numeric ID.
///
/// Accepts `movie/<id>` or `tv/<id>`, matching TMDb's own `/movie/<id>` and
/// `/tv/<id>` URL scheme. `:` is also accepted as the separator
/// (`movie:<id>`), since some indexer tools use that convention instead.
pub fn parse_tmdb_ref(s: &str) -> Result<(TmdbKind, String), String> {
    let (kind_str, id) = s
        .split_once(['/', ':'])
        .ok_or_else(|| format!("expected `movie/<id>` or `tv/<id>`, got `{s}`"))?;
    let kind = match kind_str.to_ascii_lowercase().as_str() {
        "movie" => TmdbKind::Movie,
        "tv" => TmdbKind::Tv,
        other => {
            return Err(format!(
                "unknown TMDb media type `{other}` (expected `movie` or `tv`)"
            ))
        }
    };
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("expected a numeric TMDb ID, got `{id}`"));
    }
    Ok((kind, id.to_owned()))
}

/// Normalize a parsed [`parse_tmdb_ref`] result into the value stored in
/// [`NzbMeta::tmdb_id`], e.g. `("movie", "12345")` -> `"movie/12345"`.
pub fn format_tmdb_ref(kind: TmdbKind, id: &str) -> String {
    format!("{}/{id}", kind.as_str())
}

/// Parse a `--imdb-id` value into its normalized form.
///
/// Accepts an optional `tt` prefix (case-insensitive) followed by digits;
/// the `tt` prefix is added when missing. IMDb IDs are zero-padded to at
/// least 7 digits (e.g. `133093` and `tt0133093` both normalize to
/// `tt0133093`); longer IDs are kept as-is.
pub fn parse_imdb_ref(s: &str) -> Result<String, String> {
    let trimmed = s.trim();
    let digits = trimmed
        .strip_prefix("tt")
        .or_else(|| trimmed.strip_prefix("TT"))
        .or_else(|| trimmed.strip_prefix("Tt"))
        .or_else(|| trimmed.strip_prefix("tT"))
        .unwrap_or(trimmed);
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("expected an IMDb ID like `tt1234567`, got `{s}`"));
    }
    Ok(format!("tt{digits:0>7}"))
}

/// Parse a `--tvdb-id` value into its normalized form: a plain numeric string.
///
/// Unlike TMDb, TheTVDB IDs aren't split into separate movie/series
/// namespaces from the caller's point of view — the plain ID resolves
/// correctly through TheTVDB's own dereferrer link (see [`NzbMeta::tvdb_id`]).
pub fn parse_tvdb_ref(s: &str) -> Result<String, String> {
    parse_numeric_ref(s, "TVDB")
}

/// Parse a `--mal-id` value into its normalized form: a plain numeric string.
pub fn parse_mal_ref(s: &str) -> Result<String, String> {
    parse_numeric_ref(s, "MAL")
}

fn parse_numeric_ref(s: &str, label: &str) -> Result<String, String> {
    let trimmed = s.trim();
    if trimmed.is_empty() || !trimmed.bytes().all(|b| b.is_ascii_digit()) {
        return Err(format!("expected a numeric {label} ID, got `{s}`"));
    }
    Ok(trimmed.to_owned())
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
        ("tmdbid", meta.tmdb_id.as_deref()),
        ("imdbid", meta.imdb_id.as_deref()),
        ("tvdbid", meta.tvdb_id.as_deref()),
        ("malid", meta.mal_id.as_deref()),
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
            // `name` is a `pesto`-only convention (see this module's doc
            // comment); standard NZB 1.1 (every real indexer/posting tool)
            // only writes `subject`, with the real name as the quoted
            // string inside it — exactly what `strip_part_suffix` already
            // extracts into `current_subject_name`. Fall back to that
            // instead of erroring, so foreign `.nzb`s parse at all. A fully
            // obfuscated subject (no quotes) yields the raw hash-like text
            // here — not the real name, but a valid starting point for
            // `penne::deobfuscate` to recover the true one from PAR2.
            current_file_name = xml_attr(t, "name").unwrap_or_else(|| current_subject_name.clone());
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
                file_path: PathBuf::from(&current_file_name),
                subject_name: current_subject_name.clone(),
                file_size: 0,
                part,
                total: 0, // fixed up when </file> is seen
                message_id,
                bytes,
                from: current_poster.clone(),
                date: (None, current_date),
                // Not recoverable from an .nzb (it only exists on the
                // `=yend` line of the last segment's body, which the .nzb
                // never carries) — harmless, since a segment parsed back
                // from an .nzb (--merge-season) is never re-encoded.
                full_crc32: 0,
                // A segment parsed back from an .nzb never re-enters the
                // check queue (see `PostedSegment::server_idx`).
                server_idx: 0,
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
            file_path: PathBuf::from(name),
            subject_name: name.to_string(),
            file_size: 1000,
            part,
            total,
            message_id: id.to_string(),
            bytes: 500,
            from: "poster <p@x>".to_string(),
            date: (None, None),
            full_crc32: 0,
            server_idx: 0,
        }
    }

    fn no_meta() -> NzbMeta {
        NzbMeta::default()
    }

    #[test]
    fn parse_tmdb_ref_accepts_slash_and_colon_separators() {
        assert_eq!(
            parse_tmdb_ref("movie/12345"),
            Ok((TmdbKind::Movie, "12345".to_string()))
        );
        assert_eq!(
            parse_tmdb_ref("tv:9999"),
            Ok((TmdbKind::Tv, "9999".to_string()))
        );
        assert_eq!(
            parse_tmdb_ref("MOVIE/1"),
            Ok((TmdbKind::Movie, "1".to_string()))
        );
    }

    #[test]
    fn parse_tmdb_ref_rejects_bad_input() {
        assert!(parse_tmdb_ref("12345").is_err());
        assert!(parse_tmdb_ref("book/12345").is_err());
        assert!(parse_tmdb_ref("movie/abc").is_err());
        assert!(parse_tmdb_ref("movie/").is_err());
    }

    #[test]
    fn tmdb_kind_default_category() {
        assert_eq!(TmdbKind::Movie.default_category(), "movies");
        assert_eq!(TmdbKind::Tv.default_category(), "tv");
    }

    #[test]
    fn format_tmdb_ref_normalizes_to_slash() {
        assert_eq!(format_tmdb_ref(TmdbKind::Movie, "12345"), "movie/12345");
        assert_eq!(format_tmdb_ref(TmdbKind::Tv, "9999"), "tv/9999");
    }

    #[test]
    fn parse_imdb_ref_normalizes_case() {
        assert_eq!(parse_imdb_ref("tt1234567"), Ok("tt1234567".to_string()));
        assert_eq!(parse_imdb_ref("TT1234567"), Ok("tt1234567".to_string()));
    }

    #[test]
    fn parse_imdb_ref_accepts_bare_digits_and_pads() {
        assert_eq!(parse_imdb_ref("1234567"), Ok("tt1234567".to_string()));
        assert_eq!(parse_imdb_ref("133093"), Ok("tt0133093".to_string()));
        assert_eq!(parse_imdb_ref("tt133093"), Ok("tt0133093".to_string()));
        assert_eq!(parse_imdb_ref("21"), Ok("tt0000021".to_string()));
    }

    #[test]
    fn parse_imdb_ref_keeps_long_ids_unpadded() {
        assert_eq!(parse_imdb_ref("tt12345678"), Ok("tt12345678".to_string()));
    }

    #[test]
    fn parse_imdb_ref_rejects_bad_input() {
        assert!(parse_imdb_ref("tt").is_err());
        assert!(parse_imdb_ref("ttabc").is_err());
        assert!(parse_imdb_ref("abc123").is_err());
        assert!(parse_imdb_ref("").is_err());
    }

    #[test]
    fn parse_tvdb_ref_accepts_plain_digits() {
        assert_eq!(parse_tvdb_ref("81189"), Ok("81189".to_string()));
        assert_eq!(parse_tvdb_ref("  81189  "), Ok("81189".to_string()));
    }

    #[test]
    fn parse_tvdb_ref_rejects_bad_input() {
        assert!(parse_tvdb_ref("tt81189").is_err());
        assert!(parse_tvdb_ref("").is_err());
        assert!(parse_tvdb_ref("abc").is_err());
    }

    #[test]
    fn parse_mal_ref_accepts_plain_digits() {
        assert_eq!(parse_mal_ref("1535"), Ok("1535".to_string()));
    }

    #[test]
    fn parse_mal_ref_rejects_bad_input() {
        assert!(parse_mal_ref("").is_err());
        assert!(parse_mal_ref("abc").is_err());
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
            file_path: PathBuf::from("secret-movie.mkv"),
            subject_name: "deadbeefcafe0000".to_string(),
            file_size: 1000,
            part: 1,
            total: 1,
            message_id: "<id@x>".to_string(),
            bytes: 500,
            from: String::new(),
            date: (None, None),
            full_crc32: 0,
            server_idx: 0,
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
            file_path: PathBuf::from("secret-movie.mkv"),
            subject_name: "deadbeefcafe0000".to_string(),
            file_size: 1000,
            part: 1,
            total: 1,
            message_id: "<id@x>".to_string(),
            bytes: 500,
            from: String::new(),
            date: (None, None),
            full_crc32: 0,
            server_idx: 0,
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
            tmdb_id: Some("tv/12345".into()),
            imdb_id: Some("tt1234567".into()),
            tvdb_id: Some("321".into()),
            mal_id: Some("654".into()),
            tags: Vec::new(),
        };
        let xml = generate(&["alt.test".into()], &[], &meta);
        assert!(xml.contains("<meta type=\"name\">My Upload</meta>"));
        assert!(xml.contains("<meta type=\"password\">s3cr3t</meta>"));
        assert!(xml.contains("<meta type=\"category\">TV &gt; HD</meta>"));
        assert!(xml.contains("<meta type=\"tmdbid\">tv/12345</meta>"));
        assert!(xml.contains("<meta type=\"imdbid\">tt1234567</meta>"));
        assert!(xml.contains("<meta type=\"tvdbid\">321</meta>"));
        assert!(xml.contains("<meta type=\"malid\">654</meta>"));
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
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: None,
            mal_id: None,
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
            tmdb_id: None,
            imdb_id: None,
            tvdb_id: None,
            mal_id: None,
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

    /// Real-world NZBs (every indexer/posting tool other than `pesto`
    /// itself) never write a `name` attribute on `<file>` — only `subject`,
    /// per the standard NZB 1.1 DTD. `parse()` must derive the filename
    /// from the quoted string inside `subject` in that case instead of
    /// erroring, or `penne` could never download anything but its own
    /// self-posted content.
    #[test]
    fn parse_derives_file_name_from_subject_when_name_attribute_is_absent() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="poster &lt;p@x&gt;" date="1700000000" subject="&quot;real.mkv&quot; yEnc (1/2)">
    <groups>
      <group>alt.binaries.test</group>
    </groups>
    <segments>
      <segment bytes="500" number="1">seg1@x</segment>
      <segment bytes="500" number="2">seg2@x</segment>
    </segments>
  </file>
</nzb>
"#;
        let parsed = parse(xml).expect("parse must succeed without a name attribute");
        assert_eq!(parsed.segments.len(), 2);
        assert_eq!(parsed.segments[0].file_name, "real.mkv");
    }

    /// A *fully* obfuscated post has no quoted real name in `subject`
    /// either — the raw (suffix-stripped) subject text becomes the
    /// starting `file_name`. Meaningless, but must not be a parse error:
    /// recovering the true name from PAR2 is `penne::deobfuscate`'s job,
    /// which needs the file to be queued and downloaded first.
    #[test]
    fn parse_falls_back_to_raw_subject_when_fully_obfuscated() {
        let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <file poster="poster &lt;p@x&gt;" date="1700000000" subject="a1b2c3d4e5f6 (1/1)">
    <groups>
      <group>alt.binaries.test</group>
    </groups>
    <segments>
      <segment bytes="500" number="1">seg1@x</segment>
    </segments>
  </file>
</nzb>
"#;
        let parsed = parse(xml).expect("parse must succeed on a fully obfuscated subject");
        assert_eq!(parsed.segments.len(), 1);
        assert_eq!(parsed.segments[0].file_name, "a1b2c3d4e5f6");
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

    #[test]
    fn strip_part_suffix_strips_nyuu_style_filenum_prefix() {
        // nyuu's default subject template: `[{filenum}/{files}] - "{filename}"
        // yEnc ({part}/{parts})`. Without stripping the prefix, this used to
        // leave `[01/14] - ` glued onto the "real" file name — a `/` inside
        // it then split into a bogus nested directory once `penne::assemble`
        // joined it onto a destination path.
        assert_eq!(
            strip_part_suffix("[01/14] - \"tlvUQcjvcf3NdsD6sIYfofH3.par2\" yEnc (1/1)"),
            "tlvUQcjvcf3NdsD6sIYfofH3.par2"
        );
        assert_eq!(
            strip_part_suffix("[9/14] - \"movie.mkv\" yEnc (3/2133)"),
            "movie.mkv"
        );
    }

    #[test]
    fn strip_part_suffix_leaves_a_bracket_that_is_not_a_filenum_counter_alone() {
        // Only strip when `[...]` truly looks like a `N/M` counter — a real
        // file name that happens to start with brackets must survive as-is.
        assert_eq!(
            strip_part_suffix("\"[LEAK] movie.mkv\" yEnc (1/1)"),
            "[LEAK] movie.mkv"
        );
    }
}
