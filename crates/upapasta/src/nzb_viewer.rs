//! NZB archive viewer — parse and display `.nzb` files in the History screen.

use anyhow::{Context, Result};
use quick_xml::events::Event;
use quick_xml::reader::Reader;
use std::fs;

/// One `<file>` entry from an NZB.
#[derive(Debug, Clone)]
pub struct NzbFile {
    pub name: String,
    pub groups: Vec<String>,
    pub segment_count: u32,
    pub total_bytes: u64,
}

/// Parsed contents of an `.nzb` file.
#[derive(Debug, Clone)]
pub struct NzbContents {
    /// `<meta type="name">` value, if present.
    pub meta_name: Option<String>,
    /// `<meta type="password">` value, if present.
    pub meta_password: Option<String>,
    /// `<meta type="category">` value, if present.
    pub meta_category: Option<String>,
    /// Files contained in the NZB.
    pub files: Vec<NzbFile>,
}

impl NzbContents {
    /// Total bytes across all files.
    pub fn total_bytes(&self) -> u64 {
        self.files.iter().map(|f| f.total_bytes).sum()
    }

    /// Total segment count across all files.
    pub fn total_segments(&self) -> u32 {
        self.files.iter().map(|f| f.segment_count).sum()
    }
}

/// TUI state for the NZB viewer overlay.
#[derive(Debug, Clone)]
pub struct NzbViewerState {
    pub contents: NzbContents,
    pub scroll: usize,
}

/// Parse an `.nzb` file from disk.
pub fn parse_nzb(path: &str) -> Result<NzbContents> {
    let raw = fs::read_to_string(path).with_context(|| format!("reading {path}"))?;
    parse_nzb_str(&raw)
}

/// Parse NZB XML from a string slice.
pub fn parse_nzb_str(xml: &str) -> Result<NzbContents> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);

    let mut meta_name: Option<String> = None;
    let mut meta_password: Option<String> = None;
    let mut meta_category: Option<String> = None;
    let mut files: Vec<NzbFile> = Vec::new();

    // Parser state
    let mut current_file: Option<NzbFile> = None;
    let mut current_meta_type: Option<String> = None;
    let mut in_segment = false;

    let mut buf = Vec::new();
    loop {
        match reader.read_event_into(&mut buf)? {
            Event::Start(e) => {
                let name = e.name();
                match name.as_ref() {
                    b"file" => {
                        let file_name = attr_str(&e, b"name").unwrap_or_default();
                        current_file = Some(NzbFile {
                            name: file_name,
                            groups: Vec::new(),
                            segment_count: 0,
                            total_bytes: 0,
                        });
                    }
                    b"meta" => {
                        current_meta_type = attr_str(&e, b"type");
                    }
                    b"group" => {}
                    b"segment" => {
                        in_segment = true;
                        let bytes = attr_u64(&e, b"bytes").unwrap_or(0);
                        if let Some(ref mut f) = current_file {
                            f.segment_count += 1;
                            f.total_bytes += bytes;
                        }
                    }
                    _ => {}
                }
            }
            Event::End(e) => match e.name().as_ref() {
                b"file" => {
                    if let Some(f) = current_file.take() {
                        files.push(f);
                    }
                }
                b"meta" => {
                    current_meta_type = None;
                }
                b"segment" => {
                    in_segment = false;
                }
                _ => {}
            },
            Event::Text(e) => {
                let text = e.unescape()?.into_owned();
                if let Some(ref mt) = current_meta_type {
                    match mt.as_str() {
                        "name" => meta_name = Some(text),
                        "password" => meta_password = Some(text),
                        "category" => meta_category = Some(text),
                        _ => {}
                    }
                } else if let Some(ref mut f) = current_file {
                    // Inside <group>...</group>
                    if !in_segment && !text.is_empty() && text.contains('.') {
                        // Heuristic: group names contain dots (alt.binaries.*)
                        f.groups.push(text);
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
        buf.clear();
    }

    Ok(NzbContents {
        meta_name,
        meta_password,
        meta_category,
        files,
    })
}

fn attr_str(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<String> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| String::from_utf8(a.value.into_owned()).ok())
        .map(|s| unescape_xml(&s))
}

fn attr_u64(e: &quick_xml::events::BytesStart, key: &[u8]) -> Option<u64> {
    e.attributes()
        .filter_map(|a| a.ok())
        .find(|a| a.key.as_ref() == key)
        .and_then(|a| String::from_utf8(a.value.into_owned()).ok())
        .and_then(|s| s.parse().ok())
}

fn unescape_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="name">My Movie</meta>
    <meta type="password">hunter2</meta>
    <meta type="category">Movies</meta>
  </head>
  <file name="movie.mkv" poster="test &lt;t@x&gt;" date="1700000000" subject="movie.mkv (1/3)">
    <groups>
      <group>alt.binaries.movies</group>
    </groups>
    <segments>
      <segment bytes="750000" number="1">id1@x</segment>
      <segment bytes="750000" number="2">id2@x</segment>
      <segment bytes="300000" number="3">id3@x</segment>
    </segments>
  </file>
  <file name="movie.par2" poster="test &lt;t@x&gt;" date="1700000001" subject="movie.par2">
    <groups>
      <group>alt.binaries.movies</group>
    </groups>
    <segments>
      <segment bytes="100000" number="1">id4@x</segment>
    </segments>
  </file>
</nzb>"#;

    #[test]
    fn parses_meta() {
        let c = parse_nzb_str(SAMPLE).unwrap();
        assert_eq!(c.meta_name.as_deref(), Some("My Movie"));
        assert_eq!(c.meta_password.as_deref(), Some("hunter2"));
        assert_eq!(c.meta_category.as_deref(), Some("Movies"));
    }

    #[test]
    fn parses_files_and_segments() {
        let c = parse_nzb_str(SAMPLE).unwrap();
        assert_eq!(c.files.len(), 2);
        assert_eq!(c.files[0].name, "movie.mkv");
        assert_eq!(c.files[0].segment_count, 3);
        assert_eq!(c.files[0].total_bytes, 1_800_000);
        assert_eq!(c.files[1].name, "movie.par2");
        assert_eq!(c.files[1].segment_count, 1);
        assert_eq!(c.files[1].total_bytes, 100_000);
    }

    #[test]
    fn parses_groups() {
        let c = parse_nzb_str(SAMPLE).unwrap();
        assert_eq!(c.files[0].groups, vec!["alt.binaries.movies"]);
    }

    #[test]
    fn totals() {
        let c = parse_nzb_str(SAMPLE).unwrap();
        assert_eq!(c.total_bytes(), 1_900_000);
        assert_eq!(c.total_segments(), 4);
    }

    #[test]
    fn file_name_correct() {
        let c = parse_nzb_str(SAMPLE).unwrap();
        assert_eq!(c.files[0].name, "movie.mkv");
    }
}
