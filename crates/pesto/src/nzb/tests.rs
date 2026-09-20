use std::path::Path;
use std::sync::Arc;

use crate::config::ObfuscateMode;
use crate::poster::PostedSegment;

use super::*;

fn seg(name: &str, part: u32, total: u32, id: &str) -> PostedSegment {
    PostedSegment {
        file_name: name.to_string(),
        file_path: Arc::from(Path::new(name)),
        subject_name: Arc::from(name),
        wire_name: Arc::from(name),
        wire_yenc_name: Arc::from(name),
        file_size: 1000,
        part,
        total,
        message_id: id.to_string(),
        bytes: 500,
        from: Arc::from("poster <p@x>"),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 0,
        total_files: 0,
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
fn tvdb_kind_default_category_and_dereferrer_segment() {
    assert_eq!(TvdbKind::Movie.default_category(), "movies");
    assert_eq!(TvdbKind::Series.default_category(), "tv");
    assert_eq!(TvdbKind::Movie.as_str(), "movie");
    assert_eq!(TvdbKind::Series.as_str(), "series");
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
fn parse_tvdb_ref_accepts_plain_digits_as_series() {
    assert_eq!(
        parse_tvdb_ref("81189"),
        Ok((TvdbKind::Series, "81189".to_string()))
    );
    assert_eq!(
        parse_tvdb_ref("  81189  "),
        Ok((TvdbKind::Series, "81189".to_string()))
    );
}

#[test]
fn parse_tvdb_ref_accepts_typed_movie_and_series() {
    assert_eq!(
        parse_tvdb_ref("movie/123"),
        Ok((TvdbKind::Movie, "123".to_string()))
    );
    assert_eq!(
        parse_tvdb_ref("series/81189"),
        Ok((TvdbKind::Series, "81189".to_string()))
    );
    assert_eq!(
        parse_tvdb_ref("MOVIE:123"),
        Ok((TvdbKind::Movie, "123".to_string()))
    );
}

#[test]
fn parse_tvdb_ref_accepts_tv_as_series_alias() {
    assert_eq!(
        parse_tvdb_ref("tv/81189"),
        Ok((TvdbKind::Series, "81189".to_string()))
    );
}

#[test]
fn parse_tvdb_ref_rejects_bad_input() {
    assert!(parse_tvdb_ref("tt81189").is_err());
    assert!(parse_tvdb_ref("").is_err());
    assert!(parse_tvdb_ref("abc").is_err());
    assert!(parse_tvdb_ref("episode/123").is_err());
    assert!(parse_tvdb_ref("movie/abc").is_err());
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
    let xml = generate(&["alt.test".into()], &[], &no_meta(), ObfuscateMode::None);
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
    let xml = generate(
        &["alt.test".into()],
        &segments,
        &no_meta(),
        ObfuscateMode::None,
    );

    assert_eq!(xml.matches("<file ").count(), 2);
    assert_eq!(xml.matches("<segment ").count(), 3);
    // Message-IDs appear without angle brackets.
    assert!(xml.contains(">id-a1@pesto</segment>"));
    assert!(!xml.contains("<id-a1@pesto>"));
    assert!(xml.contains("<group>alt.test</group>"));
    assert!(xml.contains("bytes=\"500\" number=\"2\""));
}

#[test]
fn file_element_never_carries_a_name_attribute() {
    // Standard NZB 1.1's <file> only defines poster/date/subject; pesto
    // used to add a non-standard `name=` carrying the real filename —
    // see write_file's doc comment for why that was removed.
    let segment = PostedSegment {
        file_name: "movie.mkv".to_string(),
        file_path: Arc::from(Path::new("movie.mkv")),
        subject_name: Arc::from("movie.mkv"),
        wire_name: Arc::from("movie.mkv"),
        wire_yenc_name: Arc::from("movie.mkv"),
        file_size: 1000,
        part: 1,
        total: 1,
        message_id: "<id@x>".to_string(),
        bytes: 500,
        from: Arc::from(""),
        date: (None, None),
        full_crc32: 0,
        server_idx: 0,
        file_index: 0,
        total_files: 0,
    };
    let xml = generate(
        &["alt.test".into()],
        &[segment],
        &no_meta(),
        ObfuscateMode::None,
    );
    assert!(!xml.contains("name=\""));
    assert!(xml.contains("<file poster="));
}

#[test]
fn xml_special_characters_are_escaped() {
    let mut s = seg("a&b<c>.bin", 1, 1, "<i@x>");
    s.from = Arc::from("a \"b\" & <c>");
    let segments = vec![s];
    let xml = generate(
        &["alt.test".into()],
        &segments,
        &no_meta(),
        ObfuscateMode::None,
    );
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
        tvdb_id: Some("series/321".into()),
        mal_id: Some("654".into()),
        tags: Vec::new(),
    };
    let xml = generate(&["alt.test".into()], &[], &meta, ObfuscateMode::None);
    assert!(xml.contains("<meta type=\"title\">My Upload</meta>"));
    assert!(xml.contains("<meta type=\"password\">s3cr3t</meta>"));
    assert!(xml.contains("<meta type=\"category\">TV &gt; HD</meta>"));
    assert!(xml.contains("<meta type=\"tag\">tmdb:tv:12345</meta>"));
    assert!(xml.contains("<meta type=\"tag\">imdb:tt1234567</meta>"));
    assert!(xml.contains("<meta type=\"tag\">tvdb:series:321</meta>"));
    assert!(xml.contains("<meta type=\"tag\">mal:654</meta>"));
}

#[test]
fn head_block_always_present() {
    // <head> is emitted even when no meta fields are set, for maximum
    // compatibility with strict NZB parsers.
    let xml = generate(&["alt.test".into()], &[], &no_meta(), ObfuscateMode::None);
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
    let xml = generate(&groups, &segments, &no_meta(), ObfuscateMode::None);

    // Three distinct <file> blocks.
    assert_eq!(xml.matches("<file ").count(), 3);
    // Five <segment> entries total.
    assert_eq!(xml.matches("<segment ").count(), 5);
    // PAR2 files appear.
    assert!(xml.contains("subject=\"&quot;movie.par2&quot; yEnc (1/1)\""));
    assert!(xml.contains("subject=\"&quot;movie.vol00+01.par2&quot; yEnc (1/1)\""));
    // Multi-part subject rendered correctly for movie.mkv.
    assert!(xml.contains("subject=\"&quot;movie.mkv&quot; yEnc (1/3)\""));
}

#[test]
fn multiple_groups_all_emitted() {
    let groups = vec!["alt.binaries.a".into(), "alt.binaries.b".into()];
    let xml = generate(
        &groups,
        &[seg("f.bin", 1, 1, "<id@x>")],
        &no_meta(),
        ObfuscateMode::None,
    );
    assert!(xml.contains("<group>alt.binaries.a</group>"));
    assert!(xml.contains("<group>alt.binaries.b</group>"));
    assert_eq!(xml.matches("<group>").count(), 2);
}

#[test]
fn single_part_subject_still_carries_1_of_1() {
    // Regression for issue #68: a single-segment file's subject must
    // have the same `(part/total)` shape as multi-segment siblings, or
    // some indexers hash it into a separate collection instead of
    // grouping it with the rest of the release.
    let xml = generate(
        &["alt.test".into()],
        &[seg("file.bin", 1, 1, "<id@x>")],
        &no_meta(),
        ObfuscateMode::None,
    );
    assert!(xml.contains("subject=\"&quot;file.bin&quot; yEnc (1/1)\""));
}

#[test]
fn escape_drops_xml_illegal_controls() {
    assert_eq!(escape("ok\u{0001}name"), "okname");
    assert_eq!(escape("keep\ttab"), "keep\ttab");
}

#[test]
fn escape_apostrophe() {
    let segments = vec![seg("it's.bin", 1, 1, "<id@x>")];
    let xml = generate(
        &["alt.test".into()],
        &segments,
        &no_meta(),
        ObfuscateMode::None,
    );
    assert!(xml.contains("it&apos;s.bin"), "apostrophe must be escaped");
    assert!(!xml.contains("it's.bin"));
}

#[test]
fn file_name_with_slash_is_not_escaped() {
    // A relative path like "Season01/ep01.mkv" — forward slash is not an
    // XML entity and must appear verbatim in the subject's quoted name.
    let mut s = seg("Season01/ep01.mkv", 1, 1, "<id@x>");
    s.file_name = "Season01/ep01.mkv".into();
    s.subject_name = "Season01/ep01.mkv".into();
    let xml = generate(&["alt.test".into()], &[s], &no_meta(), ObfuscateMode::None);
    assert!(xml.contains("subject=\"&quot;Season01/ep01.mkv&quot; yEnc (1/1)\""));
}

#[test]
fn subject_always_shows_part_one_of_total() {
    // write_file always emits "(1/N)" regardless of which parts are present.
    // This is intentional — the subject describes the file, not a segment.
    let segments = vec![
        seg("big.bin", 2, 5, "<a2@x>"),
        seg("big.bin", 3, 5, "<a3@x>"),
    ];
    let xml = generate(
        &["alt.test".into()],
        &segments,
        &no_meta(),
        ObfuscateMode::None,
    );
    assert!(xml.contains("subject=\"&quot;big.bin&quot; yEnc (1/5)\""));
    assert!(!xml.contains("(2/5)"));
}

#[test]
fn segment_bytes_attribute_is_exact() {
    let mut s = seg("f.bin", 1, 1, "<id@x>");
    s.bytes = 123_456;
    let xml = generate(&["alt.test".into()], &[s], &no_meta(), ObfuscateMode::None);
    assert!(xml.contains("bytes=\"123456\""));
}

#[test]
fn date_attribute_is_a_nonzero_number() {
    let xml = generate(
        &["alt.test".into()],
        &[seg("f.bin", 1, 1, "<id@x>")],
        &no_meta(),
        ObfuscateMode::None,
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
    let xml = generate(&["alt.test".into()], &[], &meta, ObfuscateMode::None);
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
    let xml = generate(&["alt.test".into()], &[], &meta, ObfuscateMode::None);
    assert!(xml.contains("<meta type=\"tag\">a&amp;b&lt;c&gt;</meta>"));
}

#[test]
fn empty_tags_emit_no_tag_meta() {
    let xml = generate(&["alt.test".into()], &[], &no_meta(), ObfuscateMode::None);
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
    let xml = generate(&["alt.test".into()], &[], &meta, ObfuscateMode::None);
    assert!(xml.contains("<meta type=\"password\">hunter2</meta>"));
    assert!(!xml.contains("type=\"title\""));
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
    let xml = generate(&groups, &segs, &meta, ObfuscateMode::None);
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
    let xml = generate(&groups, &segs, &no_meta(), ObfuscateMode::None);
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
    let xml = generate(&["alt.test".into()], &segs, &no_meta(), ObfuscateMode::None);
    let parsed = parse(&xml).expect("parse must succeed");
    // message_id must carry angle brackets.
    assert_eq!(parsed.segments[0].message_id, "<msgid@host>");
}

/// Real-world NZBs (every indexer/posting tool, `pesto` included) never
/// write a `name` attribute on `<file>` — only `subject`, per the
/// standard NZB 1.1 DTD. `parse()` must derive the filename from the
/// quoted string inside `subject` in that case instead of erroring, or
/// `penne` could never download anything at all.
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

// ── wire_subject / wire_subjects ─────────────────────────────────────────

fn obf(name: &str, part: u32, total: u32, id: &str, wire: &str) -> PostedSegment {
    PostedSegment {
        wire_name: Arc::from(wire),
        wire_yenc_name: Arc::from(wire),
        ..seg(name, part, total, id)
    }
}

#[test]
fn wire_subject_uses_the_wire_name_not_the_real_name() {
    let segments = vec![
        obf("movie.mkv", 1, 2, "id1", "aB3xyz"),
        obf("movie.mkv", 2, 2, "id2", "aB3xyz"),
    ];
    assert_eq!(
        wire_subject(&segments).as_deref(),
        Some("\"aB3xyz\" yEnc (1/2)")
    );
}

#[test]
fn wire_subject_none_when_no_segments() {
    assert_eq!(wire_subject(&[]), None);
}

#[test]
fn wire_subject_none_when_wire_name_is_empty() {
    // Segments reconstructed from a parsed `.nzb` never re-encode, so
    // `wire_name` is left empty (see `parse`).
    let segments = vec![obf("movie.mkv", 1, 1, "id1", "")];
    assert_eq!(wire_subject(&segments), None);
}

#[test]
fn wire_subjects_returns_one_entry_per_file() {
    // A video file plus its PAR2 volumes, each with an independently
    // obfuscated wire identity — the `Full`/`Article` case.
    let segments = vec![
        obf("movie.mkv", 1, 2, "id1", "aB3xyz"),
        obf("movie.mkv", 2, 2, "id2", "aB3xyz"),
        obf("movie.par2", 1, 1, "id3", "Qz9wvu"),
    ];
    assert_eq!(
        wire_subjects(&segments),
        vec![
            ("movie.mkv".to_string(), "\"aB3xyz\" yEnc (1/2)".to_string()),
            (
                "movie.par2".to_string(),
                "\"Qz9wvu\" yEnc (1/1)".to_string()
            ),
        ]
    );
}

#[test]
fn wire_subjects_empty_when_no_segments() {
    assert_eq!(wire_subjects(&[]), Vec::<(String, String)>::new());
}
