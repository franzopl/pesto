//! NFO file generation.
//!
//! Generates a plain-text `.nfo` summary describing the upload:
//! - Single media file → `mediainfo` output for that file.
//! - Series directory (name contains SXX pattern) → `mediainfo` of first episode.
//! - Generic directory (courses, documents, etc.) → banner + stats + directory tree.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use tracing::{debug, warn};

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "m4v", "mov", "wmv", "flv", "ts", "m2ts", "vob", "divx", "xvid",
];

const MAX_FILENAME_LEN: usize = 42;

/// Generate NFO content for `paths` (the original input paths before any compression).
///
/// Runs `mediainfo` when a media file can be identified; for generic directories
/// produces a banner + statistics + directory tree. Returns `None` when there are
/// no paths.
pub fn generate(paths: &[PathBuf]) -> Option<String> {
    if paths.is_empty() {
        debug!("nfo::generate called with no paths — skipping");
        return None;
    }

    debug!(paths = paths.len(), "generating NFO");

    // Single file: mediainfo if video, plain listing otherwise.
    if paths.len() == 1 && paths[0].is_file() {
        if is_video(&paths[0]) {
            debug!(path = %paths[0].display(), "running mediainfo on single video file");
            match run_mediainfo(&paths[0]) {
                Ok(out) => return Some(out),
                Err(e) => {
                    warn!(path = %paths[0].display(), error = %e, "mediainfo failed; falling back to listing")
                }
            }
        }
        return Some(build_listing(paths));
    }

    // Directory: check if series → mediainfo; otherwise → rich tree.
    if paths.len() == 1 && paths[0].is_dir() {
        let dir = &paths[0];
        let folder_name = dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        // Blu-ray disc structure: BDMV/index.bdmv
        let bd_roots = find_bluray_disc_roots(dir);
        if !bd_roots.is_empty() {
            debug!(discs = bd_roots.len(), folder = %folder_name, "detected Blu-ray structure");
            let mut sections: Vec<String> = Vec::new();
            for root in &bd_roots {
                let disc_label = root
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.display().to_string());
                let bdinfo_result = run_bdinfo(root);
                if bdinfo_result.is_none() {
                    eprintln!(
                        "warning: bdinfo not found — Blu-ray NFO generated via mediainfo may \
                         have incorrect playlist selection or missing language tags.\n\
                         Install one of: go-bdinfo (https://github.com/autobrr/go-bdinfo) \
                         or BDInfoCLI-ng (https://github.com/tetrahydroc/BDInfoCLI)"
                    );
                }
                let mi = if let Some(bdinfo_out) = bdinfo_result {
                    debug!(root = %root.display(), "bdinfo succeeded for Blu-ray disc");
                    bdinfo_out
                } else {
                    match find_main_mpls(root).or_else(|| find_main_m2ts(root)) {
                        Some(media) => {
                            debug!(media = %media.display(), "running mediainfo on Blu-ray main feature");
                            let mi = match run_mediainfo(&media) {
                                Ok(out) => out,
                                Err(e) => {
                                    warn!(media = %media.display(), error = %e, "mediainfo failed for Blu-ray main feature");
                                    format!("[mediainfo failed for {}: {}]", media.display(), e)
                                }
                            };
                            let is_mpls = media
                                .extension()
                                .and_then(|e| e.to_str())
                                .map(|e| e.eq_ignore_ascii_case("mpls"))
                                .unwrap_or(false);
                            if is_mpls {
                                let lang_map = mpls_language_map(&media);
                                debug!(pid_count = lang_map.len(), "parsed MPLS language map");
                                inject_language_tags(&mi, &lang_map)
                            } else {
                                mi
                            }
                        }
                        None => {
                            warn!(root = %root.display(), "no MPLS or M2TS found in BDMV");
                            "[no playable stream found]\n".to_owned()
                        }
                    }
                };
                sections.push(format!("=== Blu-ray Disc: {disc_label} ===\n{mi}\n"));
            }
            return Some(sections.join("\n"));
        }

        // DVD disc structure: VIDEO_TS/ with IFO files.
        let disc_roots = find_dvd_disc_roots(dir);
        if !disc_roots.is_empty() {
            debug!(discs = disc_roots.len(), folder = %folder_name, "detected DVD structure");
            let mut sections: Vec<String> = Vec::new();
            for root in &disc_roots {
                let title_ifo = find_title_ifo(root)
                    .unwrap_or_else(|| root.join("VIDEO_TS").join("VTS_01_0.IFO"));
                debug!(ifo = %title_ifo.display(), "running mediainfo on title IFO");
                let mi = match run_mediainfo(&title_ifo) {
                    Ok(out) => out,
                    Err(e) => {
                        warn!(ifo = %title_ifo.display(), error = %e, "mediainfo failed for DVD IFO");
                        format!("[mediainfo failed for {}: {}]", title_ifo.display(), e)
                    }
                };
                let disc_label = root
                    .file_name()
                    .map(|s| s.to_string_lossy().into_owned())
                    .unwrap_or_else(|| root.display().to_string());
                sections.push(format!("=== DVD Disc: {disc_label} ===\n{mi}\n"));
            }
            return Some(sections.join("\n"));
        }

        if is_series_folder(&folder_name) {
            debug!(folder = %folder_name, "detected series folder — looking for first video");
            if let Some(first_ep) = find_first_video(dir) {
                debug!(episode = %first_ep.display(), "running mediainfo on first episode");
                match run_mediainfo(&first_ep) {
                    Ok(out) => return Some(out),
                    Err(e) => {
                        warn!(episode = %first_ep.display(), error = %e, "mediainfo failed; falling back to folder NFO")
                    }
                }
            } else {
                debug!("no video file found in series folder; using folder NFO");
            }
        }

        return Some(build_folder_nfo(dir));
    }

    // Multiple paths: fall back to plain listing.
    debug!("multiple paths — using plain listing");
    Some(build_listing(paths))
}

/// Generate NFO content for a consolidated season (multiple source directories).
///
/// Finds the alphabetically first video file across all `dirs`, runs `mediainfo`
/// on it, and returns the output. Falls back to `generate(dirs)` when no video
/// is found or `mediainfo` fails.
pub fn generate_season(dirs: &[PathBuf]) -> Option<String> {
    if dirs.is_empty() {
        debug!("nfo::generate_season called with no dirs — skipping");
        return None;
    }

    debug!(dirs = dirs.len(), "generating season NFO");

    // Collect all directories, sorted, so episode order is stable.
    let mut sorted_dirs: Vec<&PathBuf> = dirs.iter().collect();
    sorted_dirs.sort();
    for dir in sorted_dirs {
        let first = if dir.is_dir() {
            find_first_video(dir)
        } else if is_video(dir) {
            Some(dir.clone())
        } else {
            None
        };
        if let Some(video) = first {
            debug!(video = %video.display(), "running mediainfo for season NFO");
            match run_mediainfo(&video) {
                Ok(out) => return Some(out),
                Err(e) => {
                    warn!(video = %video.display(), error = %e, "mediainfo failed for season entry")
                }
            }
        }
    }
    // Fallback: plain listing.
    debug!("mediainfo unavailable for all season entries; falling back to listing");
    generate(dirs)
}

/// Write the NFO content to `path`, creating or overwriting it.
pub fn write(path: &Path, content: &str) -> std::io::Result<()> {
    debug!(path = %path.display(), bytes = content.len(), "writing NFO");
    std::fs::write(path, content.as_bytes())
}

// ── helpers ──────────────────────────────────────────────────────────────────

fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| VIDEO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Detect series directories by the SXX or SXXEXX pattern in the folder name.
fn is_series_folder(name: &str) -> bool {
    // Matches S01, S01E01, s02, etc. not preceded by a letter.
    let upper = name.to_uppercase();
    let bytes = upper.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'S' {
            let prev_is_letter = i > 0 && bytes[i - 1].is_ascii_alphabetic();
            if prev_is_letter {
                continue;
            }
            // expect at least two digits after S
            let rest = &upper[i + 1..];
            let digits: usize = rest.chars().take_while(|c| c.is_ascii_digit()).count();
            if digits >= 2 {
                return true;
            }
        }
    }
    false
}

/// Return the alphabetically first video file inside `dir`, recursing into sub-dirs.
fn find_first_video(dir: &Path) -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    collect_videos(dir, &mut candidates);
    candidates.sort();
    candidates.into_iter().next()
}

fn collect_videos(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_videos(&child, out);
        } else if is_video(&child) {
            out.push(child);
        }
    }
}

/// Find Blu-ray disc roots by locating BDMV/index.bdmv anywhere under `path`.
fn find_bluray_disc_roots(path: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    collect_bluray_roots(path, &mut roots);
    roots.sort();
    roots.dedup();
    roots
}

fn collect_bluray_roots(dir: &Path, roots: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.filter_map(|e| e.ok()) {
        let child = entry.path();
        if child.is_dir() {
            // Skip BDMV/BACKUP/ — the Blu-ray spec mandates a backup of BDMV
            // metadata there, including a duplicate index.bdmv, but it has no
            // STREAM/ folder so it would produce a phantom disc section.
            let is_backup = child
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.eq_ignore_ascii_case("BACKUP"))
                .unwrap_or(false);
            let parent_is_bdmv = dir
                .file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.eq_ignore_ascii_case("BDMV"))
                .unwrap_or(false);
            if !(is_backup && parent_is_bdmv) {
                collect_bluray_roots(&child, roots);
            }
        } else if child
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("index.bdmv"))
            .unwrap_or(false)
        {
            // index.bdmv -> BDMV/ -> disc root
            if let Some(bdmv) = child.parent() {
                if let Some(disc_root) = bdmv.parent() {
                    roots.push(disc_root.to_path_buf());
                }
            }
        }
    }
}

/// Return the `.mpls` with the longest duration inside `disc_root/BDMV/PLAYLIST/`.
///
/// Language tags for audio and subtitle streams live in the playlist, not the
/// raw `.m2ts` clip. `mediainfo` parses `.mpls` natively and joins language
/// info from the playlist to codec info from the referenced clip.
///
/// Duration is the reliable heuristic — the main feature is always the longest
/// title. File size is used only as a fallback when `mediainfo` is unavailable.
fn find_main_mpls(disc_root: &Path) -> Option<PathBuf> {
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
            let ms = mediainfo_duration_ms(p)?;
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
fn mpls_language_map(path: &Path) -> HashMap<u16, String> {
    std::fs::read(path)
        .ok()
        .and_then(|data| parse_mpls(&data))
        .unwrap_or_default()
}

/// Top-level MPLS parser. Returns `None` only on structural failures (bad
/// magic, truncation); an empty map is returned when no language tags are found.
fn parse_mpls(data: &[u8]) -> Option<HashMap<u16, String>> {
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
fn parse_stn_table(data: &[u8], offset: usize, map: &mut HashMap<u16, String>) {
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
fn iso639_2_to_name(code: &str) -> String {
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
fn inject_language_tags(output: &str, lang_map: &HashMap<u16, String>) -> String {
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

fn mpls_u16(data: &[u8], offset: usize) -> Option<u16> {
    data.get(offset..offset + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_be_bytes)
}

fn mpls_u32(data: &[u8], offset: usize) -> Option<u32> {
    data.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_be_bytes)
}

// ── end MPLS parser ──────────────────────────────────────────────────────────

/// Return the largest `.m2ts` file inside `disc_root/BDMV/STREAM/`.
///
/// The largest file is the main feature; extras and menus are much smaller.
fn find_main_m2ts(disc_root: &Path) -> Option<PathBuf> {
    let stream = disc_root.join("BDMV").join("STREAM");
    std::fs::read_dir(&stream)
        .ok()?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("m2ts"))
                .unwrap_or(false)
        })
        .max_by_key(|p| p.metadata().map(|m| m.len()).unwrap_or(0))
}

/// Find DVD disc roots by locating VIDEO_TS/VIDEO_TS.VOB anywhere under `path`.
fn find_dvd_disc_roots(path: &Path) -> Vec<PathBuf> {
    let mut roots: Vec<PathBuf> = Vec::new();
    collect_dvd_roots(path, &mut roots);
    roots.sort();
    roots.dedup();
    roots
}

fn collect_dvd_roots(dir: &Path, roots: &mut Vec<PathBuf>) {
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
            .map(|n| n.eq_ignore_ascii_case("VIDEO_TS.VOB"))
            .unwrap_or(false)
        {
            // VIDEO_TS.VOB -> VIDEO_TS/ -> disc root
            if let Some(video_ts) = child.parent() {
                if let Some(disc_root) = video_ts.parent() {
                    roots.push(disc_root.to_path_buf());
                }
            }
        }
    }
}

/// Return the VTS_*_0.IFO with the longest duration inside `disc_root/VIDEO_TS/`.
///
/// DVD title sets are numbered VTS_01..VTS_NN; the main feature is not always
/// VTS_01 — on multi-angle or bonus-heavy discs the title with the longest
/// duration is the actual feature. We query `mediainfo` with a minimal template
/// to get each IFO's duration in milliseconds, then return the longest one.
/// Falls back to alphabetical first if mediainfo is unavailable.
fn find_title_ifo(disc_root: &Path) -> Option<PathBuf> {
    let video_ts = disc_root.join("VIDEO_TS");
    let mut ifos: Vec<PathBuf> = std::fs::read_dir(&video_ts)
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

    // Try to pick the IFO with the longest duration via a lightweight mediainfo query.
    let best = ifos
        .iter()
        .filter_map(|p| {
            let ms = mediainfo_duration_ms(p)?;
            Some((ms, p.clone()))
        })
        .max_by_key(|(ms, _)| *ms)
        .map(|(_, p)| p);

    best.or_else(|| ifos.into_iter().next())
}

/// Run `mediainfo` with a minimal General template to obtain the duration in ms.
/// Returns `None` if mediainfo is not available or the output cannot be parsed.
fn mediainfo_duration_ms(path: &Path) -> Option<u64> {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let output = std::process::Command::new("mediainfo")
        .arg("--Output=General;%Duration%")
        .arg(&abs)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .ok()
}

fn run_mediainfo(path: &Path) -> std::io::Result<String> {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let output = std::process::Command::new("mediainfo")
        .arg(&abs)
        .output()
        .map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("could not launch mediainfo (is it installed and in PATH?): {e}"),
            )
        })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let msg = if stderr.trim().is_empty() {
            format!("mediainfo exited with status {}", output.status)
        } else {
            format!(
                "mediainfo exited with status {}: {}",
                output.status,
                stderr.trim()
            )
        };
        return Err(std::io::Error::other(msg));
    }
    let raw = String::from_utf8_lossy(&output.stdout);
    // Replace any occurrence of the full filesystem path with just the
    // basename, hiding the local directory. On Windows, canonicalize() adds a
    // \\?\ prefix that mediainfo echoes back, so we replace both forms.
    let filename = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();
    let canonical_str = abs.to_string_lossy();
    let original_str = path.to_string_lossy();
    let replaced = raw.replace(canonical_str.as_ref(), &filename);
    let replaced = replaced.replace(original_str.as_ref(), &filename);
    Ok(replaced)
}

/// Run bdinfo and return the QUICK SUMMARY text. Tries two implementations:
///
/// 1. `bdinfo --stdout --main --summaryonly` (go-bdinfo / autobrr fork) — stdout output.
/// 2. `BDInfo -w <disc> <tmpdir>` (tetrahydroc/BDInfoCLI-ng, .NET 8) — writes
///    `BDINFO.<label>.txt` to a temp directory; we read the QUICK SUMMARY section.
///
/// Returns None if neither tool is available or both fail.
fn run_bdinfo(disc_root: &Path) -> Option<String> {
    // --- attempt 1: BDInfoCLI-ng (.NET, file output) ---
    if let Some(result) = run_bdinfocli_ng(disc_root) {
        return Some(result);
    }

    // --- attempt 2: go-bdinfo (stdout) ---
    run_go_bdinfo(disc_root)
}

fn run_go_bdinfo(disc_root: &Path) -> Option<String> {
    let output = std::process::Command::new("bdinfo")
        .arg("--stdout")
        .arg("--main")
        .arg("--summaryonly")
        .arg(disc_root)
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    extract_quick_summary_from_str(&String::from_utf8_lossy(&output.stdout))
}

fn run_bdinfocli_ng(disc_root: &Path) -> Option<String> {
    let tmp = std::env::temp_dir().join(format!(
        "pesto-bdinfo-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&tmp).ok()?;

    let status = std::process::Command::new("BDInfo")
        .arg("-w")
        .arg(disc_root)
        .arg(&tmp)
        .status()
        .ok()?;

    let result = if status.success() {
        std::fs::read_dir(&tmp)
            .ok()?
            .filter_map(|e| e.ok())
            .find(|e| {
                e.file_name()
                    .to_string_lossy()
                    .to_uppercase()
                    .starts_with("BDINFO")
                    && e.path().extension().map_or(false, |x| x == "txt")
            })
            .and_then(|e| std::fs::read_to_string(e.path()).ok())
            .as_deref()
            .and_then(extract_quick_summary_from_str)
    } else {
        None
    };

    let _ = std::fs::remove_dir_all(&tmp);
    result
}

/// Extract the QUICK SUMMARY block from a BDInfo report string (stdout or file).
fn extract_quick_summary_from_str(raw: &str) -> Option<String> {
    // Find the line after "QUICK SUMMARY:" and collect until the next section
    // header or end of input.
    let after = raw
        .lines()
        .skip_while(|l| l.trim() != "QUICK SUMMARY:")
        .skip(1) // skip the header itself
        .skip_while(|l| l.trim().is_empty())
        .take_while(|l| !l.starts_with('<') && l.trim() != "[/code]")
        .collect::<Vec<_>>()
        .join("\n");

    let trimmed = after.trim_end().to_owned();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn format_size(bytes: u64) -> String {
    let mut val = bytes as f64;
    for unit in &["B", "KB", "MB", "GB"] {
        if val < 1024.0 {
            if *unit == "B" {
                return format!("{} B", bytes);
            }
            return format!("{val:.2} {unit}");
        }
        val /= 1024.0;
    }
    format!("{val:.2} TB")
}

fn center(text: &str, width: usize) -> String {
    if text.len() >= width {
        return text.to_string();
    }
    let pad = (width - text.len()) / 2;
    format!("{:pad$}{}{:pad$}", "", text, "")
}

fn default_banner() -> &'static str {
    ".------------------------------------------------------------------------------.\n\
     |                                                                              |\n\
     |    ____  _____ ____ _____ ___                                               |\n\
     |   |  _ \\| ____/ ___|_   _/ _ \\                                             |\n\
     |   | |_) |  _| \\___ \\ | || | | |                                            |\n\
     |   |  __/| |___ ___) || || |_| |                                            |\n\
     |   |_|   |_____|____/ |_| \\___/                                             |\n\
     |                                                                              |\n\
     |                     usenet poster                                            |\n\
     |                                                                              |\n\
     '------------------------------------------------------------------------------'"
}

/// Collect all files under `dir` (skipping `.nfo` with the same base name).
fn collect_all_files(dir: &Path, nfo_name: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    collect_files_recursive(dir, nfo_name, &mut out);
    out
}

fn collect_files_recursive(dir: &Path, nfo_name: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    children.sort();
    for child in children {
        if child.is_dir() {
            collect_files_recursive(&child, nfo_name, out);
        } else {
            let fname = child
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default();
            if fname != nfo_name {
                out.push(child);
            }
        }
    }
}

struct TreeState {
    lines: Vec<String>,
    file_count: usize,
    dir_count: usize,
}

fn build_tree(dir: &Path, nfo_name: &str, file_sizes: &HashMap<PathBuf, u64>) -> TreeState {
    let mut state = TreeState {
        lines: Vec::new(),
        file_count: 0,
        dir_count: 0,
    };
    let root_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string_lossy().into_owned());
    state.lines.push(root_name);
    walk_tree(dir, "", nfo_name, file_sizes, &mut state);
    state
}

fn walk_tree(
    current_dir: &Path,
    prefix: &str,
    nfo_name: &str,
    file_sizes: &HashMap<PathBuf, u64>,
    state: &mut TreeState,
) {
    let Ok(entries) = std::fs::read_dir(current_dir) else {
        return;
    };
    let mut contents: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .map(|n| n.to_string_lossy() != nfo_name)
                .unwrap_or(true)
        })
        .collect();
    contents.sort_by(|a, b| {
        a.file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_lowercase()
            .cmp(
                &b.file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .to_lowercase(),
            )
    });

    let total = contents.len();
    for (i, path) in contents.iter().enumerate() {
        let is_last = i == total - 1;
        let pointer = if is_last { "`-- " } else { "|-- " };
        let new_prefix = format!("{}{}", prefix, if is_last { "    " } else { "|   " });
        let item_name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        if path.is_dir() {
            state
                .lines
                .push(format!("{}{}{}", prefix, pointer, item_name));
            state.dir_count += 1;
            walk_tree(path, &new_prefix, nfo_name, file_sizes, state);
        } else {
            state.file_count += 1;
            let display_name = if item_name.len() > MAX_FILENAME_LEN {
                format!("{}...", &item_name[..MAX_FILENAME_LEN])
            } else {
                item_name.clone()
            };
            let canonical = path.canonicalize().unwrap_or_else(|_| path.clone());
            let size = file_sizes.get(&canonical).copied().unwrap_or(0);
            let size_str = format_size(size);
            state.lines.push(format!(
                "{}{}{} [{}]",
                prefix, pointer, display_name, size_str
            ));
        }
    }
}

/// Build a rich NFO for a generic directory (banner + stats + tree).
fn build_folder_nfo(dir: &Path) -> String {
    let folder_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| dir.to_string_lossy().into_owned());
    let nfo_name = format!("{folder_name}.nfo");

    let all_files = collect_all_files(dir, &nfo_name);

    let mut file_sizes: HashMap<PathBuf, u64> = HashMap::new();
    let mut total_size: u64 = 0;
    for f in &all_files {
        let size = f.metadata().map(|m| m.len()).unwrap_or(0);
        let canonical = f.canonicalize().unwrap_or_else(|_| f.clone());
        file_sizes.insert(canonical, size);
        total_size += size;
    }

    let tree = build_tree(dir, &nfo_name, &file_sizes);

    let mut ext_counts: HashMap<String, usize> = HashMap::new();
    for f in &all_files {
        let ext = f
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy().to_lowercase()))
            .unwrap_or_else(|| ".".to_string());
        *ext_counts.entry(ext).or_insert(0) += 1;
    }

    let mut lines: Vec<String> = Vec::new();

    for l in default_banner().lines() {
        lines.push(l.to_string());
    }
    lines.push(String::new());

    let title = folder_name.to_uppercase();
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(format!("|{}|", center(&title, 78)));
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(String::new());
    lines.push("-".repeat(80));
    lines.push(String::new());

    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(format!("|{}|", center("*** GENERAL STATISTICS ***", 78)));
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(String::new());
    lines.push(format!(
        "  > Total Size:         {}",
        format_size(total_size)
    ));
    lines.push(format!("  > Directories:        {}", tree.dir_count));
    lines.push(format!("  > Total Files:        {}", tree.file_count));
    lines.push("  > Files by Type:".to_string());

    let mut ext_vec: Vec<(String, usize)> = ext_counts.into_iter().collect();
    ext_vec.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (ext, count) in &ext_vec {
        let label = ext.trim_start_matches('.').to_uppercase();
        let label = if label.is_empty() { "NO EXT" } else { &label };
        lines.push(format!("    - {label}: {count} file(s)"));
    }

    lines.push(String::new());
    lines.push(String::new());
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(format!(
        "|{}|",
        center("*** FILE AND DIRECTORY STRUCTURE ***", 78)
    ));
    lines.push(format!("+{}+", "-".repeat(78)));
    lines.push(String::new());
    lines.extend(tree.lines);
    lines.push(String::new());
    lines.push(format!(
        "{} directories, {} files, {}",
        tree.dir_count,
        tree.file_count,
        format_size(total_size)
    ));

    lines.join("\n")
}

/// Build a human-readable recursive listing of all paths (fallback for multiple paths).
fn build_listing(paths: &[PathBuf]) -> String {
    let mut buf = String::new();
    for root in paths {
        let name = root.file_name().unwrap_or(root.as_os_str());
        if root.is_file() {
            let size = root.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = writeln!(buf, "{} ({})", name.to_string_lossy(), format_size(size));
        } else if root.is_dir() {
            let _ = writeln!(buf, "{}/", name.to_string_lossy());
            append_dir_listing(root, &mut buf, 1);
        }
    }
    buf
}

fn append_dir_listing(dir: &Path, buf: &mut String, depth: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let indent = "  ".repeat(depth);
    let mut children: Vec<PathBuf> = entries.filter_map(|e| e.ok()).map(|e| e.path()).collect();
    children.sort();
    for child in children {
        let name = child.file_name().unwrap_or(child.as_os_str());
        if child.is_dir() {
            let _ = writeln!(buf, "{}{}/", indent, name.to_string_lossy());
            append_dir_listing(&child, buf, depth + 1);
        } else {
            let size = child.metadata().map(|m| m.len()).unwrap_or(0);
            let _ = writeln!(
                buf,
                "{}{}  ({})",
                indent,
                name.to_string_lossy(),
                format_size(size)
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // ── is_video ─────────────────────────────────────────────────────────────

    #[test]
    fn is_video_known_extensions() {
        for ext in &["mkv", "mp4", "avi", "ts", "m2ts", "mov"] {
            let p = PathBuf::from(format!("file.{ext}"));
            assert!(is_video(&p), "{ext} should be recognised as video");
        }
    }

    #[test]
    fn is_video_unknown_extension() {
        assert!(!is_video(&PathBuf::from("file.txt")));
        assert!(!is_video(&PathBuf::from("file.nfo")));
        assert!(!is_video(&PathBuf::from("file.nzb")));
    }

    #[test]
    fn is_video_no_extension() {
        assert!(!is_video(&PathBuf::from("README")));
    }

    #[test]
    fn is_video_mixed_case() {
        assert!(is_video(&PathBuf::from("movie.MKV")));
        assert!(is_video(&PathBuf::from("clip.Mp4")));
    }

    // ── is_series_folder ─────────────────────────────────────────────────────

    #[test]
    fn series_folder_detection() {
        assert!(is_series_folder("Breaking.Bad.S01E01.mkv"));
        assert!(is_series_folder("Show.S02"));
        assert!(is_series_folder("My Series S03E05 720p"));
        assert!(!is_series_folder("Curso Python Avancado"));
        assert!(!is_series_folder("Documentary.2024"));
        // "AS01" should not match — 'A' is an alpha prefix
        assert!(!is_series_folder("AS01.mkv"));
    }

    // ── build_folder_nfo ─────────────────────────────────────────────────────

    #[test]
    fn folder_nfo_contains_stats_and_tree() {
        let dir = TempDir::new().unwrap();
        let sub = dir.path().join("module1");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("lesson.pdf"), b"pdf content").unwrap();
        fs::write(dir.path().join("readme.txt"), b"hello").unwrap();

        let nfo = build_folder_nfo(dir.path());
        assert!(nfo.contains("GENERAL STATISTICS"));
        assert!(nfo.contains("FILE AND DIRECTORY STRUCTURE"));
        assert!(nfo.contains("lesson.pdf"));
        assert!(nfo.contains("readme.txt"));
        assert!(nfo.contains("|--") || nfo.contains("`--"));
    }

    #[test]
    fn folder_nfo_shows_formatted_sizes() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("file.txt"), vec![0u8; 2048]).unwrap();

        let nfo = build_folder_nfo(dir.path());
        assert!(nfo.contains("KB"));
    }

    // ── generate ─────────────────────────────────────────────────────────────

    #[test]
    fn generate_returns_none_for_empty_paths() {
        assert!(generate(&[]).is_none());
    }

    #[test]
    fn generate_falls_back_to_listing_for_non_video() {
        let dir = TempDir::new().unwrap();
        let f = dir.path().join("data.nzb");
        fs::write(&f, b"content").unwrap();

        let result = generate(&[f]);
        assert!(result.is_some());
        let listing = result.unwrap();
        assert!(listing.contains("data.nzb"));
    }

    #[test]
    fn generate_generic_dir_produces_rich_nfo() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("notes.txt"), b"study notes").unwrap();
        fs::write(dir.path().join("slides.pdf"), b"slides").unwrap();

        let result = generate(&[dir.path().to_path_buf()]);
        assert!(result.is_some());
        let nfo = result.unwrap();
        assert!(nfo.contains("GENERAL STATISTICS"));
        assert!(nfo.contains("notes.txt"));
    }

    // ── Blu-ray detection ────────────────────────────────────────────────────

    fn make_bluray_structure(base: &Path) {
        let bdmv = base.join("BDMV");
        let stream = bdmv.join("STREAM");
        let backup = bdmv.join("BACKUP");
        fs::create_dir_all(&stream).unwrap();
        fs::create_dir_all(&backup).unwrap();
        fs::write(bdmv.join("index.bdmv"), b"").unwrap();
        fs::write(bdmv.join("MovieObject.bdmv"), b"").unwrap();
        // Real Blu-rays carry a duplicate index.bdmv in BACKUP/ — must not be
        // treated as a second disc root.
        fs::write(backup.join("index.bdmv"), b"").unwrap();
        // Main feature (large) and a short extra (small).
        fs::write(stream.join("00001.m2ts"), vec![0u8; 8000]).unwrap();
        fs::write(stream.join("00002.m2ts"), vec![0u8; 100]).unwrap();
    }

    #[test]
    fn find_bluray_disc_roots_detects_index_bdmv() {
        let dir = TempDir::new().unwrap();
        make_bluray_structure(dir.path());

        let roots = find_bluray_disc_roots(dir.path());
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], dir.path());
    }

    #[test]
    fn find_bluray_disc_roots_empty_for_non_bluray() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("movie.mkv"), b"").unwrap();

        let roots = find_bluray_disc_roots(dir.path());
        assert!(roots.is_empty());
    }

    #[test]
    fn find_main_m2ts_picks_largest_file() {
        let dir = TempDir::new().unwrap();
        make_bluray_structure(dir.path());

        let m2ts = find_main_m2ts(dir.path()).unwrap();
        assert_eq!(m2ts.file_name().unwrap(), "00001.m2ts");
    }

    #[test]
    fn find_bluray_disc_roots_ignores_backup_index_bdmv() {
        let dir = TempDir::new().unwrap();
        make_bluray_structure(dir.path()); // now includes BACKUP/index.bdmv

        let roots = find_bluray_disc_roots(dir.path());
        assert_eq!(roots.len(), 1, "BACKUP/index.bdmv must not be a disc root");
        assert_eq!(roots[0], dir.path());
    }

    #[test]
    fn find_main_mpls_picks_largest_playlist() {
        let dir = TempDir::new().unwrap();
        make_bluray_structure(dir.path());
        let playlist = dir.path().join("BDMV").join("PLAYLIST");
        fs::create_dir_all(&playlist).unwrap();
        fs::write(playlist.join("00001.mpls"), vec![0u8; 8000]).unwrap();
        fs::write(playlist.join("00002.mpls"), vec![0u8; 100]).unwrap();

        let mpls = find_main_mpls(dir.path()).unwrap();
        assert_eq!(mpls.file_name().unwrap(), "00001.mpls");
    }

    #[test]
    fn find_main_mpls_returns_none_without_playlist_dir() {
        let dir = TempDir::new().unwrap();
        make_bluray_structure(dir.path()); // no PLAYLIST/ created

        assert!(find_main_mpls(dir.path()).is_none());
    }

    #[test]
    fn generate_bluray_does_not_call_folder_nfo() {
        let dir = TempDir::new().unwrap();
        make_bluray_structure(dir.path());

        let result = generate(&[dir.path().to_path_buf()]);
        assert!(result.is_some());
        let nfo = result.unwrap();
        assert!(nfo.contains("=== Blu-ray Disc:"));
        assert!(!nfo.contains("GENERAL STATISTICS"));
    }

    #[test]
    fn bluray_detection_does_not_trigger_for_dvd() {
        let dir = TempDir::new().unwrap();
        make_dvd_structure(dir.path());

        let bd_roots = find_bluray_disc_roots(dir.path());
        assert!(bd_roots.is_empty());
    }

    #[test]
    fn dvd_detection_does_not_trigger_for_bluray() {
        let dir = TempDir::new().unwrap();
        make_bluray_structure(dir.path());

        let dvd_roots = find_dvd_disc_roots(dir.path());
        assert!(dvd_roots.is_empty());
    }

    // ── DVD detection ─────────────────────────────────────────────────────────

    fn make_dvd_structure(base: &Path) {
        let vts = base.join("VIDEO_TS");
        fs::create_dir_all(&vts).unwrap();
        fs::write(vts.join("VIDEO_TS.IFO"), b"").unwrap();
        fs::write(vts.join("VIDEO_TS.BUP"), b"").unwrap();
        fs::write(vts.join("VIDEO_TS.VOB"), b"").unwrap();
        fs::write(vts.join("VTS_01_0.IFO"), b"").unwrap();
        fs::write(vts.join("VTS_01_0.BUP"), b"").unwrap();
        fs::write(vts.join("VTS_01_1.VOB"), b"").unwrap();
    }

    #[test]
    fn find_dvd_disc_roots_detects_video_ts_vob() {
        let dir = TempDir::new().unwrap();
        make_dvd_structure(dir.path());

        let roots = find_dvd_disc_roots(dir.path());
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0], dir.path());
    }

    #[test]
    fn find_dvd_disc_roots_empty_for_non_dvd() {
        let dir = TempDir::new().unwrap();
        fs::write(dir.path().join("movie.mkv"), b"").unwrap();

        let roots = find_dvd_disc_roots(dir.path());
        assert!(roots.is_empty());
    }

    #[test]
    fn find_title_ifo_falls_back_to_alphabetical_without_mediainfo() {
        // Without a real mediainfo binary producing parseable durations,
        // find_title_ifo falls back to the alphabetically first VTS_*_0.IFO.
        let dir = TempDir::new().unwrap();
        make_dvd_structure(dir.path());
        fs::write(dir.path().join("VIDEO_TS").join("VTS_02_0.IFO"), b"").unwrap();

        let ifo = find_title_ifo(dir.path()).unwrap();
        assert_eq!(ifo.file_name().unwrap(), "VTS_01_0.IFO");
    }

    #[test]
    fn generate_dvd_does_not_call_folder_nfo() {
        // With mediainfo absent, generate() should still return Some with a
        // "[mediainfo failed …]" message rather than falling through to the
        // generic folder NFO (which would contain "GENERAL STATISTICS").
        let dir = TempDir::new().unwrap();
        make_dvd_structure(dir.path());

        let result = generate(&[dir.path().to_path_buf()]);
        assert!(result.is_some());
        let nfo = result.unwrap();
        // Must mention the DVD disc header.
        assert!(nfo.contains("=== DVD Disc:"));
        // Must NOT contain the generic folder NFO banner.
        assert!(!nfo.contains("GENERAL STATISTICS"));
    }

    // Run with: cargo test -- --ignored mpls_lang_map_allquiet
    #[test]
    #[ignore]
    fn mpls_lang_map_allquiet() {
        let mpls = std::path::PathBuf::from(
            "/media/ironwolf/downloads/radarr4k/All.Quiet.On.The.Western.Front.2022.2160p.EUR.UHD.BluRay.HDR.HEVC.TrueHD.7.1.Atmos-PEBBLES104/BDMV/PLAYLIST/00000.mpls",
        );
        let map = mpls_language_map(&mpls);
        println!("map size: {}", map.len());
        let mut pairs: Vec<_> = map.iter().collect();
        pairs.sort_by_key(|(pid, _)| **pid);
        for (pid, lang) in &pairs {
            println!("  {pid:#06x} → {lang}");
        }
        assert!(map.contains_key(&0x12a0), "subtitle PID 0x12a0 missing");
        assert_eq!(map[&0x12a0], "German");
    }

    // Run with: cargo test -- --ignored nfo_topgun_real_disc
    #[test]
    #[ignore]
    fn nfo_topgun_real_disc() {
        let path = std::path::PathBuf::from(
            "/media/ironwolf/downloads/radarr/Top Gun Maverick 2022 1080p EUR Blu-ray AVC TrueHD 7.1-ESiR",
        );
        let nfo = generate(&[path]).expect("generate returned None");
        println!("{nfo}");
        assert_eq!(
            nfo.matches("=== Blu-ray Disc:").count(),
            1,
            "expected exactly one disc section:\n{nfo}"
        );
        assert!(!nfo.contains("[no playable stream found]"));
        // bdinfo should pick 00001.MPLS (main feature, 2h10min), not 00003.MPLS (looping playlist)
        assert!(
            nfo.contains("00001.MPLS"),
            "expected main playlist 00001.MPLS in NFO:\n{nfo}"
        );
    }

    // Run with: cargo test -- --ignored nfo_goodbadugly_real_disc
    #[test]
    #[ignore]
    fn nfo_goodbadugly_real_disc() {
        let path = std::path::PathBuf::from(
            "/media/ironwolf/downloads/cross-seeds/links/DigitalCore/The Good, the Bad and the Ugly 1966 Extended Cut 1080p EUR Blu-ray AVC DTS-HD MA 5.1",
        );
        let nfo = generate(&[path]).expect("generate returned None");
        println!("{nfo}");
        assert_eq!(
            nfo.matches("=== Blu-ray Disc:").count(),
            1,
            "expected exactly one disc section:\n{nfo}"
        );
        assert!(!nfo.contains("[no playable stream found]"));
    }

    // Run with: cargo test -- --ignored nfo_allquiet_real_disc
    #[test]
    #[ignore]
    fn nfo_allquiet_real_disc() {
        let path = std::path::PathBuf::from(
            "/media/ironwolf/downloads/radarr4k/All.Quiet.On.The.Western.Front.2022.2160p.EUR.UHD.BluRay.HDR.HEVC.TrueHD.7.1.Atmos-PEBBLES104",
        );
        let nfo = generate(&[path]).expect("generate returned None");
        println!("{nfo}");
        assert_eq!(
            nfo.matches("=== Blu-ray Disc:").count(),
            1,
            "expected exactly one disc section:\n{nfo}"
        );
        assert!(
            !nfo.contains("[no playable stream found]"),
            "main MPLS/M2TS not found:\n{nfo}"
        );
    }

    // Run with: cargo test -- --ignored nfo_tron_real_disc
    #[test]
    #[ignore]
    fn nfo_tron_real_disc() {
        let path = std::path::PathBuf::from(
            "/media/ironwolf/downloads/radarr/Tron.1982.REMASTERED.COMPLETE.BLURAY-INCUBO",
        );
        let nfo = generate(&[path]).expect("generate returned None");
        println!("{nfo}");
        // Must produce exactly one disc section (BACKUP/ must not create a phantom).
        assert_eq!(
            nfo.matches("=== Blu-ray Disc:").count(),
            1,
            "expected exactly one disc section:\n{nfo}"
        );
        assert!(
            !nfo.contains("[no playable stream found]"),
            "main MPLS/M2TS not found:\n{nfo}"
        );
    }

    #[test]
    fn find_media_file_returns_alphabetically_first() {
        let dir = TempDir::new().unwrap();
        let a = dir.path().join("ep02.mkv");
        let b = dir.path().join("ep01.mkv");
        fs::write(&a, b"").unwrap();
        fs::write(&b, b"").unwrap();

        let result = find_first_video(dir.path());
        assert_eq!(result.unwrap().file_name().unwrap(), "ep01.mkv");
    }
}
