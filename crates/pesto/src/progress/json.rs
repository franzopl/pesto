//! Newline-delimited JSON progress emitter.

use std::io::Write;

use tokio::task::JoinHandle;

use super::{ProgressEvent, ProgressReceiver, ProgressSender};

/// Spawn a newline-delimited JSON emitter for machine-readable consumers
/// (e.g. `upapasta`).
///
/// Each [`ProgressEvent`] is translated to one JSON object printed to stdout.
/// After the emitter finishes, the caller should print a
/// `{"type":"nzb_written","path":"..."}` event itself once the NZB file has
/// been written. This decouples path resolution from the progress stream.
///
/// Returns the [`ProgressSender`] to hand to the poster and a [`JoinHandle`]
/// the caller must await after posting returns.
pub fn spawn_json_emitter() -> (ProgressSender, JoinHandle<()>) {
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let handle = tokio::spawn(json_emit_loop(rx));
    (tx, handle)
}

async fn json_emit_loop(mut rx: ProgressReceiver) {
    let stdout = std::io::stdout();
    let mut total_segments: u64 = 0;
    let mut done_segments: u64 = 0;
    let mut done_bytes: u64 = 0;
    let mut total_bytes: u64 = 0;
    let mut failures: u64 = 0;
    // PAR2 bytes hint pre-seeded into total_bytes at Started; absorbed as the
    // real PAR2 segments arrive via QueueExtended so total_bytes (and thus
    // progress_pct) never jumps when PAR2 posting starts. Mirrors terminal.rs.
    let mut par2_hint_remaining: u64 = 0;

    loop {
        match rx.recv().await {
            None | Some(ProgressEvent::Finished) => {
                let pct = if total_bytes > 0 {
                    (done_bytes as f64 / total_bytes as f64 * 100.0).min(100.0)
                } else {
                    100.0
                };
                let ok = failures == 0;
                let mut out = stdout.lock();
                let _ = writeln!(
                    out,
                    r#"{{"type":"finished","segments":{done_segments},"failures":{failures},"progress_pct":{pct:.1},"ok":{ok}}}"#
                );
                break;
            }
            Some(ev) => {
                let mut out = stdout.lock();
                match ev {
                    ProgressEvent::Started {
                        files,
                        connections,
                        check_connections,
                        target,
                        par2_bytes_hint,
                        ..
                    } => {
                        let target_json = target
                            .as_deref()
                            .map(|s| format!("\"{}\"", s.replace('"', "\\\"")))
                            .unwrap_or_else(|| "null".to_string());
                        for f in &files {
                            total_segments += f.segments;
                            total_bytes += f.bytes;
                        }
                        // Pre-seed with the PAR2 estimate so progress_pct never
                        // goes backwards once QueueExtended arrives.
                        total_bytes += par2_bytes_hint;
                        par2_hint_remaining = par2_bytes_hint;
                        let _ = writeln!(
                            out,
                            r#"{{"type":"started","total_files":{nf},"total_bytes":{total_bytes},"total_segments":{total_segments},"connections":{connections},"check_connections":{check_connections},"target":{target_json}}}"#,
                            nf = files.len(),
                        );
                    }
                    ProgressEvent::SegmentDone { file, bytes, ok } => {
                        done_segments += 1;
                        done_bytes += bytes;
                        if !ok {
                            failures += 1;
                        }
                        let pct = if total_bytes > 0 {
                            (done_bytes as f64 / total_bytes as f64 * 100.0).min(100.0)
                        } else {
                            0.0
                        };
                        let file_esc = file.replace('"', "\\\"");
                        let _ = writeln!(
                            out,
                            r#"{{"type":"segment_done","file":"{file_esc}","bytes":{bytes},"ok":{ok},"done_segments":{done_segments},"total_segments":{total_segments},"done_bytes":{done_bytes},"total_bytes":{total_bytes},"progress_pct":{pct:.1}}}"#
                        );
                    }
                    ProgressEvent::QueueExtended {
                        file,
                        segments,
                        bytes,
                    } => {
                        total_segments += segments;
                        // Absorb the real PAR2 bytes against the pre-seeded
                        // hint; only grow total_bytes by any excess so the
                        // running percentage doesn't dip.
                        if bytes <= par2_hint_remaining {
                            par2_hint_remaining -= bytes;
                        } else {
                            let excess = bytes - par2_hint_remaining;
                            par2_hint_remaining = 0;
                            total_bytes += excess;
                        }
                        let file_esc = file.replace('"', "\\\"");
                        let _ = writeln!(
                            out,
                            r#"{{"type":"queue_extended","file":"{file_esc}","segments":{segments},"bytes":{bytes},"total_segments":{total_segments},"total_bytes":{total_bytes}}}"#
                        );
                    }
                    ProgressEvent::Status { text } => {
                        let text_esc = text.replace('\\', "\\\\").replace('"', "\\\"");
                        let _ = writeln!(out, r#"{{"type":"status","text":"{text_esc}"}}"#);
                    }
                    ProgressEvent::ProxyStatus { text } => {
                        let text_esc = text.replace('\\', "\\\\").replace('"', "\\\"");
                        let _ = writeln!(out, r#"{{"type":"proxy_status","text":"{text_esc}"}}"#);
                    }
                    ProgressEvent::Failed { description } => {
                        let desc_esc = description.replace('\\', "\\\\").replace('"', "\\\"");
                        let _ = writeln!(out, r#"{{"type":"failed","description":"{desc_esc}"}}"#);
                    }
                    ProgressEvent::Interrupted => {
                        let _ = writeln!(out, r#"{{"type":"interrupted"}}"#);
                    }
                    ProgressEvent::Aborted => {
                        let _ = writeln!(out, r#"{{"type":"aborted"}}"#);
                    }
                    ProgressEvent::Paused => {
                        let _ = writeln!(out, r#"{{"type":"paused"}}"#);
                    }
                    ProgressEvent::Resumed => {
                        let _ = writeln!(out, r#"{{"type":"resumed"}}"#);
                    }
                    ProgressEvent::CompressStarted { total_bytes: tb } => {
                        let _ =
                            writeln!(out, r#"{{"type":"compress_started","total_bytes":{tb}}}"#);
                    }
                    ProgressEvent::CompressProgress { bytes_written } => {
                        let _ = writeln!(
                            out,
                            r#"{{"type":"compress_progress","bytes_written":{bytes_written}}}"#
                        );
                    }
                    ProgressEvent::CompressDone => {
                        let _ = writeln!(out, r#"{{"type":"compress_done"}}"#);
                    }
                    ProgressEvent::Par2EncodeStarted {
                        input_bytes,
                        input_slices,
                        input_files,
                        recovery_slices,
                        slice_size,
                        passes,
                        chunk_size,
                        simd_method,
                        threads,
                        memory_limit,
                    } => {
                        let simd_esc = simd_method.replace('"', "\\\"");
                        let _ = writeln!(
                            out,
                            r#"{{"type":"par2_encode_started","input_bytes":{input_bytes},"input_slices":{input_slices},"input_files":{input_files},"recovery_slices":{recovery_slices},"slice_size":{slice_size},"passes":{passes},"chunk_size":{chunk_size},"simd_method":"{simd_esc}","threads":{threads},"memory_limit":{memory_limit}}}"#
                        );
                    }
                    ProgressEvent::Par2InputProgress { done, total } => {
                        let _ = writeln!(
                            out,
                            r#"{{"type":"par2_encode_progress","done":{done},"total":{total}}}"#
                        );
                    }
                    ProgressEvent::Par2WriteStarted { total } => {
                        let _ = writeln!(out, r#"{{"type":"par2_write_started","total":{total}}}"#);
                    }
                    ProgressEvent::Par2SliceWritten => {
                        let _ = writeln!(out, r#"{{"type":"par2_slice_written"}}"#);
                    }
                    ProgressEvent::CheckProgress { checked, ok } => {
                        let ok_str = if ok { "true" } else { "false" };
                        let _ = writeln!(
                            out,
                            r#"{{"type":"check_progress","checked":{checked},"ok":{ok_str}}}"#
                        );
                    }
                    ProgressEvent::CheckInconclusive { count, reason } => {
                        let reason_esc = reason.replace('\\', "\\\\").replace('"', "\\\"");
                        let _ = writeln!(
                            out,
                            r#"{{"type":"check_inconclusive","count":{count},"reason":"{reason_esc}"}}"#
                        );
                    }
                    ProgressEvent::CheckFastRepost {
                        first_checks,
                        first_misses,
                    } => {
                        let _ = writeln!(
                            out,
                            r#"{{"type":"check_fast_repost","first_checks":{first_checks},"first_misses":{first_misses}}}"#
                        );
                    }
                    ProgressEvent::CheckDone {
                        failed,
                        inconclusive,
                    } => {
                        let _ = writeln!(
                            out,
                            r#"{{"type":"check_done","failed":{failed},"inconclusive":{inconclusive}}}"#
                        );
                    }
                    ProgressEvent::CheckRetrying {
                        attempt,
                        max_attempts,
                        delay_secs,
                        reason,
                    } => {
                        let _ = writeln!(
                            out,
                            r#"{{"type":"check_retrying","attempt":{attempt},"max_attempts":{max_attempts},"delay_secs":{delay_secs},"reason":"{reason}"}}"#
                        );
                    }
                    ProgressEvent::CheckReposted { reposted } => {
                        let _ =
                            writeln!(out, r#"{{"type":"check_reposted","reposted":{reposted}}}"#);
                    }
                    ProgressEvent::CheckRecoverStarted { total } => {
                        let _ =
                            writeln!(out, r#"{{"type":"check_recover_started","total":{total}}}"#);
                    }
                    ProgressEvent::CheckRecoverProgress { done, total, ok } => {
                        let ok_str = if ok { "true" } else { "false" };
                        let _ = writeln!(
                            out,
                            r#"{{"type":"check_recover_progress","done":{done},"total":{total},"ok":{ok_str}}}"#
                        );
                    }
                    ProgressEvent::CheckPoolScaledUp { check_connections } => {
                        let _ = writeln!(
                            out,
                            r#"{{"type":"check_pool_scaled_up","check_connections":{check_connections}}}"#
                        );
                    }
                    // Connection and pool events are noisy and not useful to consumers.
                    ProgressEvent::PostRetryQueued
                    | ProgressEvent::PostRetryRecovered { .. }
                    | ProgressEvent::Par2PassStarted { .. }
                    | ProgressEvent::Par2ComputeStarted { .. }
                    | ProgressEvent::CheckRetryRecovered
                    | ProgressEvent::ConnectionBusy { .. }
                    | ProgressEvent::ConnectionIdle { .. }
                    | ProgressEvent::ConnectionAuth { .. }
                    | ProgressEvent::ConnectionRetrying { .. }
                    | ProgressEvent::CheckConnectionBusy { .. }
                    | ProgressEvent::CheckConnectionIdle { .. }
                    | ProgressEvent::BufferPoolStats { .. }
                    | ProgressEvent::Finished => {}
                }
            }
        }
    }
}
