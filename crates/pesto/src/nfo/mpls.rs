//! Blu-ray MPLS playlist parsing and language-tag injection.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Return the `.mpls` with the longest duration inside `disc_root/BDMV/PLAYLIST/`.
///
/// Language tags for audio and subtitle streams live in the playlist, not the
/// raw `.m2ts` clip. `mediainfo` parses `.mpls` natively and joins language
/// info from the playlist to codec info from the referenced clip.
///
/// Duration is the reliable heuristic — the main feature is always the longest
/// title. File size is used only as a fallback when `mediainfo` is unavailable.
pub(super) fn find_main_mpls(disc_root: &Path) -> Option<PathBuf> {
    let playlist = disc_root.join("BDMV").join("PLAYLIST");
    let mut playlists: Vec<PathBuf> = std::fs::read_dir(&playlist)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mpls"))
                .unwrap_or(false)
        })
        .collect();
    playlists.sort();

    // Prefer the playlist with the longest duration (main feature = longest title).
    let best = playlists
        .iter()
        .filter_map(|p| {
            let ms = super::mediainfo_duration_ms(p)?;
            Some((ms, p.clone()))
        })
        .max_by_key(|(ms, _)| *ms)
        .map(|(_, p)| p);

    // Fall back to largest by file size when mediainfo is unavailable.
    best.or_else(|| {
        playlists
            .into_iter()
            .max_by_key(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
    })
}

// ── MPLS binary parser ───────────────────────────────────────────────────────

/// Parse a Blu-ray `.mpls` playlist and return a map of stream PID → language name.
///
/// The language tag for each audio/subtitle stream lives in the STN_table of
/// each PlayItem. `mediainfo` sometimes omits it for PGS subtitle streams even
/// though the data is present in the playlist.
pub(super) fn mpls_language_map(path: &Path) -> HashMap<u16, String> {
    std::fs::read(path)
        .ok()
        .and_then(|data| parse_mpls(&data))
        .unwrap_or_default()
}

/// Top-level MPLS parser. Returns `None` only on structural failures (bad
/// magic, truncation); an empty map is returned when no language tags are found.
pub(super) fn parse_mpls(data: &[u8]) -> Option<HashMap<u16, String>> {
    // Header: "MPLS" + version (4 bytes) + AppInfo offset (4) + PlayList offset (4) + ...
    if data.len() < 24 || &data[0..4] != b"MPLS" {
        return None;
    }
    // Empirically, header[8..12] points to the PlayList section (PlayItems +
    // STN_tables) for both MPLS 0200 and 0300. header[12..16] points to a
    // compact AppInfo section. The Blu-ray spec labels them the other way, but
    // real disc files consistently use this layout.
    let playlist_start = mpls_u32(data, 8)? as usize;
    if data.len() < playlist_start + 10 {
        return None;
    }

    // PlayList: length(4) reserved(2) n_items(2) n_subpaths(2) items...
    let n_items = mpls_u16(data, playlist_start + 6)? as usize;
    let mut map: HashMap<u16, String> = HashMap::new();
    let mut pos = playlist_start + 10;

    for _ in 0..n_items {
        if pos + 2 > data.len() {
            break;
        }
        let item_len = mpls_u16(data, pos)? as usize;
        let item_end = pos + 2 + item_len;
        if item_end > data.len() {
            break;
        }

        // PlayItem content starts at pos+2:
        //  [0..4]  ClipInformationFileName  (5 bytes)
        //  [5..8]  ClipCodecIdentifier      (4 bytes)
        //  [9]     reserved(3b)|is_multi_angle(1b)|connection_condition(4b)
        //  [10]    ref_to_STC_id
        //  [11..14] IN_time
        //  [15..18] OUT_time
        //  [19..26] UO_mask_table           (8 bytes)
        //  [27]    random_access_flag|reserved
        //  [28]    still_mode
        //  [29..30] still_time/reserved
        //  [31+]   [multi-angle entries]   STN_table
        let base = pos + 2;
        if base + 31 > data.len() {
            pos = item_end;
            continue;
        }

        let is_multi_angle = (data[base + 9] >> 4) & 1 == 1;
        // PlayItem fixed fields before STN_table:
        // ClipFilename(5) + Codec(4) + flags(2) + stc_id(1) + IN(4) + OUT(4)
        // + UO_mask(8) + random_access(1) + still_mode(1) + still_time(2) = 32
        let mut stn_pos = base + 32;

        if is_multi_angle {
            if stn_pos + 2 > data.len() {
                pos = item_end;
                continue;
            }
            let n_angles = data[stn_pos] as usize;
            stn_pos += 2; // number_of_angles(1) + flags(1)
            stn_pos += n_angles.saturating_sub(1) * 10; // each extra angle: name(5)+codec(4)+stc(1)
        }

        parse_stn_table(data, stn_pos, &mut map);
        pos = item_end;
    }

    Some(map)
}

/// Parse one STN_table and insert PID → language entries into `map`.
pub(super) fn parse_stn_table(data: &[u8], offset: usize, map: &mut HashMap<u16, String>) {
    // STN_table: length(2) reserved(2) counts(8) reserved(4) entries...
    let stn_len = match mpls_u16(data, offset) {
        Some(v) => v as usize,
        None => return,
    };
    let stn_end = offset + 2 + stn_len;
    if stn_end > data.len() || offset + 16 > data.len() {
        return;
    }

    let total_streams: usize = data[offset + 4..offset + 12]
        .iter()
        .map(|&c| c as usize)
        .sum();

    let mut pos = offset + 16;

    for _ in 0..total_streams {
        if pos + 2 > stn_end {
            break;
        }

        // StreamEntry: length(1) stream_type(1) payload...
        let entry_len = data[pos] as usize;
        if pos + 1 + entry_len > stn_end {
            break;
        }
        let stream_type = data[pos + 1];
        let pid: Option<u16> = match stream_type {
            1 if entry_len >= 3 => mpls_u16(data, pos + 2),
            2 | 3 if entry_len >= 5 => mpls_u16(data, pos + 4),
            4 if entry_len >= 4 => mpls_u16(data, pos + 3),
            _ => None,
        };
        pos += 1 + entry_len;

        // StreamAttributes: length(1) coding_type(1) payload...
        if pos >= stn_end {
            break;
        }
        let attr_len = data[pos] as usize;
        if pos + 1 + attr_len > stn_end {
            break;
        }

        if let Some(pid) = pid.filter(|&p| p != 0) {
            if attr_len >= 1 {
                let coding_type = data[pos + 1];
                // Audio: coding_type(1) + channel/rate(1) + lang(3) → attr_len >= 5
                // PGS/IG: coding_type(1) + lang(3)                  → attr_len >= 4
                // TextST: coding_type(1) + charset(1) + lang(3)     → attr_len >= 5
                let lang_bytes: Option<&[u8]> = match coding_type {
                    0x80 | 0x81 | 0x82 | 0x83 | 0x84 | 0x85 | 0x86 | 0xA1 | 0xA2
                        if attr_len >= 5 =>
                    {
                        Some(&data[pos + 3..pos + 6])
                    }
                    0x90 | 0x91 if attr_len >= 4 => Some(&data[pos + 2..pos + 5]),
                    0x92 if attr_len >= 5 => Some(&data[pos + 3..pos + 6]),
                    _ => None,
                };
                if let Some(bytes) = lang_bytes {
                    if let Ok(code) = std::str::from_utf8(bytes) {
                        let code = code.trim_matches('\0').trim();
                        if !code.is_empty() {
                            let name = iso639_2_to_name(code);
                            map.entry(pid).or_insert(name);
                        }
                    }
                }
            }
        }

        pos += 1 + attr_len;
    }
}

/// Convert an ISO 639-2 bibliographic language code to an English display name.
/// Falls back to the raw code for less common languages.
pub(super) fn iso639_2_to_name(code: &str) -> String {
    let name = match &*code.to_ascii_lowercase() {
        "eng" => "English",
        "deu" | "ger" => "German",
        "fra" | "fre" => "French",
        "spa" => "Spanish",
        "ita" => "Italian",
        "jpn" => "Japanese",
        "zho" | "chi" => "Chinese",
        "kor" => "Korean",
        "por" => "Portuguese",
        "rus" => "Russian",
        "ara" => "Arabic",
        "pol" => "Polish",
        "nld" | "dut" => "Dutch",
        "swe" => "Swedish",
        "nor" => "Norwegian",
        "dan" => "Danish",
        "fin" => "Finnish",
        "hun" => "Hungarian",
        "ces" | "cze" => "Czech",
        "slk" | "slo" => "Slovak",
        "ron" | "rum" => "Romanian",
        "hrv" => "Croatian",
        "srp" => "Serbian",
        "bul" => "Bulgarian",
        "ukr" => "Ukrainian",
        "tur" => "Turkish",
        "ell" | "gre" => "Greek",
        "heb" => "Hebrew",
        "tha" => "Thai",
        "vie" => "Vietnamese",
        "ind" => "Indonesian",
        "msa" | "may" => "Malay",
        "cat" => "Catalan",
        "zxx" => "(No linguistic content)",
        _ => code,
    };
    name.to_owned()
}

/// Inject missing `Language` lines into `mediainfo` text output using the
/// PID → language map parsed from the `.mpls` playlist.
///
/// `mediainfo` reports language for audio streams but sometimes omits it for
/// PGS subtitle streams. This walks the output section by section, looks up
/// the PID of each stream, and inserts a `Language` line after the `ID` line
/// when the section lacks one.
pub(super) fn inject_language_tags(output: &str, lang_map: &HashMap<u16, String>) -> String {
    if lang_map.is_empty() {
        return output.to_owned();
    }

    // Split into sections. A section header is a non-empty, non-indented line
    // that doesn't look like a "Key  :  Value" line (e.g. "General", "Text #3").
    let mut sections: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();

    for line in output.lines() {
        let is_header = !line.is_empty()
            && !line.starts_with(' ')
            && !line.starts_with('\t')
            && line
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic())
                .unwrap_or(false)
            && !line.contains(" : ");

        if is_header && !current.is_empty() {
            sections.push(std::mem::take(&mut current));
        }
        current.push(line.to_owned());
    }
    if !current.is_empty() {
        sections.push(current);
    }

    let mut out: Vec<String> = Vec::with_capacity(sections.len());

    for mut section in sections {
        // Nothing to do if the section already carries a Language line.
        if section
            .iter()
            .any(|l| l.trim_start().starts_with("Language"))
        {
            out.push(section.join("\n"));
            continue;
        }

        // Extract PID from the "ID  :  1234 (0x04D2)" line.
        let pid = section.iter().find_map(|line| {
            let t = line.trim_start();
            if !t.starts_with("ID ") && !t.starts_with("ID\t") {
                return None;
            }
            let hex_start = line.find("(0x")?;
            let after = &line[hex_start + 3..];
            let end = after.find(')')?;
            u16::from_str_radix(&after[..end], 16).ok()
        });

        if let Some(lang) = pid.and_then(|p| lang_map.get(&p)) {
            // Insert "Language" immediately after the "ID" line.
            let id_pos = section.iter().position(|l| {
                let t = l.trim_start();
                t.starts_with("ID ") || t.starts_with("ID\t")
            });
            if let Some(pos) = id_pos {
                section.insert(
                    pos + 1,
                    format!("Language                                 : {lang}"),
                );
            }
        }

        out.push(section.join("\n"));
    }

    out.join("\n")
}

pub(super) fn mpls_u16(data: &[u8], offset: usize) -> Option<u16> {
    data.get(offset..offset + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_be_bytes)
}

pub(super) fn mpls_u32(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes)
}
