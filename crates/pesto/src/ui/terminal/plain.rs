use crate::ui::format::{fast_repost_label, format_size, inconclusive_label, strip_ansi_for_plain};
use crate::ui::state::RenderState;
use std::io::Write;
use std::time::Instant;

impl RenderState {
    // ---- non-TTY plain rendering ----------------------------------------

    pub(super) fn draw_plain(&mut self, final_draw: bool) {
        if !self.started {
            return;
        }
        // Print once, right when the run starts — upload and check pools are
        // separate connection sets (see `split_connections`), so a single
        // combined number would hide how many of each are actually in use.
        if !self.plain_connections_printed {
            self.plain_connections_printed = true;
            let conns = self.conn_files.len();
            if conns > 0 || self.check_connections > 0 {
                let check_str = if self.check_connections > 0 {
                    format!(" · {} check", self.check_connections)
                } else {
                    String::new()
                };
                let total_conns = conns + self.check_connections;
                let mut err = std::io::stderr().lock();
                let _ = writeln!(
                    err,
                    "connections: {total_conns} total ({conns} upload{check_str})"
                );
                let _ = err.flush();
            }
        }
        // Throttle to roughly one line every ~2s so logs stay readable.
        self.plain_ticks += 1;
        if !final_draw && !self.plain_ticks.is_multiple_of(10) {
            return;
        }
        self.expire_check_retry();
        let mut err = std::io::stderr().lock();

        // Print each new `status` line exactly once, ahead of the
        // phase-specific branches below — several of those `return` early,
        // which used to mean a `Status` event (e.g. the memory-budget
        // banner, or "PAR2 recovery data split into N passes") never made it
        // into redirected/logged output at all.
        if !self.status.is_empty() && self.status != self.plain_status_printed {
            let _ = writeln!(err, "{}", self.status);
            let _ = err.flush();
            self.plain_status_printed = self.status.clone();
        }

        if !self.plain_failed_printed {
            if let Some(desc) = &self.failed_description {
                self.plain_failed_printed = true;
                let _ = writeln!(err, "⚠ {desc}");
                let _ = err.flush();
            }
        }

        if self.compress_active {
            let elapsed = self.compress_start.elapsed().as_secs_f64().max(0.001);
            let rate = self.compress_written as f64 / elapsed;
            let _ = writeln!(
                err,
                "compressing: {}/{} · {}/s",
                format_size(self.compress_written),
                format_size(self.compress_total),
                format_size(rate as u64),
            );
            let _ = err.flush();
            return;
        }

        if self.par2_compute_active {
            let _ = writeln!(
                err,
                "PAR2: computing recovery data — pass {}/{}",
                self.par2_pass_index + 1,
                self.par2_passes,
            );
            let _ = err.flush();
            return;
        }

        if self.par2_writing_active {
            let _ = writeln!(
                err,
                "PAR2: writing recovery volumes — pass {}/{} · {}/{} slices",
                self.par2_pass_index + 1,
                self.par2_passes,
                self.par2_write_done,
                self.par2_write_total,
            );
            let _ = err.flush();
            return;
        }

        if self.par2_encode_units_done() < self.par2_encode_units_total() {
            let _ = writeln!(
                err,
                "PAR2: Reading sources — pass {}/{} · {}/{} slices",
                self.par2_pass_index + 1,
                self.par2_passes,
                self.par2_encode_done,
                self.par2_encode_total,
            );
            let _ = err.flush();
            return;
        }

        if self.par2_write_active {
            let _ = writeln!(
                err,
                "PAR2: writing recovery volumes · {}/{} slices",
                self.par2_write_done, self.par2_write_total,
            );
            let _ = err.flush();
            return;
        }

        // Final recovery pass — see the TTY panel's "recover" box. Its own
        // early-return branch for the same reason as `compress_active`/
        // `par2_write_active` above: without one, this sequential,
        // potentially multi-minute pass produced no plain-mode output at all
        // once the streaming check's `check_active` suffix below stopped
        // applying (`CheckDone` already fired by the time this starts).
        if self.recover_active {
            let ok_count = self.recover_done.saturating_sub(self.recover_failed);
            let missing_str = if self.recover_failed > 0 {
                format!(" · {} still missing", self.recover_failed)
            } else {
                String::new()
            };
            let _ = writeln!(
                err,
                "recovering availability: {}/{} article(s) · {ok_count} recovered{missing_str}",
                self.recover_done, self.recover_total,
            );
            let _ = err.flush();
            return;
        }

        // Streaming check runs concurrently with the upload, so it's an
        // extra suffix on the normal progress line rather than a phase that
        // suppresses it.
        let check_suffix = if self.check_active || (final_draw && self.check_checked > 0) {
            let verified = self
                .check_checked
                .saturating_sub(self.check_failed)
                .saturating_sub(self.check_inconclusive);
            let pending = self.done_segments.saturating_sub(self.check_checked);
            let mut suffix = format!(
                " · availability: {verified} confirmed/{} missing/{} inconclusive/{pending} pending",
                self.check_failed, self.check_inconclusive
            );
            let retries_awaiting_confirmation = self
                .check_reposted
                .saturating_sub(self.recovered_check_retries);
            if retries_awaiting_confirmation > 0 {
                suffix.push_str(&format!(
                    "/{retries_awaiting_confirmation} awaiting confirmation"
                ));
            }
            // Unlike the boxed panel, plain mode has no dedicated retry line
            // — append it here so a connection-error backoff isn't silently
            // invisible when output is redirected/logged (non-TTY).
            if let Some((label, deadline)) = &self.check_retry {
                let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
                suffix.push_str(&format!(" · {label} in {remaining}s"));
            }
            if self.check_inconclusive > 0 {
                suffix.push_str(&format!(
                    " · {}",
                    inconclusive_label(self.check_inconclusive)
                ));
            }
            if let Some((n, m)) = self.check_fast_repost {
                suffix.push_str(&format!(" · {}", fast_repost_label(n, m)));
            }
            suffix
        } else {
            String::new()
        };

        if self.files.is_empty() {
            if !check_suffix.is_empty() {
                let _ = writeln!(err, "check:{check_suffix}");
                let _ = err.flush();
            }
            return;
        }

        let rate = self.rate();
        if final_draw {
            for line in self.summary_lines(usize::MAX) {
                let _ = writeln!(err, "{}", strip_ansi_for_plain(&line));
            }
        } else {
            let transient = if self.post_retry_pending > 0 {
                format!(" · {} temporarily retrying", self.post_retry_pending)
            } else {
                String::new()
            };
            let _ = writeln!(
                err,
                "uploading: {}/{} articles · {} · {}/s{transient}{check_suffix}",
                self.done_segments,
                self.total_segments,
                format_size(self.done_bytes),
                format_size(rate as u64),
            );
        }
        let _ = err.flush();
    }
}
