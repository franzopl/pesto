use super::*;

use std::fs;
use tempfile::TempDir;

// ── truncate_filename ────────────────────────────────────────────────────

#[test]
fn truncate_filename_keeps_short_names() {
    assert_eq!(
        truncate_filename("short.mp4", MAX_FILENAME_LEN),
        "short.mp4"
    );
}

#[test]
fn truncate_filename_ascii_over_limit() {
    let name = "a".repeat(MAX_FILENAME_LEN + 5);
    let out = truncate_filename(&name, MAX_FILENAME_LEN);
    assert_eq!(out, format!("{}...", "a".repeat(MAX_FILENAME_LEN)));
}

#[test]
fn truncate_filename_does_not_split_multibyte_at_limit() {
    // Regression: pesto-rt panicked at nfo.rs walk_tree when a filename
    // ended a 42-byte window inside `é` (bytes 41..43).
    let prefix = "a".repeat(41);
    let name = format!("{prefix}é-rest-of-a-very-long-filename.mp4");
    assert!(!name.is_char_boundary(MAX_FILENAME_LEN));
    let out = truncate_filename(&name, MAX_FILENAME_LEN);
    assert!(out.ends_with("..."));
    assert!(out.is_char_boundary(out.len()));
    assert_eq!(out, format!("{prefix}..."));
}

// ── is_series_folder ─────────────────────────────────────────────────────

#[test]
fn series_folder_detection() {
    assert!(is_series_folder("Breaking.Bad.S01E01.mkv"));
    assert!(is_series_folder("Show.S02"));
    assert!(is_series_folder("My Series S03E05 720p"));
    assert!(!is_series_folder("Curso Python Avancado"));
    assert!(!is_series_folder("Documentary.2024"));
    assert!(!is_series_folder("AS01.mkv")); // 'A' is an alpha prefix
}

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

// ── has_video_in_subdirs / find_root_video ────────────────────────────────

#[test]
fn movie_folder_flat_has_no_video_in_subdirs() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("movie.mkv"), b"fake").unwrap();
    fs::write(dir.path().join("movie.srt"), b"sub").unwrap();
    assert!(!has_video_in_subdirs(dir.path()));
    assert!(find_root_video(dir.path()).is_some());
}

#[test]
fn course_folder_with_video_subdir_detected() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("01-intro");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("lesson1.mp4"), b"fake").unwrap();
    assert!(has_video_in_subdirs(dir.path()));
}

#[test]
fn movie_folder_with_subtitle_subdir_not_flagged_as_course() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("movie.mkv"), b"fake").unwrap();
    let subs = dir.path().join("Subs");
    fs::create_dir(&subs).unwrap();
    fs::write(subs.join("English.srt"), b"sub").unwrap(); // not a video
    assert!(!has_video_in_subdirs(dir.path()));
    assert!(find_root_video(dir.path()).is_some());
}

#[test]
fn find_root_video_ignores_subdirs() {
    let dir = TempDir::new().unwrap();
    let sub = dir.path().join("extras");
    fs::create_dir(&sub).unwrap();
    fs::write(sub.join("bonus.mkv"), b"fake").unwrap(); // in subdir
                                                        // No video at root level
    assert!(find_root_video(dir.path()).is_none());
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
fn find_dvd_disc_roots_detects_menuless_disc() {
    // Case B: standard VIDEO_TS/ layout but no VIDEO_TS.VOB (menu stripped).
    let dir = TempDir::new().unwrap();
    let vts = dir.path().join("VIDEO_TS");
    fs::create_dir_all(&vts).unwrap();
    fs::write(vts.join("VIDEO_TS.IFO"), b"").unwrap();
    fs::write(vts.join("VIDEO_TS.BUP"), b"").unwrap();
    // No VIDEO_TS.VOB intentionally.
    fs::write(vts.join("VTS_01_0.IFO"), b"").unwrap();
    fs::write(vts.join("VTS_01_1.VOB"), b"").unwrap();

    let roots = find_dvd_disc_roots(dir.path());
    assert_eq!(roots.len(), 1, "menuless disc must still be detected");
    assert_eq!(roots[0], dir.path());
}

#[test]
fn find_dvd_disc_roots_detects_flat_layout() {
    // Case A: IFO/BUP/VOB files directly in disc root (no VIDEO_TS/ subfolder).
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("VIDEO_TS.IFO"), b"").unwrap();
    fs::write(dir.path().join("VIDEO_TS.BUP"), b"").unwrap();
    fs::write(dir.path().join("VIDEO_TS.VOB"), b"").unwrap();
    fs::write(dir.path().join("VTS_01_0.IFO"), b"").unwrap();
    fs::write(dir.path().join("VTS_01_1.VOB"), b"").unwrap();

    let roots = find_dvd_disc_roots(dir.path());
    assert_eq!(roots.len(), 1, "flat layout must be detected");
    assert_eq!(
        roots[0],
        dir.path(),
        "disc root must be the release folder itself"
    );
}

#[test]
fn find_title_ifo_handles_flat_layout() {
    // Case A: VTS IFOs directly in disc root (no VIDEO_TS/ subfolder).
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("VIDEO_TS.IFO"), b"").unwrap();
    fs::write(dir.path().join("VTS_01_0.IFO"), b"").unwrap();
    fs::write(dir.path().join("VTS_01_1.VOB"), b"data").unwrap();

    let ifo = find_title_ifo(dir.path()).unwrap();
    assert_eq!(ifo.file_name().unwrap(), "VTS_01_0.IFO");
}

#[test]
fn find_dvd_disc_roots_empty_for_non_dvd() {
    let dir = TempDir::new().unwrap();
    fs::write(dir.path().join("movie.mkv"), b"").unwrap();

    let roots = find_dvd_disc_roots(dir.path());
    assert!(roots.is_empty());
}

#[test]
fn find_title_ifo_picks_largest_vob_set() {
    // VTS_01 has one empty VOB; VTS_02 has no VOBs → VTS_01 wins by size.
    // When sizes tie (both 0), alphabetically first wins.
    let dir = TempDir::new().unwrap();
    make_dvd_structure(dir.path());
    fs::write(dir.path().join("VIDEO_TS").join("VTS_02_0.IFO"), b"").unwrap();

    let ifo = find_title_ifo(dir.path()).unwrap();
    assert_eq!(ifo.file_name().unwrap(), "VTS_01_0.IFO");
}

#[test]
fn find_title_ifo_picks_vts_with_more_vob_bytes() {
    // VTS_02 has a larger VOB than VTS_01 → VTS_02 should win.
    let dir = TempDir::new().unwrap();
    let vts = dir.path().join("VIDEO_TS");
    fs::create_dir_all(&vts).unwrap();
    fs::write(vts.join("VIDEO_TS.IFO"), b"").unwrap();
    fs::write(vts.join("VIDEO_TS.BUP"), b"").unwrap();
    fs::write(vts.join("VIDEO_TS.VOB"), b"").unwrap();
    fs::write(vts.join("VTS_01_0.IFO"), b"").unwrap();
    fs::write(vts.join("VTS_01_1.VOB"), b"small").unwrap();
    fs::write(vts.join("VTS_02_0.IFO"), b"").unwrap();
    fs::write(vts.join("VTS_02_1.VOB"), vec![0u8; 1024]).unwrap();

    let ifo = find_title_ifo(dir.path()).unwrap();
    assert_eq!(ifo.file_name().unwrap(), "VTS_02_0.IFO");
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

// ── DVD language injection ────────────────────────────────────────────────

/// Live integration test: requires the Voyage disc at the known path and
/// mediainfo in PATH. Skipped automatically when the IFO is absent.
#[test]
fn dvd_language_injection_voyage() {
    let ifo = std::path::PathBuf::from(
            "/media/ironwolf/downloads/Voyage.to.the.Bottom.of.the.Sea.1961.NTSC.DVD9.DD5.1-Win/VIDEO_TS/VTS_01_0.IFO",
        );
    if !ifo.exists() {
        return; // disc not mounted — skip silently
    }

    let mi = match run_mediainfo(&ifo) {
        Ok(s) => s,
        Err(_) => return, // mediainfo not available — skip
    };

    // Baseline: no Language tags present before injection.
    // Simple check: count Language lines in Audio/Text sections.
    let lang_lines_before: usize = {
        let mut in_av = false;
        let mut count = 0;
        for line in mi.lines() {
            let t = line.trim_start();
            if t.starts_with("Audio") || t.starts_with("Text") {
                in_av = true;
            } else if !t.starts_with(' ') && !t.is_empty() && !t.contains(" : ") {
                in_av = false;
            }
            if in_av && t.starts_with("Language") {
                count += 1;
            }
        }
        count
    };
    assert_eq!(
        lang_lines_before, 0,
        "expected no Language in raw mediainfo output"
    );

    let injected = inject_dvd_language_tags(&mi, &ifo);

    // After injection: each audio stream should have a Language line.
    let audio_sections: Vec<&str> = injected.split("\nAudio").collect();
    for section in audio_sections.iter().skip(1) {
        assert!(
            section.contains("Language"),
            "Audio section missing Language after injection:\n{section}"
        );
    }

    // Spot-check known languages: en, es present for audio.
    assert!(
        injected.contains("Language                                 : en"),
        "expected English audio"
    );
    assert!(
        injected.contains("Language                                 : es"),
        "expected Spanish audio"
    );

    // Subtitle sections should also carry Language.
    let text_sections: Vec<&str> = injected.split("\nText").collect();
    for section in text_sections.iter().skip(1) {
        assert!(
            section.contains("Language"),
            "Text section missing Language after injection:\n{section}"
        );
    }
    // pt (Portuguese) subtitle expected.
    assert!(
        injected.contains("Language                                 : pt"),
        "expected Portuguese subtitle"
    );
}
