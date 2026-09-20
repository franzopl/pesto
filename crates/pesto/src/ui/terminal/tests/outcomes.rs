use super::super::*;
use super::started_state;
use crate::progress::FileEntry;
use crate::ui::render::visible_len;

/// Drive `state` to upload-complete with the streaming check part-way
/// through — the tail where the old panel showed "posting PAR2" and a
/// bar-less check box.
fn upload_done_check_running() -> RenderState {
    let mut state = started_state(false);
    let remaining = state.total_segments - state.done_segments;
    for _ in 0..remaining {
        state.apply(ProgressEvent::SegmentDone {
            file: "Movie.2026/movie.mkv".to_string(),
            bytes: 768_000,
            ok: true,
        });
    }
    // Half the plan confirmed by the check queue.
    state.apply(ProgressEvent::CheckProgress {
        checked: state.total_segments / 2,
        ok: true,
    });
    state
}

#[test]
fn header_says_verifying_once_the_upload_is_done_and_check_runs() {
    let state = upload_done_check_running();
    let header = &state.panel_lines(false, 100)[0];
    assert!(
        header.contains("verifying"),
        "header should move on from posting to verifying: {header:?}"
    );
    assert!(
        !header.contains("posting"),
        "header still claims posting after the upload finished: {header:?}"
    );
}

#[test]
fn check_box_has_no_redundant_bar() {
    // The upload bar already shows check progress as its trailing blue
    // band; a second bar in the check box duplicated it. The box keeps the
    // verified/pending tally but draws no bar, percentage, or `N/M
    // checked` line of its own.
    let state = upload_done_check_running();
    let panel = state.panel_lines(false, 100);
    let tally = panel
        .iter()
        .find(|l| l.contains("confirmed available"))
        .expect("check box tally line drawn");
    assert!(
        !tally.contains('%'),
        "check box should not show its own percentage: {tally:?}"
    );
    assert!(
        !panel.iter().any(|l| l.contains("checked")),
        "the separate check bar line should be gone"
    );
}

#[test]
fn check_is_excluded_from_the_overall_eta() {
    // The check's bursty throughput would distort the ETA, so it must not
    // feed `overall_eta_secs`. With the upload complete and no PAR2 work
    // pending, the ETA resolves to None rather than a check-derived guess.
    let state = upload_done_check_running();
    assert!(
        state.overall_eta_secs().is_none(),
        "a lone draining check must not synthesise an ETA"
    );
}

/// Upload finished, streaming check drained with a couple of stubborn
/// misses, and the final recovery pass just started on that small tail —
/// mirrors what `poster::post_files_inner` actually does: `CheckDone`
/// fires (turning `check_active` off) strictly before
/// `check::recover_missing` (and its `CheckRecoverStarted`) ever runs.
fn recovery_pass_running() -> RenderState {
    let mut state = upload_done_check_running();
    state.apply(ProgressEvent::CheckDone {
        failed: 2,
        inconclusive: 0,
    });
    state.apply(ProgressEvent::CheckRecoverStarted { total: 2 });
    state
}

#[test]
fn header_says_recovering_during_the_final_recovery_pass() {
    let state = recovery_pass_running();
    let header = &state.panel_lines(false, 100)[0];
    assert!(
        header.contains("recovering"),
        "header should name the recovery pass: {header:?}"
    );
    assert!(
        !header.contains("writing PAR2"),
        "header must not fall through to the PAR2-write label during recovery: {header:?}"
    );
    assert!(
        !header.contains("verifying"),
        "the streaming check already finished by the time recovery starts: {header:?}"
    );
}

#[test]
fn recover_box_replaces_the_check_box_and_tracks_progress() {
    let mut state = recovery_pass_running();
    let panel = state.panel_lines(false, 100);
    assert!(
        panel.iter().any(|l| l.contains("recover")),
        "a recover box should be drawn while the pass is active:\n{}",
        panel.join("\n")
    );
    assert!(
        !panel.iter().any(|l| l.contains("verified")),
        "the check box's tally line must not still be showing:\n{}",
        panel.join("\n")
    );

    state.apply(ProgressEvent::CheckRecoverProgress {
        done: 1,
        total: 2,
        ok: true,
    });
    let panel = state.panel_lines(false, 100);
    let bar_line = panel
        .iter()
        .find(|l| l.contains("article(s)"))
        .expect("recover progress line drawn");
    assert!(
        bar_line.contains("1/2 article(s)"),
        "recover box should track done/total: {bar_line:?}"
    );
    let tally_line = panel
        .iter()
        .find(|l| l.contains("recovered"))
        .expect("recover tally line drawn");
    assert!(
        tally_line.contains("1 recovered"),
        "recover box should count confirmed reposts: {tally_line:?}"
    );
}

#[test]
fn a_confirmed_recovery_repairs_the_missing_and_reposted_tallies() {
    // `CheckDone` snapshots `check_failed` *before* the recovery pass
    // gets a chance to fix anything; a successful recovery must correct
    // that snapshot (and credit the repost), or the final summary would
    // go on reporting an article as missing after it was actually
    // confirmed present.
    let mut state = recovery_pass_running();
    assert_eq!(state.check_failed, 2);
    assert_eq!(state.check_reposted, 0);

    state.apply(ProgressEvent::CheckRecoverProgress {
        done: 1,
        total: 2,
        ok: true,
    });
    assert_eq!(state.check_failed, 1);
    assert_eq!(state.check_reposted, 1);
}

#[test]
fn a_failed_recovery_still_reports_progress_instead_of_going_silent() {
    // Before this fix, a repost or final-STAT failure inside the
    // recovery pass emitted nothing but a `tracing::warn!` (invisible
    // without `-v`) — the batch could go silent for however long the
    // remaining round trips took. Every resolution, success or failure,
    // must now move `recover_done` and show up in the panel.
    let mut state = recovery_pass_running();
    state.apply(ProgressEvent::CheckRecoverProgress {
        done: 1,
        total: 2,
        ok: false,
    });
    assert_eq!(state.recover_done, 1);
    assert_eq!(state.recover_failed, 1);
    // A failed resolution must not be mistaken for a fixed article.
    assert_eq!(state.check_failed, 2);
    let panel = state.panel_lines(false, 100);
    let tally_line = panel
        .iter()
        .find(|l| l.contains("recovered"))
        .expect("recover tally line drawn");
    assert!(
        tally_line.contains("still missing"),
        "a failed resolution should surface in the panel, not go silent: {tally_line:?}"
    );
}

#[test]
fn connection_activity_is_a_single_dot_row() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::ConnectionBusy {
        conn: 0,
        file: "Movie.2026/movie.mkv".to_string(),
    });
    let panel = state.panel_lines(false, 100);
    let conn_lines: Vec<&String> = panel.iter().filter(|l| l.contains("conns")).collect();
    assert_eq!(conn_lines.len(), 1, "the grid should collapse to one line");
    let line = conn_lines[0];
    assert!(line.contains('●'), "a busy worker should show a filled dot");
    assert!(line.contains('○'), "idle workers should show hollow dots");
    assert!(line.contains("7/7 active") || line.contains("1/7 active"));
}

#[test]
fn final_summary_is_two_lines_and_reports_the_outcome() {
    let mut state = upload_done_check_running();
    // Finish the check clean.
    state.apply(ProgressEvent::CheckProgress {
        checked: state.total_segments,
        ok: true,
    });
    state.apply(ProgressEvent::CheckDone {
        failed: 0,
        inconclusive: 0,
    });
    for width in [60, 80, 100] {
        let summary = state.summary_lines(width);
        assert_eq!(summary.len(), 2, "clean summary stays at two lines");
        assert!(summary[0].contains('✓'), "clean run gets a check glyph");
        assert!(
            summary[0].contains("articles confirmed"),
            "a fully-confirmed run should say so: {:?}",
            summary[0]
        );
        assert!(!summary.iter().any(|line| line.contains('…')));
        assert!(summary.iter().all(|line| visible_len(line) <= width));
        // No frozen box borders in the summary.
        assert!(!summary.iter().any(|line| line.contains('│')));
    }

    let narrow = state.summary_lines(40);
    assert!(!narrow.iter().any(|line| line.contains('…')));
    assert!(narrow.iter().all(|line| visible_len(line) <= 40));
    assert!(narrow.join(" ").contains("articles confirmed"));
}

#[test]
fn final_summary_flags_failures() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::SegmentDone {
        file: "Movie.2026/movie.mkv".to_string(),
        bytes: 768_000,
        ok: false,
    });
    let summary = state.summary_lines(100);
    assert!(summary[0].contains('✗'), "a failed run gets a cross glyph");
    assert!(summary[0].contains("unresolved failure"));
}

#[test]
fn recovered_post_rejection_is_a_success_with_an_amber_counter() {
    let mut state = started_state(false);
    let remaining = state.total_segments - state.done_segments;
    for _ in 1..remaining {
        state.apply(ProgressEvent::SegmentDone {
            file: "Movie.2026/movie.mkv".to_string(),
            bytes: 768_000,
            ok: true,
        });
    }
    state.apply(ProgressEvent::PostRetryQueued);
    state.apply(ProgressEvent::Failed {
        description: "Movie.2026/movie.mkv part 785/785: 440 Posting Not Allowed".to_string(),
    });
    state.apply(ProgressEvent::SegmentDone {
        file: "Movie.2026/movie.mkv".to_string(),
        bytes: 768_000,
        ok: false,
    });

    let retrying_panel = state.panel_lines(false, 100).join("\n");
    assert!(retrying_panel.contains("temporarily retrying"));
    assert!(!retrying_panel.contains("\x1b[31m"));

    state.apply(ProgressEvent::PostRetryRecovered {
        count: 1,
        previously_failed: true,
    });
    state.apply(ProgressEvent::CheckProgress {
        checked: state.total_segments,
        ok: true,
    });
    state.apply(ProgressEvent::CheckDone {
        failed: 0,
        inconclusive: 0,
    });

    for width in [60, 80] {
        let lines = state.summary_lines(width);
        assert_eq!(lines.len(), 3, "a recovered retry gets one extra line");
        assert!(lines[0].contains("Upload complete"));
        assert!(lines[0].contains("articles confirmed"));
        assert!(!lines[0].contains("retry"));
        assert!(lines[2].contains("↻ Recovered automatically after 1 temporary retry"));
        assert!(!lines.iter().any(|line| line.contains('…')));
        assert!(lines.iter().all(|line| visible_len(line) <= width));
        assert!(!lines.join("\n").contains("\x1b[31m"));
    }
    assert_eq!(state.failures, 0);
}

#[test]
fn recovered_verification_miss_is_counted_only_after_confirmation() {
    let mut state = upload_done_check_running();
    state.apply(ProgressEvent::CheckReposted { reposted: 1 });
    assert_eq!(state.recovered_retries(), 0);
    let pending_panel = state.panel_lines(false, 100).join("\n");
    assert!(pending_panel.contains("1 retry awaiting confirmation"));

    state.apply(ProgressEvent::CheckRetryRecovered);
    let recovered_panel = state.panel_lines(false, 100).join("\n");
    assert!(!recovered_panel.contains("awaiting confirmation"));
    assert!(!recovered_panel.contains("issues"));
    state.apply(ProgressEvent::CheckProgress {
        checked: state.total_segments,
        ok: true,
    });
    state.apply(ProgressEvent::CheckDone {
        failed: 0,
        inconclusive: 0,
    });

    let summary = state.summary_lines(80).join("\n");
    assert!(summary.contains("Upload complete"));
    assert!(summary.contains("Recovered automatically after 1 temporary retry"));
    assert!(summary.contains("articles confirmed"));
    assert!(!summary.contains('…'));
}

#[test]
fn unrecoverable_verification_miss_is_red_and_actionable() {
    let mut state = upload_done_check_running();
    state.apply(ProgressEvent::CheckProgress {
        checked: state.total_segments,
        ok: false,
    });
    state.apply(ProgressEvent::CheckDone {
        failed: 1,
        inconclusive: 0,
    });

    let summary = state.summary_lines(160).join("\n");
    assert!(summary.contains("Upload incomplete"));
    assert!(summary.contains("1 unresolved failure"));
    assert!(
        summary.contains('✗'),
        "unresolved failures use the error glyph"
    );
    assert!(summary.contains("Retry with --resume"));
    assert!(!summary.contains("Upload complete"));
}

#[test]
fn checks_disabled_never_claims_articles_were_confirmed() {
    let mut state = started_state(false);
    state.checks_enabled = false;
    state.check_connections = 0;
    let remaining = state.total_segments - state.done_segments;
    for _ in 0..remaining {
        state.apply(ProgressEvent::SegmentDone {
            file: "Movie.2026/movie.mkv".to_string(),
            bytes: 768_000,
            ok: true,
        });
    }

    let summary = state.summary_lines(120).join("\n");
    assert!(summary.contains("accepted by the server"));
    assert!(summary.contains("Not independently verified"));
    assert!(!summary.contains("articles confirmed"));
}

#[test]
fn inconclusive_is_shown_distinctly_from_missing() {
    let mut state = upload_done_check_running();
    state.apply(ProgressEvent::CheckInconclusive {
        count: 2,
        reason: "connection error",
    });
    state.apply(ProgressEvent::CheckProgress {
        checked: state.total_segments,
        ok: false,
    });
    state.apply(ProgressEvent::CheckDone {
        failed: 1,
        inconclusive: 2,
    });

    let panel = state.panel_lines(true, 160).join("\n");
    assert!(
        panel.contains("2 inconclusive (check path failed — not a confirmed gap)"),
        "availability box should name inconclusive distinctly:\n{panel}"
    );
    assert!(
        panel.contains("1 missing"),
        "confirmed-missing tally must still appear alongside inconclusive:\n{panel}"
    );
    assert!(
        !panel.contains("2 missing"),
        "inconclusive count must not be labelled as missing:\n{panel}"
    );

    let summary = state.summary_lines(160).join("\n");
    assert!(
        summary.contains("2 inconclusive (check path failed — not a confirmed gap)"),
        "final summary should name inconclusive:\n{summary}"
    );
    assert!(
        summary.contains("unresolved failure"),
        "NZB-policy missing count stays in the summary:\n{summary}"
    );
    assert!(summary.contains('✗'));
}

#[test]
fn fast_repost_is_shown_when_it_fires() {
    let mut state = upload_done_check_running();
    state.apply(ProgressEvent::CheckFastRepost {
        first_checks: 20,
        first_misses: 1,
    });
    let panel = state.panel_lines(false, 160).join("\n");
    assert!(
        panel.contains("fast-repost: isolated miss (miss rate 5% of 20 first checks)"),
        "availability box should report the fast-repost heuristic:\n{panel}"
    );
}

#[test]
fn inconclusive_check_never_claims_articles_were_confirmed() {
    let mut state = upload_done_check_running();
    state.apply(ProgressEvent::CheckInconclusive {
        count: 1,
        reason: "connection error",
    });
    state.apply(ProgressEvent::CheckDone {
        failed: 0,
        inconclusive: 1,
    });

    let summary = state.summary_lines(160).join("\n");
    assert!(summary.contains("Upload incomplete"));
    assert!(summary.contains("unresolved"));
    assert!(
        !summary.contains("articles confirmed"),
        "Inconclusive must not be reported as confirmed: {summary}"
    );
    assert!(!summary.contains("Upload complete"));
}

#[test]
fn explicit_par2_pass_events_keep_equal_counters_monotonic_and_name_compute() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::Par2EncodeStarted {
        input_bytes: 600_000_000,
        input_slices: 100,
        input_files: 1,
        recovery_slices: 20,
        slice_size: 768_000,
        passes: 2,
        chunk_size: 32_768,
        simd_method: "avx2".to_string(),
        threads: 6,
        memory_limit: 1 << 30,
    });
    state.apply(ProgressEvent::Par2PassStarted { pass: 1, passes: 2 });
    state.apply(ProgressEvent::Par2InputProgress {
        done: 1,
        total: 100,
    });
    let first = state.par2_encode_units_done();
    state.apply(ProgressEvent::Par2PassStarted { pass: 2, passes: 2 });
    state.apply(ProgressEvent::Par2InputProgress {
        done: 1,
        total: 100,
    });
    assert!(state.par2_encode_units_done() > first);

    state.apply(ProgressEvent::Par2ComputeStarted { pass: 2, passes: 2 });
    let panel = state.panel_lines(false, 120).join("\n");
    assert!(panel.contains("Computing recovery data — pass 2/2"));
}

// Issue #57: a run that dies to a `producer` error (bad PAR2 geometry, a
// memory-budget check, …) before posting anything used to print a bare
// green `✓` here, because `summary_lines` only looked at `interrupted`
// (set solely by a real Ctrl-C) and never at `failed_description` (set by
// `ProgressEvent::Failed`) — indistinguishable from a clean run.
#[test]
fn final_summary_flags_producer_failure_instead_of_a_bare_checkmark() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::Failed {
        description: "producer error: too many input slices: 46875 (max 32768)".to_string(),
    });
    let summary = state.summary_lines(100);
    assert!(
        !summary.iter().any(|l| l.contains('✓')),
        "a run that failed in producer must not show a green checkmark: {summary:?}"
    );
    assert!(
        summary.iter().any(|l| l.contains('⚠')),
        "a producer failure should get the same warning glyph as a cancellation: {summary:?}"
    );
    assert!(
        summary.iter().any(|l| l.contains("too many input slices")),
        "the actual failure reason should be visible in the final summary: {summary:?}"
    );
}

#[test]
fn abort_note_replaces_the_graceful_interrupt_hint() {
    let mut state = started_state(false);
    state.apply(ProgressEvent::Interrupted);
    let graceful = state.panel_lines(false, 120).join("\n");
    assert!(graceful.contains("Ctrl+C again to abort"));

    state.apply(ProgressEvent::Aborted);
    let aborted = state.panel_lines(false, 120).join("\n");
    assert!(aborted.contains("dropping connections, saving resume state"));
    assert!(!aborted.contains("Ctrl+C again to abort"));
}

#[test]
fn box_borders_align_with_wide_and_emoji_filenames() {
    // A CJK ideograph / wide emoji is two columns; `visible_len` measures
    // display width now, so the padded box interior stays rectangular even
    // when the active-file line below carries such a name.
    let mut state = RenderState::new();
    state.apply(ProgressEvent::Started {
        mode: RunMode::Post,
        files: vec![FileEntry {
            name: "映画作品２０２６/動画🎬.mkv".to_string(),
            segments: 70,
            bytes: 50_000_000,
        }],
        connections: 4,
        check_connections: 1,
        target: Some("news.example.com:563".to_string()),
        par2_bytes_hint: 0,
        par2_segments_hint: 0,
    });
    state.apply(ProgressEvent::ConnectionBusy {
        conn: 0,
        file: "映画作品２０２６/動画🎬.mkv".to_string(),
    });
    state.apply(ProgressEvent::SegmentDone {
        file: "映画作品２０２６/動画🎬.mkv".to_string(),
        bytes: 768_000,
        ok: true,
    });
    for width in [40, 60, 80] {
        let lines = state.panel_lines(false, width);
        let box_lines: Vec<&String> = lines
            .iter()
            .filter(|l| l.starts_with('┌') || l.starts_with('│') || l.starts_with('└'))
            .collect();
        let expected = visible_len(box_lines[0]);
        for line in &box_lines {
            assert_eq!(
                visible_len(line),
                expected,
                "wide-char content skewed a box edge at width={width}: {line:?}"
            );
        }
    }
}
