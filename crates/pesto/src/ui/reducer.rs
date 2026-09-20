use crate::progress::ProgressEvent;
use crate::ui::state::{ConnState, RenderState};
use std::time::{Duration, Instant};

impl RenderState {
    pub(super) fn apply(&mut self, ev: ProgressEvent) {
        match ev {
            ProgressEvent::Started {
                mode,
                files,
                connections,
                check_connections,
                target,
                par2_bytes_hint,
                par2_segments_hint,
            } => {
                self.started = true;
                self.mode = mode;
                self.target = target;
                self.start = Instant::now();
                self.check_connections = check_connections;
                self.checks_enabled = check_connections > 0;
                self.conn_files = vec![None; connections];
                self.conn_state = vec![ConnState::Idle; connections];
                self.check_conn_state = vec![ConnState::Idle; check_connections];
                for f in files {
                    self.total_segments += f.segments;
                    self.total_bytes += f.bytes;
                    self.files.insert(f.name, (0, f.segments));
                }
                // Pre-seed totals with the exact PAR2 geometry so neither
                // bar jumps when QueueExtended arrives with the real files.
                self.total_bytes += par2_bytes_hint;
                self.par2_hint_remaining = par2_bytes_hint;
                self.total_segments += par2_segments_hint;
                self.par2_segment_hint_remaining = par2_segments_hint;
            }
            ProgressEvent::ConnectionBusy { conn, file } => {
                if let Some(slot) = self.conn_files.get_mut(conn) {
                    *slot = Some(file);
                }
                if let Some(s) = self.conn_state.get_mut(conn) {
                    *s = ConnState::Busy;
                }
            }
            ProgressEvent::ConnectionIdle { conn } => {
                if let Some(slot) = self.conn_files.get_mut(conn) {
                    *slot = None;
                }
                if let Some(s) = self.conn_state.get_mut(conn) {
                    *s = ConnState::Idle;
                }
            }
            ProgressEvent::ConnectionAuth { conn } => {
                if let Some(s) = self.conn_state.get_mut(conn) {
                    *s = ConnState::Auth;
                }
            }
            ProgressEvent::ConnectionRetrying { conn } => {
                if let Some(s) = self.conn_state.get_mut(conn) {
                    *s = ConnState::Retrying;
                }
            }
            ProgressEvent::BufferPoolStats { total, free } => {
                self.buf_total = total;
                self.buf_free = free;
            }
            ProgressEvent::SegmentDone { file, bytes, ok } => {
                self.done_segments += 1;
                self.done_bytes += bytes;
                if !ok {
                    self.failures += 1;
                    self.post_retry_pending += 1;
                    // Legacy/manual streams may omit PostRetryQueued; clear
                    // the just-received segment detail once the typed
                    // SegmentDone outcome proves it is rescue-eligible.
                    self.failed_description.take();
                }
                if let Some(entry) = self.files.get_mut(&file) {
                    entry.0 += 1;
                }
            }
            ProgressEvent::QueueExtended {
                file,
                segments,
                bytes,
            } => {
                self.posting_par2 = true;
                // Absorb the real bytes/segments against the pre-seeded hints
                // so neither total jumps. If the real PAR2 geometry somehow
                // differs from the hint (slice-size/count overrides changing
                // between the estimate and the run shouldn't happen, but stay
                // defensive), only the excess grows the total.
                if bytes <= self.par2_hint_remaining {
                    self.par2_hint_remaining -= bytes;
                    // total_bytes already includes this; no change needed.
                } else {
                    let excess = bytes - self.par2_hint_remaining;
                    self.par2_hint_remaining = 0;
                    self.total_bytes += excess;
                }
                if segments <= self.par2_segment_hint_remaining {
                    self.par2_segment_hint_remaining -= segments;
                } else {
                    let excess = segments - self.par2_segment_hint_remaining;
                    self.par2_segment_hint_remaining = 0;
                    self.total_segments += excess;
                }
                self.files.entry(file).or_insert((0, 0)).1 += segments;
            }
            ProgressEvent::Status { text } => {
                let display_text = if text.starts_with("retry:") {
                    "Temporarily retried an article; continuing upload".to_string()
                } else if text.starts_with("check: reposted ") {
                    "Temporarily retried an article; confirming availability".to_string()
                } else {
                    text
                };
                if display_text.is_empty() {
                    self.status_since = None;
                } else if self.status.is_empty() || self.status != display_text {
                    self.status_since = Some(Instant::now());
                }
                self.status = display_text;
            }
            ProgressEvent::ProxyStatus { text } => {
                self.proxy_status = Some(text);
            }
            ProgressEvent::PostRetryQueued => {
                self.post_retry_queued_events += 1;
            }
            ProgressEvent::Failed { description } => {
                if self.post_retry_queued_events > 0 {
                    self.post_retry_queued_events -= 1;
                } else {
                    self.failed_description = Some(description);
                }
            }
            ProgressEvent::PostRetryRecovered {
                count,
                previously_failed,
            } => {
                self.recovered_post_retries += count;
                if previously_failed {
                    self.failures = self.failures.saturating_sub(count);
                    self.post_retry_pending = self.post_retry_pending.saturating_sub(count);
                }
            }
            ProgressEvent::Interrupted => self.interrupted = true,
            ProgressEvent::Aborted => {
                self.interrupted = true;
                self.aborted = true;
            }
            // No CLI flag drives external_pause yet (embedder-only, see
            // ROADMAP.new.md Phase 2) — reuse the existing status line so a
            // future embedder-triggered pause still renders sensibly here.
            ProgressEvent::Paused => {
                self.status = "paused".to_string();
                self.status_since = Some(Instant::now());
            }
            ProgressEvent::Resumed => {
                self.status = String::new();
                self.status_since = None;
            }
            ProgressEvent::Finished => self.finished = true,
            ProgressEvent::CompressStarted { total_bytes } => {
                self.compress_active = true;
                self.compress_total = total_bytes;
                self.compress_written = 0;
                self.compress_start = Instant::now();
                self.started = true; // allow panel to draw before posting starts
            }
            ProgressEvent::CompressProgress { bytes_written } => {
                self.compress_written = bytes_written;
            }
            ProgressEvent::CompressDone => {
                self.compress_written = self.compress_total;
                self.compress_active = false;
            }
            // Only the slice counts drive the panel. The rest of the encoder
            // geometry (slice size, chunking, SIMD path, memory budget) used
            // to be printed as a six-line block here; it is diagnostic detail
            // rather than progress, and `poster` already logs the same
            // numbers under `-v` ("PAR2 geometry" / "RS encoder").
            ProgressEvent::Par2EncodeStarted {
                input_slices,
                recovery_slices,
                passes,
                ..
            } => {
                self.par2_encode_total = input_slices;
                self.par2_encode_done = 0;
                self.par2_encode_start = Instant::now();
                self.prev_par2_encode_done = 0;
                self.par2_encode_rate_ema = 0.0;
                // Learn the write phase's size now so the combined bar has a
                // stable denominator before `Par2WriteStarted` arrives.
                self.par2_recovery_total = recovery_slices;
                self.par2_passes = passes.max(1);
                self.par2_pass_index = 0;
                self.par2_explicit_pass = false;
                self.par2_compute_active = false;
                self.par2_writing_active = false;
                self.started = true;
            }
            ProgressEvent::Par2PassStarted { pass, passes } => {
                self.par2_passes = passes.max(1);
                self.par2_pass_index = pass.saturating_sub(1).min(self.par2_passes - 1);
                self.par2_encode_done = 0;
                self.par2_explicit_pass = true;
                self.par2_compute_active = false;
                self.par2_writing_active = false;
                self.started = true;
            }
            ProgressEvent::Par2ComputeStarted { pass, passes } => {
                self.par2_passes = passes.max(1);
                self.par2_pass_index = pass.saturating_sub(1).min(self.par2_passes - 1);
                self.par2_compute_active = true;
                self.par2_writing_active = false;
                self.started = true;
            }
            ProgressEvent::Par2InputProgress { done, total } => {
                // Keep the legacy rollover inference for embedding callers
                // that do not emit the additive Par2PassStarted signal.
                if !self.par2_explicit_pass && done < self.par2_encode_done {
                    let next = (self.par2_pass_index + 1).min(self.par2_passes.saturating_sub(1));
                    if next > self.par2_pass_index {
                        self.par2_pass_index = next;
                        self.par2_encode_done = done;
                    }
                } else if self.par2_explicit_pass {
                    self.par2_encode_done = self.par2_encode_done.max(done);
                } else {
                    self.par2_encode_done = done;
                }
                self.par2_compute_active = false;
                self.par2_encode_total = total;
            }
            ProgressEvent::Par2WriteStarted { total } => {
                self.par2_write_active = true;
                self.par2_write_total = total;
                self.par2_write_done = 0;
                self.par2_write_start = Instant::now();
                self.prev_par2_write_done = 0;
                self.par2_write_rate_ema = 0.0;
            }
            ProgressEvent::Par2SliceWritten => {
                self.par2_compute_active = false;
                self.par2_writing_active = true;
                self.par2_write_done = self.par2_write_done.saturating_add(1);
                if self.par2_write_done >= self.par2_write_total {
                    self.par2_write_active = false;
                    self.par2_writing_active = false;
                }
            }
            ProgressEvent::CheckProgress { checked, ok } => {
                if !self.check_active {
                    // Lazy start: the streaming check queue has no fixed
                    // total known upfront, so it just starts showing up the
                    // first time an article gets resolved, concurrently
                    // with the upload panel above it.
                    self.started = true;
                    self.check_active = true;
                    self.check_start = Instant::now();
                }
                self.check_checked = checked;
                if !ok {
                    self.check_failed += 1;
                }
            }
            ProgressEvent::CheckInconclusive { count, .. } => {
                if !self.check_active {
                    self.started = true;
                    self.check_active = true;
                    self.check_start = Instant::now();
                }
                if count > self.check_inconclusive {
                    self.check_checked = self
                        .check_checked
                        .saturating_add(count - self.check_inconclusive);
                }
                self.check_inconclusive = count;
            }
            ProgressEvent::CheckFastRepost {
                first_checks,
                first_misses,
            } => {
                if !self.check_active {
                    self.started = true;
                    self.check_active = true;
                    self.check_start = Instant::now();
                }
                self.check_fast_repost = Some((first_checks, first_misses));
            }
            ProgressEvent::CheckRetrying {
                attempt,
                max_attempts,
                delay_secs,
                reason: _,
            } => {
                self.check_retry = Some((
                    format!("temporarily retrying availability check ({attempt}/{max_attempts})"),
                    Instant::now() + Duration::from_secs(delay_secs),
                ));
            }
            ProgressEvent::CheckReposted { reposted } => {
                self.check_reposted = reposted;
            }
            ProgressEvent::CheckRetryRecovered => {
                self.recovered_check_retries += 1;
            }
            ProgressEvent::CheckDone {
                failed,
                inconclusive,
            } => {
                self.check_active = false;
                self.check_failed = failed;
                if inconclusive > self.check_inconclusive {
                    self.check_checked = self
                        .check_checked
                        .saturating_add(inconclusive - self.check_inconclusive);
                }
                self.check_inconclusive = inconclusive;
                self.check_retry = None;
            }
            ProgressEvent::CheckRecoverStarted { total } => {
                self.recover_active = true;
                self.recover_done = 0;
                self.recover_total = total;
                self.recover_failed = 0;
                self.recover_start = Instant::now();
            }
            ProgressEvent::CheckRecoverProgress { done, total, ok } => {
                self.recover_done = done;
                self.recover_total = total;
                if ok {
                    // This article was part of `check_failed`'s count from
                    // `CheckDone` (that's how it ended up in this batch);
                    // now that it's confirmed, the "missing" tally and the
                    // final summary line must reflect that instead of
                    // freezing on the pre-recovery number. Also counts
                    // toward `check_reposted` — the streaming coordinator's
                    // own counter never sees recovery-pass reposts, since
                    // `recover_missing` runs standalone, after that
                    // coordinator has already shut down.
                    self.check_failed = self.check_failed.saturating_sub(1);
                    self.check_reposted += 1;
                    self.recovered_check_retries += 1;
                } else {
                    self.recover_failed += 1;
                }
                if done >= total {
                    self.recover_active = false;
                }
            }
            ProgressEvent::CheckConnectionBusy { conn } => {
                if let Some(s) = self.check_conn_state.get_mut(conn) {
                    *s = ConnState::Busy;
                }
            }
            ProgressEvent::CheckConnectionIdle { conn } => {
                if let Some(s) = self.check_conn_state.get_mut(conn) {
                    *s = ConnState::Idle;
                }
            }
            ProgressEvent::CheckPoolScaledUp { check_connections } => {
                self.check_connections = check_connections;
                self.check_conn_state
                    .resize(check_connections, ConnState::Idle);
            }
        }
    }
}
