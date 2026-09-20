//! Progress event formatting and session summary logging.

use crate::events::{FileProgressUpdate, ProgressUpdate};

/// Append a one-line structured summary to the session log file.
///
/// Written after all tracing events so it is always the last line, making
/// `tail -1` a reliable way to check the outcome of any upload.
pub(crate) fn write_session_summary(
    path: &std::path::Path,
    label: &str,
    outcome: &pesto::upload::UploadOutcome,
) {
    use std::io::Write;

    let status = if outcome.cancelled {
        "cancelled"
    } else if outcome.had_failures {
        "failed"
    } else {
        "ok"
    };

    let total_mb = outcome.total_bytes as f64 / 1_048_576.0;
    let nzb = outcome
        .nzb_path
        .as_deref()
        .and_then(|p| p.file_name())
        .and_then(|n| n.to_str())
        .unwrap_or("-");

    let now = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
    let line = format!(
        "{now}  summary  status={status}  label=\"{label}\"  segments={}  bytes={:.1}MiB  nzb={nzb}\n",
        outcome.segments.len(),
        total_mb,
    );

    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = f.write_all(line.as_bytes());
    }
}

/// Convert rich pesto ProgressEvent into a single-line string for the log.
pub(crate) fn format_progress_event(ev: &pesto::progress::ProgressEvent) -> String {
    use pesto::progress::ProgressEvent as E;

    match ev {
        E::Started {
            files,
            connections,
            mode,
            ..
        } => {
            format!(
                "Started ({:?}) — {} files, {} connections",
                mode,
                files.len(),
                connections
            )
        }
        E::SegmentDone { file, ok, .. } if !ok => {
            format!("Segment FAILED — {}", file)
        }
        E::SegmentDone { .. } => String::new(), // shown in gauge, not logs
        E::Status { text } if !text.is_empty() => text.clone(),
        E::QueueExtended { file, segments, .. } => {
            format!("PAR2 extended queue: {} (+{} segments)", file, segments)
        }
        E::Par2EncodeStarted {
            input_files,
            recovery_slices,
            ..
        } => {
            format!(
                "PAR2 encoding started — {} files, {} recovery slices",
                input_files, recovery_slices
            )
        }
        E::Par2InputProgress { .. } => String::new(), // shown in gauge, not logs
        E::Par2SliceWritten => String::new(),         // too noisy
        E::Finished => "=== Pesto run finished ===".into(),
        E::Failed { description } => format!("FAILED: {}", description),
        E::Interrupted => "Interrupted by user".into(),
        E::Paused => "=== Upload paused ===".into(),
        E::Resumed => "=== Upload resumed ===".into(),
        E::CheckDone {
            failed,
            inconclusive,
        } if *failed == 0 && *inconclusive == 0 => "✓ Check: all articles verified".into(),
        E::CheckDone {
            failed,
            inconclusive,
        } => {
            let mut parts = Vec::new();
            if *failed > 0 {
                parts.push(format!(
                    "{failed} article(s) still missing after every repost attempt"
                ));
            }
            if *inconclusive > 0 {
                parts.push(format!(
                    "{inconclusive} inconclusive (check path failed — not a confirmed gap)"
                ));
            }
            format!("✗ Check: {}", parts.join("; "))
        }
        E::CheckRetrying {
            attempt,
            max_attempts,
            delay_secs,
            reason,
        } => format!("Check retry {attempt}/{max_attempts} ({reason}, waiting {delay_secs}s)…"),
        E::CheckReposted { reposted } => format!("Check: reposted article #{reposted}"),
        _ => String::new(), // many low-level events are too noisy for the TUI log
    }
}

/// Extract accurate numbers from ProgressEvent for the progress bar + per-file updates
pub(crate) fn extract_progress_update(
    ev: &pesto::progress::ProgressEvent,
    previous: &ProgressUpdate,
) -> Option<ProgressUpdate> {
    use crate::events::UploadPhase;
    use pesto::progress::ProgressEvent as E;

    match ev {
        E::Started {
            files,
            par2_bytes_hint,
            par2_segments_hint,
            ..
        } => {
            let total_segments: u64 =
                files.iter().map(|f| f.segments).sum::<u64>() + par2_segments_hint;
            let total_bytes: u64 = files.iter().map(|f| f.bytes).sum::<u64>() + par2_bytes_hint;
            Some(ProgressUpdate {
                done_segments: 0,
                total_segments,
                done_bytes: 0,
                total_bytes,
                current_speed_mbps: 0.0,
                message: None,
                file_update: None,
                phase: Some(UploadPhase::Uploading),
                par2_slices: None,
                check_progress: None,
                queue_extended: None,
                par2_hint_bytes: *par2_bytes_hint,
                par2_segment_hint: *par2_segments_hint,
                par2_complete: false,
            })
        }
        E::CompressStarted { total_bytes } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Compressing {
                done_bytes: 0,
                total_bytes: *total_bytes,
            }),
            par2_slices: None,
            check_progress: None,
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
        }),
        E::CompressProgress { bytes_written } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Compressing {
                done_bytes: *bytes_written,
                total_bytes: match &previous.phase {
                    Some(UploadPhase::Compressing { total_bytes, .. }) => *total_bytes,
                    _ => 0,
                },
            }),
            par2_slices: None,
            check_progress: None,
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
        }),
        E::CompressDone => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Preparing),
            par2_slices: None,
            check_progress: None,
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
        }),
        // Par2EncodeStarted is a config announcement, NOT a sequential phase.
        // PAR2 encoding runs concurrently with NNTP posting. Store total slices
        // for the concurrent progress indicator; keep the phase as Uploading.
        E::Par2EncodeStarted {
            recovery_slices, ..
        } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::Uploading),
            par2_slices: Some((0, *recovery_slices)),
            check_progress: None,
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
        }),
        E::Par2InputProgress { done, total } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: None, // phase stays Uploading
            par2_slices: Some((*done, *total)),
            check_progress: None,
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
        }),
        // PAR2 volumes are written to disk after encoding completes (sequential).
        E::Par2WriteStarted { total } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: Some(UploadPhase::WritingPar2 {
                written: 0,
                total: *total,
            }),
            par2_slices: None,
            check_progress: None,
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
        }),
        E::Par2SliceWritten => {
            let (written, total) = match &previous.phase {
                Some(UploadPhase::WritingPar2 { written, total }) => (written + 1, *total),
                _ => (1, 1),
            };
            let all_written = total > 0 && written >= total;
            Some(ProgressUpdate {
                done_segments: previous.done_segments,
                total_segments: previous.total_segments,
                done_bytes: previous.done_bytes,
                total_bytes: previous.total_bytes,
                current_speed_mbps: previous.current_speed_mbps,
                message: None,
                file_update: None,
                phase: Some(UploadPhase::WritingPar2 { written, total }),
                par2_slices: None,
                check_progress: None,
                queue_extended: None,
                par2_hint_bytes: 0,
                par2_segment_hint: 0,
                par2_complete: all_written,
            })
        }
        // The streaming check queue runs concurrently with the upload rather
        // than as its own phase, so it only updates `check_progress`
        // (rendered as a suffix alongside whatever `phase` already is)
        // instead of replacing `phase` the way the old end-of-run sweep did.
        E::CheckProgress { checked, ok } => {
            let prev_failed = previous.check_progress.map(|(_, f)| f).unwrap_or(0);
            let failed = if *ok { prev_failed } else { prev_failed + 1 };
            Some(ProgressUpdate {
                done_segments: previous.done_segments,
                total_segments: previous.total_segments,
                done_bytes: previous.done_bytes,
                total_bytes: previous.total_bytes,
                current_speed_mbps: previous.current_speed_mbps,
                message: None,
                file_update: None,
                phase: None,
                par2_slices: None,
                check_progress: Some((*checked, failed)),
                queue_extended: None,
                par2_hint_bytes: 0,
                par2_segment_hint: 0,
                par2_complete: false,
            })
        }
        E::CheckDone {
            failed,
            inconclusive,
        } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: None,
            par2_slices: None,
            check_progress: Some((
                previous.check_progress.map(|(c, _)| c).unwrap_or(0),
                *failed + *inconclusive,
            )),
            queue_extended: None,
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
        }),
        E::SegmentDone { file, bytes, ok } => {
            let file_update = FileProgressUpdate {
                name: file.clone(),
                done_segments: 1,
                total_segments: 0,
                done_bytes: *bytes,
                total_bytes: 0,
                ok: *ok,
            };
            Some(ProgressUpdate {
                done_segments: previous.done_segments + 1,
                total_segments: previous.total_segments,
                done_bytes: previous.done_bytes + bytes,
                total_bytes: previous.total_bytes,
                current_speed_mbps: previous.current_speed_mbps,
                message: None,
                file_update: Some(file_update),
                phase: None,
                par2_slices: None,
                check_progress: None,
                queue_extended: None,
                par2_hint_bytes: 0,
                par2_segment_hint: 0,
                par2_complete: false,
            })
        }
        E::QueueExtended {
            segments, bytes, ..
        } => Some(ProgressUpdate {
            done_segments: previous.done_segments,
            total_segments: previous.total_segments,
            done_bytes: previous.done_bytes,
            total_bytes: previous.total_bytes,
            current_speed_mbps: previous.current_speed_mbps,
            message: None,
            file_update: None,
            phase: None,
            par2_slices: None,
            check_progress: None,
            queue_extended: Some((*segments, *bytes)),
            par2_hint_bytes: 0,
            par2_segment_hint: 0,
            par2_complete: false,
        }),
        _ => None,
    }
}
