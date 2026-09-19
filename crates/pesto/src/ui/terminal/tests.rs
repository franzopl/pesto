use super::*;
use crate::progress::FileEntry;
use crate::ui::render::visible_len;

pub(super) fn started_state(width_samples: bool) -> RenderState {
    let mut state = RenderState::new();
    state.apply(ProgressEvent::Started {
        mode: RunMode::Post,
        files: vec![FileEntry {
            name: "Movie.2026/movie.mkv".to_string(),
            segments: 785,
            bytes: 600_000_000,
        }],
        connections: 7,
        check_connections: 1,
        target: Some("news.example.com:563".to_string()),
        par2_bytes_hint: 60_000_000,
        par2_segments_hint: 79,
    });
    for _ in 0..300 {
        state.apply(ProgressEvent::SegmentDone {
            file: "Movie.2026/movie.mkv".to_string(),
            bytes: 768_000,
            ok: true,
        });
    }
    if width_samples {
        state.push_speed_sample(50_000_000.0);
        state.push_speed_sample(60_000_000.0);
    }
    state
}

#[test]
fn box_borders_survive_the_terminal_width_truncation() {
    // The panel used to be a fixed 60 columns wide, so a narrower
    // terminal had `draw_panel`'s truncate eat the right-hand border of
    // every box, leaving `┌─ upload ────…`. Boxes must now be sized so
    // that same truncation is a no-op for them. Below `MIN_BODY_W + 4`
    // columns there is nothing to be done, so that is the floor.
    let state = started_state(true);
    for width in [MIN_BODY_W + 4, 30, 40, 50, 60, 80, 100, 140, 200] {
        for line in state.panel_lines(false, width) {
            let first = line.chars().next().unwrap_or(' ');
            if !"┌│└".contains(first) {
                continue;
            }
            assert_eq!(
                truncate(&line, width),
                line,
                "box line loses its right border at width={width}"
            );
            let last = line.chars().last().unwrap_or(' ');
            assert!(
                "┐│┘".contains(last),
                "width={width} left a ragged box line: {line:?}"
            );
        }
    }
}

#[test]
fn box_borders_line_up_with_each_other() {
    let state = started_state(false);
    for width in [40, 60, 80, 120] {
        let lines = state.panel_lines(false, width);
        let box_lines: Vec<&String> = lines
            .iter()
            .filter(|l| l.starts_with('┌') || l.starts_with('│') || l.starts_with('└'))
            .collect();
        assert!(!box_lines.is_empty(), "no box drawn at width={width}");
        let expected = visible_len(box_lines[0]);
        for line in &box_lines {
            assert_eq!(
                visible_len(line),
                expected,
                "ragged box edge at width={width}: {line:?}"
            );
        }
    }
}

#[test]
fn quiet_and_panel_report_the_same_percentage() {
    // `-q` used to divide bytes (freezing at 95% because `total_bytes`
    // carries the unconsumed PAR2 hint) while the panel divided segments.
    let state = started_state(false);
    let pct_quiet = (state.progress_frac() * 100.0).round() as u64;
    let panel = state.panel_lines(false, 80);
    let bar_line = panel
        .iter()
        .find(|l| l.contains("seg"))
        .expect("upload box drawn");
    assert!(
        bar_line.contains(&format!("{pct_quiet}%")),
        "panel line {bar_line:?} disagrees with quiet {pct_quiet}%"
    );
}

#[test]
fn progress_reaches_100_percent_when_every_segment_is_done() {
    let mut state = started_state(false);
    let remaining = state.total_segments - state.done_segments;
    for _ in 0..remaining {
        state.apply(ProgressEvent::SegmentDone {
            file: "Movie.2026/movie.mkv".to_string(),
            bytes: 768_000,
            ok: true,
        });
    }
    assert_eq!((state.progress_frac() * 100.0).round() as u64, 100);
}

#[test]
fn par2_encoder_details_and_process_line_stay_out_of_the_panel() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::Par2EncodeStarted {
        input_bytes: 600_000_000,
        input_slices: 785,
        input_files: 1,
        recovery_slices: 78,
        slice_size: 768_000,
        passes: 1,
        chunk_size: 32_768,
        simd_method: "avx2+gfni".to_string(),
        threads: 6,
        memory_limit: 16 << 30,
    });
    state.proc_rss_bytes = 200 << 20;
    let panel = state.panel_lines(false, 100).join("\n");
    assert!(
        panel.contains("par2  [")
            && panel.contains("Reading sources")
            && panel.contains("0/785 slices"),
        "the combined par2 progress bar itself must stay:\n{panel}"
    );
    for noise in [
        "PAR2 encoder",
        "Multiply method",
        "Memory usage",
        "Input pass(es)",
        "process  ram",
    ] {
        assert!(
            !panel.contains(noise),
            "{noise:?} should not be in the default panel:\n{panel}"
        );
    }
}

#[test]
fn par2_bar_is_one_line_and_never_regresses_across_encode_then_write() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::Par2EncodeStarted {
        input_bytes: 600_000_000,
        input_slices: 100,
        input_files: 1,
        recovery_slices: 20,
        slice_size: 768_000,
        passes: 1,
        chunk_size: 32_768,
        simd_method: "avx2".to_string(),
        threads: 6,
        memory_limit: 16 << 30,
    });

    // Walk encode 0→100, then write 0→20, sampling the combined fraction.
    let mut last = -1.0_f64;
    let mut par2_line_counts = Vec::new();
    let mut sample = |st: &RenderState| {
        let panel = st.panel_lines(false, 100);
        let par2: Vec<&String> = panel.iter().filter(|l| l.contains("par2  [")).collect();
        par2_line_counts.push(par2.len());
    };

    for done in (0..=100).step_by(10) {
        state.apply(ProgressEvent::Par2InputProgress { done, total: 100 });
        sample(&state);
        let frac = (state.par2_encode_done + state.par2_write_done as usize) as f64
            / (state.par2_encode_total + state.par2_recovery_total) as f64;
        assert!(
            frac + 1e-9 >= last,
            "par2 fraction went backward: {frac} < {last}"
        );
        last = frac;
    }
    state.apply(ProgressEvent::Par2WriteStarted { total: 20 });
    for _ in 0..20 {
        state.apply(ProgressEvent::Par2SliceWritten);
        sample(&state);
        let frac = (state.par2_encode_done + state.par2_write_done as usize) as f64
            / (state.par2_encode_total + state.par2_recovery_total) as f64;
        assert!(
            frac + 1e-9 >= last,
            "par2 fraction went backward: {frac} < {last}"
        );
        last = frac;
    }

    // At most one par2 line at any sampled moment — never the old two.
    assert!(
        par2_line_counts.iter().all(|&n| n <= 1),
        "par2 should render as a single line, saw counts {par2_line_counts:?}"
    );
}

#[test]
fn par2_bar_does_not_reset_on_a_multi_pass_encode() {
    // A tight memory budget splits the recovery set across passes, and
    // every pass re-reads the whole input: `poster` declares the
    // `Par2InputProgress` counter *inside* its pass loop, so `done`
    // restarts at 0 each time. Without pass accounting the bar snapped
    // back to zero once per pass.
    const SLICES: usize = 200;
    const PASSES: usize = 3;
    let mut state = started_state(false);
    state.apply(ProgressEvent::Par2EncodeStarted {
        input_bytes: 600_000_000,
        input_slices: SLICES,
        input_files: 1,
        recovery_slices: 60,
        slice_size: 768_000,
        passes: PASSES,
        chunk_size: 32_768,
        simd_method: "avx2".to_string(),
        threads: 6,
        memory_limit: 1 << 30,
    });

    let frac = |st: &RenderState| {
        (st.par2_encode_units_done() + st.par2_write_done as usize) as f64
            / (st.par2_encode_units_total() + st.par2_recovery_total) as f64
    };

    let mut last = 0.0_f64;
    let mut last_pass_panel = String::new();
    for pass in 0..PASSES {
        for done in (0..=SLICES).step_by(20) {
            state.apply(ProgressEvent::Par2InputProgress {
                done,
                total: SLICES,
            });
            let f = frac(&state);
            assert!(
                f + 1e-9 >= last,
                "bar reset on pass {pass} at done={done}: {f} < {last}"
            );
            last = f;
            if pass == PASSES - 1 && done == 20 {
                last_pass_panel = state.panel_lines(false, 100).join("\n");
            }
        }
    }
    assert_eq!(
        state.par2_pass_index,
        PASSES - 1,
        "should have tracked every pass rollover"
    );
    // Encode complete across all passes → the bar sits at the encode
    // share, then the write stage carries it to 100%.
    assert_eq!(state.par2_encode_units_done(), SLICES * PASSES);

    // And the panel names the pass while multi-pass encoding is running.
    assert!(
        last_pass_panel.contains("pass 3/3"),
        "multi-pass encode should name the current pass:\n{last_pass_panel}"
    );
}

#[test]
fn par2_never_shows_100_percent_while_work_remains() {
    // The line only renders while PAR2 work is outstanding, so a rounded
    // 99.8% reading "100%" claimed the stage was finished while recovery
    // slices were still being written.
    let mut state = started_state(false);
    state.apply(ProgressEvent::Par2EncodeStarted {
        input_bytes: 600_000_000,
        input_slices: 912,
        input_files: 1,
        recovery_slices: 273,
        slice_size: 768_000,
        passes: 1,
        chunk_size: 32_768,
        simd_method: "avx2".to_string(),
        threads: 6,
        memory_limit: 1 << 30,
    });
    state.apply(ProgressEvent::Par2InputProgress {
        done: 912,
        total: 912,
    });
    state.apply(ProgressEvent::Par2WriteStarted { total: 273 });
    // Write all but the last few slices — 1182/1185 ≈ 99.7%.
    for _ in 0..270 {
        state.apply(ProgressEvent::Par2SliceWritten);
    }
    let panel = state.panel_lines(false, 100).join("\n");
    assert!(
        panel.contains("par2  ["),
        "par2 line should still be drawn:\n{panel}"
    );
    assert!(
        !panel.contains("100%"),
        "par2 must not read 100% with slices left:\n{panel}"
    );
}

#[test]
fn proxy_status_has_a_dedicated_persistent_panel() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::ProxyStatus {
        text: "SOCKS5 proxy active via 127.0.0.1:1080; remote DNS enabled".to_string(),
    });
    state.apply(ProgressEvent::Status {
        text: "computing recovery data".to_string(),
    });
    let panel = state.panel_lines(false, 100).join("\n");
    assert!(panel.contains("proxy"), "proxy area missing:\n{panel}");
    assert!(
        panel.contains("SOCKS5 proxy active via 127.0.0.1:1080"),
        "proxy route missing:\n{panel}"
    );
    assert!(
        panel.contains("computing recovery data"),
        "status missing:\n{panel}"
    );
}

#[test]
fn status_text_appears_exactly_once() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::Status {
        text: "memory: address-space limit none detected".to_string(),
    });
    let hits = state
        .panel_lines(false, 200)
        .iter()
        .filter(|l| l.contains("address-space limit"))
        .count();
    assert_eq!(hits, 1, "status was drawn twice (cyan line + `▸` line)");
}

#[path = "tests/outcomes.rs"]
mod outcomes;
