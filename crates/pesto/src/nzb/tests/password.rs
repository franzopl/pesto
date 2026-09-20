use super::*;

#[test]
fn bare_ampersand_nzb_parses_as_upstream() {
    // Ordinary-world NZBs with bare & in subject or group names
    // must parse cleanly without strict XML entity parsing errors.
    let xml = r#"<?xml version="1.0" encoding="iso-8859-1"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="title">Tom & Jerry The Movie</meta>
    <meta type="category">TV & Film</meta>
  </head>
  <file poster="poster & co" date="1700000000" subject="&quot;Tom &amp; Jerry &amp; Friends.bin&quot; yEnc (1/1)">
    <groups>
      <group>alt.binaries.cartoons & stuff</group>
    </groups>
    <segments>
      <segment bytes="1024" number="1">part1@news.example.com</segment>
    </segments>
  </file>
</nzb>"#;

    let parsed = parse(xml).expect("bare ampersand in ordinary NZB must parse as upstream");
    assert_eq!(parsed.meta.name.as_deref(), Some("Tom & Jerry The Movie"));
    assert_eq!(parsed.meta.category.as_deref(), Some("TV & Film"));
    assert_eq!(parsed.groups, vec!["alt.binaries.cartoons & stuff"]);
    assert_eq!(parsed.segments.len(), 1);
    assert_eq!(parsed.segments[0].bytes, 1024);
}

#[test]
fn password_preserves_lf_tab_non_ascii_verbatim() {
    // passwords containing LF, tabs, and non-ASCII UTF-8 bytes
    // must survive generation and parsing without alteration or trimming.
    let complex_password = "first-line\nsecond-line\twith tab and Schlüssel🔑";
    let meta = NzbMeta {
        password: Some(complex_password.to_string()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let s = seg("data.bin", 1, 1, "<msg@test>");
    let xml = generate(
        &["alt.binaries.test".into()],
        &[s],
        &meta,
        ObfuscateMode::None,
    );

    let parsed = parse(&xml).expect("generated XML with complex password must parse");
    assert_eq!(
        parsed.meta.password.as_deref(),
        Some(complex_password),
        "password with LF, tab, and UTF-8 characters must match byte-for-byte"
    );
}

#[test]
fn xml_unrepresentable_password_rejected_by_validation() {
    // XML-unrepresentable transport password characters must be
    // rejected before posting (no silent dropping or modification).
    use crate::config::validation::{is_xml_representable, validate_encryption};

    assert!(!is_xml_representable("bad\u{0001}pass"));
    assert!(!is_xml_representable("bad\u{0000}pass"));
    assert!(!is_xml_representable("bad\u{000b}pass")); // vertical tab is forbidden in XML 1.0
    assert!(is_xml_representable("valid\t\n\rpass 🔑"));

    let err = validate_encryption(Some("forbidden\u{0001}"))
        .expect_err("XML-forbidden transport password must be rejected");
    assert!(err.to_string().contains("cannot be represented in XML"));
}

#[test]
fn escape_drops_c0_controls_in_title() {
    let meta = NzbMeta {
        name: Some("Title\u{0001}With\u{0008}Controls\u{001f}".into()),
        ..Default::default()
    };
    let xml = generate(&["alt.test".into()], &[], &meta, ObfuscateMode::None);
    assert!(xml.contains("<meta type=\"title\">TitleWithControls</meta>"));
}

#[test]
fn encrypted_password_meta_is_not_filtered() {
    let meta = NzbMeta {
        password: Some("secret\u{0080}with\u{009f}c1".into()),
        yenc_encrypted: true,
        ..Default::default()
    };
    let xml = generate(&["alt.test".into()], &[], &meta, ObfuscateMode::None);
    assert!(xml.contains("<meta type=\"password\">secret\u{0080}with\u{009f}c1</meta>"));
}

#[test]
fn archive_password_meta_filtered_as_upstream() {
    let meta = NzbMeta {
        password: Some("secret\u{0001}\u{0080}rar".into()),
        yenc_encrypted: false,
        ..Default::default()
    };
    let xml = generate(&["alt.test".into()], &[], &meta, ObfuscateMode::None);
    assert!(xml.contains("<meta type=\"password\">secretrar</meta>"));
}
