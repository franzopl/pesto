//! DVD IFO parsing: ordered language tables and title selection.

use std::path::{Path, PathBuf};

/// Parse the ordered audio and subtitle language codes from `mediainfo --Details=1`
/// output for a DVD IFO file.
///
/// The IFO VTSI_MAT table declares streams in index order. The Details output
/// exposes them as consecutive "Audio (8 bytes)" / "Text (6 bytes)" blocks inside
/// the "VTS (VTS for movie…)" section, each followed by a "Language code: XX" line.
/// We collect only entries where `Language type` is `1` (2CC), which means a valid
/// ISO 639-1 code is present; `Unknown` (type 0) entries produce an empty string
/// placeholder so that index alignment with stream IDs is preserved.
///
/// Returns `(audio_langs, subtitle_langs)` where each vec is ordered by stream
/// index (0-based).
pub(super) fn parse_ifo_language_tables(ifo: &Path) -> (Vec<String>, Vec<String>) {
    let abs = ifo.canonicalize().unwrap_or_else(|_| ifo.to_path_buf());
    let output = match std::process::Command::new("mediainfo")
        .arg("--Details=1")
        .arg(&abs)
        .output()
    {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => return (vec![], vec![]),
    };

    // We only want the VTS section (movie content, not VTSM menus).
    // It starts at the line containing "VTS (VTS for movie" and runs until the
    // next top-level section (a line whose offset jumps significantly, but in
    // practice we just scan forward and stop when we leave the 0x02xx block).
    let mut in_vts = false;
    let mut in_audio_block = false;
    let mut in_text_block = false;

    // current stream being accumulated
    let mut current_lang_type: Option<u8> = None; // 0=Unknown, 1=2CC
    let mut current_lang_code = String::new();

    let mut audio_langs: Vec<String> = Vec::new();
    let mut subtitle_langs: Vec<String> = Vec::new();

    for line in output.lines() {
        let trimmed = line.trim();

        // Detect entry into the VTS movie section.
        if trimmed.contains("VTS (VTS for movie") {
            in_vts = true;
            in_audio_block = false;
            in_text_block = false;
            continue;
        }

        if !in_vts {
            continue;
        }

        // Detect the audio / subtitle count headers, which delimit the blocks.
        if trimmed.contains("Audio streams -") && trimmed.contains("streams") {
            // Flush any pending stream from a previous block.
            in_audio_block = true;
            in_text_block = false;
            current_lang_type = None;
            current_lang_code.clear();
            continue;
        }
        if trimmed.contains("Text streams -") && trimmed.contains("streams") {
            in_audio_block = false;
            in_text_block = true;
            current_lang_type = None;
            current_lang_code.clear();
            continue;
        }

        // Lines in --Details=1 output are prefixed with a hex offset, e.g.:
        //   "00204    Language type:                       1 (0x1) - 2CC"
        // Strip that prefix to get the semantic content.
        let content = trimmed
            .trim_start_matches(|c: char| c.is_ascii_hexdigit())
            .trim_start();

        // Leaving the stream attribute area: a line for a new top-level block.
        if (in_audio_block || in_text_block)
            && (content.starts_with("Reserved for Audio")
                || content.starts_with("Reserved for Text")
                || content.starts_with("Unknown:"))
        {
            // Flush the current stream (may be empty if no lang code seen yet).
            if current_lang_type.is_some() {
                let lang = if current_lang_type == Some(1) {
                    current_lang_code.clone()
                } else {
                    String::new()
                };
                if in_audio_block {
                    audio_langs.push(lang);
                } else {
                    subtitle_langs.push(lang);
                }
                current_lang_type = None;
                current_lang_code.clear();
            }
            continue;
        }

        // Inside a stream attribute block, pick up Language type and code.
        if in_audio_block || in_text_block {
            if content.starts_with("Language type:") {
                // Flush any previously accumulated stream.
                if current_lang_type.is_some() {
                    let lang = if current_lang_type == Some(1) {
                        current_lang_code.clone()
                    } else {
                        String::new()
                    };
                    if in_audio_block {
                        audio_langs.push(lang);
                    } else {
                        subtitle_langs.push(lang);
                    }
                    current_lang_code.clear();
                }
                // Parse type value: "Language type:                       1 (0x1) - 2CC"
                current_lang_type = content
                    .trim_start_matches("Language type:")
                    .split_whitespace()
                    .next()
                    .and_then(|s| s.parse::<u8>().ok());
            } else if content.starts_with("Language code:") {
                // "Language code:                       en"
                current_lang_code = content
                    .trim_start_matches("Language code:")
                    .trim()
                    .to_lowercase();
            } else if content.starts_with("Audio (") {
                // New audio stream sub-block; flush previous if any.
                if current_lang_type.is_some() {
                    let lang = if current_lang_type == Some(1) {
                        current_lang_code.clone()
                    } else {
                        String::new()
                    };
                    audio_langs.push(lang);
                    current_lang_type = None;
                    current_lang_code.clear();
                }
            } else if content.starts_with("Text (") {
                // New subtitle stream sub-block; flush previous if any.
                if current_lang_type.is_some() {
                    let lang = if current_lang_type == Some(1) {
                        current_lang_code.clone()
                    } else {
                        String::new()
                    };
                    subtitle_langs.push(lang);
                    current_lang_type = None;
                    current_lang_code.clear();
                }
            }
        }
    }

    // Flush the last pending stream.
    if current_lang_type.is_some() {
        let lang = if current_lang_type == Some(1) {
            current_lang_code.clone()
        } else {
            String::new()
        };
        if in_audio_block {
            audio_langs.push(lang);
        } else {
            subtitle_langs.push(lang);
        }
    }

    (audio_langs, subtitle_langs)
}

/// Inject missing `Language` lines into `mediainfo` text output for a DVD IFO.
///
/// `mediainfo` sometimes omits language tags for audio and subtitle streams when
/// the first entry in the IFO's VTSI_MAT language table has `Language type: 0
/// (Unknown)`. The language data is present in the IFO binary; this function
/// reads it via `--Details=1` and injects it into the normal output.
///
/// DVD stream ID mapping (sub-ID portion of "189 (0xBD)-NNN"):
/// - Audio:    sub_id - 0x80 → audio_langs index
/// - Subtitle: (sub_id - 0x20) % subtitle_langs.len() → subtitle_langs index
///   (wide, letterbox and pan&scan variants share the same language table)
pub(super) fn inject_dvd_language_tags(mi_output: &str, ifo: &Path) -> String {
    // Fast path: if every stream already has a Language line, do nothing.
    let needs_injection = {
        let mut in_av_section = false;
        let mut has_lang = false;
        let mut missing = false;
        for line in mi_output.lines() {
            let t = line.trim_start();
            if t.starts_with("Audio") || t.starts_with("Text") {
                if in_av_section && !has_lang {
                    missing = true;
                    break;
                }
                in_av_section = true;
                has_lang = false;
            } else if t.starts_with("Language") {
                has_lang = true;
            }
        }
        if in_av_section && !has_lang {
            missing = true;
        }
        missing
    };

    if !needs_injection {
        return mi_output.to_owned();
    }

    let (audio_langs, subtitle_langs) = parse_ifo_language_tables(ifo);
    if audio_langs.is_empty() && subtitle_langs.is_empty() {
        return mi_output.to_owned();
    }

    // Split output into sections (same logic as inject_language_tags for MPLS).
    let mut sections: Vec<Vec<String>> = Vec::new();
    let mut current: Vec<String> = Vec::new();
    for line in mi_output.lines() {
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
        // Skip sections that already have a Language line.
        if section
            .iter()
            .any(|l| l.trim_start().starts_with("Language"))
        {
            out.push(section.join("\n"));
            continue;
        }

        let header = section.first().map(|s| s.as_str()).unwrap_or("");
        let is_audio = header.starts_with("Audio");
        let is_text = header.starts_with("Text");

        if !is_audio && !is_text {
            out.push(section.join("\n"));
            continue;
        }

        // Extract the sub-ID from "ID : 189 (0xBD)-130 (0x82)".
        let sub_id: Option<u8> = section.iter().find_map(|line| {
            let t = line.trim_start();
            if !t.starts_with("ID ") && !t.starts_with("ID\t") {
                return None;
            }
            // There may be two hex groups; we want the second one after '-'.
            let after_dash = line
                .rfind('-')
                .map(|i| &line[i + 1..])
                .unwrap_or(line.as_str());
            let hex_start = after_dash.find("(0x")?;
            let after = &after_dash[hex_start + 3..];
            let end = after.find(')')?;
            u8::from_str_radix(&after[..end], 16).ok()
        });

        let lang: Option<&str> = sub_id.and_then(|sid| {
            if is_audio && sid >= 0x80 {
                let idx = (sid - 0x80) as usize;
                audio_langs.get(idx).map(|s| s.as_str())
            } else if is_text && sid >= 0x20 && !subtitle_langs.is_empty() {
                let idx = ((sid - 0x20) as usize) % subtitle_langs.len();
                subtitle_langs.get(idx).map(|s| s.as_str())
            } else {
                None
            }
        });

        if let Some(lang) = lang.filter(|s| !s.is_empty()) {
            // Insert Language after the ID line.
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

/// Find DVD disc roots by locating DVD-Video marker files anywhere under `path`.
///
/// Accepts VIDEO_TS.IFO, VIDEO_TS.BUP, or VIDEO_TS.VOB as markers. Per the
/// DVD-Video spec (ECMA-267 / DVD-Books Part 3), VIDEO_TS.IFO and VIDEO_TS.BUP
/// are mandatory; VIDEO_TS.VOB (the Video Manager Menu) is optional and stripped
/// on many scene releases. Handles both the standard layout
/// (`<disc_root>/VIDEO_TS/VIDEO_TS.IFO`) and the flat layout where files sit
/// directly in the disc root (`<disc_root>/VIDEO_TS.IFO`).
pub(super) fn find_dvd_disc_roots(path: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    collect_dvd_roots(path, &mut roots);
    roots.sort();
    roots.dedup();
    roots
}

pub(super) fn collect_dvd_roots(dir: &Path, roots: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let child = entry.path();
        if child.is_dir() {
            collect_dvd_roots(&child, roots);
        } else if child
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| {
                let u = n.to_ascii_uppercase();
                // VIDEO_TS.IFO and VIDEO_TS.BUP are mandatory per the DVD-Video
                // spec; VIDEO_TS.VOB (the Video Manager Menu) is optional and
                // stripped on many scene releases. Accept any of the three.
                u == "VIDEO_TS.IFO" || u == "VIDEO_TS.BUP" || u == "VIDEO_TS.VOB"
            })
            .unwrap_or(false)
        {
            // The marker file lives either inside a VIDEO_TS/ subfolder
            // (standard layout) or directly in the disc root (flat layout).
            if let Some(parent) = child.parent() {
                let is_inside_video_ts = parent
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.eq_ignore_ascii_case("VIDEO_TS"))
                    .unwrap_or(false);
                let disc_root = if is_inside_video_ts {
                    parent.parent().map(|p| p.to_path_buf())
                } else {
                    Some(parent.to_path_buf())
                };
                if let Some(root) = disc_root {
                    roots.push(root);
                }
            }
        }
    }
}

/// Return the VTS_*_0.IFO whose title set has the largest total VOB byte size
/// inside `disc_root/VIDEO_TS/`.
///
/// DVD title sets are numbered VTS_01..VTS_NN. IFO-reported duration is
/// unreliable for picking the main feature: on widescreen+fullscreen discs
/// (common on Fox DVD9s) the fullscreen title set can report the correct
/// film duration while referencing the widescreen VOBs via seamless branching,
/// causing the duration heuristic to select the wrong title set. Total VOB
/// size is a more robust signal — the main feature always dominates in bytes.
/// Falls back to alphabetical first if all sizes are zero.
pub(super) fn find_title_ifo(disc_root: &Path) -> Option<PathBuf> {
    let video_ts = disc_root.join("VIDEO_TS");
    // Standard layout: VIDEO_TS/ subfolder exists. Flat layout: IFOs are directly
    // in disc_root (some scene releases skip the VIDEO_TS/ subfolder).
    let search_dir = if video_ts.is_dir() {
        &video_ts
    } else {
        disc_root
    };
    let mut ifos: Vec<PathBuf> = std::fs::read_dir(search_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| {
                    let u = n.to_uppercase();
                    u.starts_with("VTS_") && u.ends_with("_0.IFO")
                })
                .unwrap_or(false)
        })
        .collect();
    ifos.sort();

    // `ifos` is sorted alphabetically; use the index as a tiebreaker so that
    // the alphabetically first title set wins when VOB sizes are equal.
    let best = ifos
        .iter()
        .enumerate()
        .max_by_key(|(i, ifo)| (vob_set_total_size(ifo), usize::MAX - i))
        .map(|(_, p)| p.clone());

    best.or_else(|| ifos.into_iter().next())
}

/// Sum the byte sizes of all VOBs belonging to the same title set as `ifo`.
///
/// `ifo` is expected to be named `VTS_NN_0.IFO`; we glob `VTS_NN_*.VOB` in
/// the same directory to get the complete set including the `_0` menu VOB.
pub(super) fn vob_set_total_size(ifo: &Path) -> u64 {
    let stem = match ifo.file_stem().and_then(|s| s.to_str()) {
        Some(s) => s,
        None => return 0,
    };
    // "VTS_01_0" → prefix "VTS_01_"
    let prefix = match stem.rfind('_') {
        Some(i) => format!("{}_", &stem[..i]),
        None => return 0,
    };
    let dir = match ifo.parent() {
        Some(d) => d,
        None => return 0,
    };
    std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.file_name()
                .to_str()
                .map(|n| {
                    let u = n.to_uppercase();
                    u.starts_with(&prefix.to_uppercase()) && u.ends_with(".VOB")
                })
                .unwrap_or(false)
        })
        .filter_map(|e| e.metadata().ok())
        .map(|m| m.len())
        .sum()
}
