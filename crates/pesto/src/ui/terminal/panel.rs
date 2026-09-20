use crate::progress::RunMode;
use crate::ui::format::{
    bar_width, body_width, fast_repost_label, format_size, inconclusive_label, render_dual_bar,
    wrapped_note, CHECK_BAND_COLOR, UPLOAD_BAND_COLOR,
};
use crate::ui::render::{
    ansi, box_bottom, box_line, box_top, format_duration, render_bar, render_sparkline,
    terminal_width, truncate,
};
use crate::ui::state::{ConnState, RenderState};
use std::io::Write;
use std::time::Instant;

/// Above this connection count the per-connection grid is replaced by a
/// one-line summary, so the panel never grows unbounded.
const GRID_LIMIT: usize = 12;

impl RenderState {
    // ---- TTY panel rendering --------------------------------------------

    /// Read RSS and CPU usage from /proc/self (Linux only; no-op on other OS).
    fn poll_proc_stats(&mut self) {
        #[cfg(target_os = "linux")]
        {
            // RSS from /proc/self/status  →  VmRSS: N kB
            if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
                for line in status.lines() {
                    if let Some(rest) = line.strip_prefix("VmRSS:") {
                        if let Some(kb_str) = rest.split_whitespace().next() {
                            if let Ok(kb) = kb_str.parse::<u64>() {
                                self.proc_rss_bytes = kb * 1024;
                            }
                        }
                        break;
                    }
                }
            }
            // CPU from /proc/self/stat  →  field 14 (utime) + field 15 (stime)
            if let Ok(stat) = std::fs::read_to_string("/proc/self/stat") {
                // Skip past the comm field which may contain spaces inside parens.
                let after_comm = stat.rfind(')').map(|i| &stat[i + 2..]).unwrap_or("");
                let fields: Vec<&str> = after_comm.split_whitespace().collect();
                // Fields are 0-indexed from after comm; utime is index 11, stime 12.
                if fields.len() > 12 {
                    let utime: u64 = fields[11].parse().unwrap_or(0);
                    let stime: u64 = fields[12].parse().unwrap_or(0);
                    let ticks = utime + stime;
                    let now = Instant::now();
                    let elapsed = now
                        .duration_since(self.proc_prev_tick_time)
                        .as_secs_f64()
                        .max(0.001);
                    let clk_tck: f64 = 100.0; // sysconf(_SC_CLK_TCK) is 100 on Linux
                    let delta_ticks = ticks.saturating_sub(self.proc_prev_ticks) as f64;
                    self.proc_cpu_pct = (delta_ticks / clk_tck / elapsed * 100.0).min(9999.0);
                    self.proc_prev_ticks = ticks;
                    self.proc_prev_tick_time = now;
                }
            }
        }
    }

    /// Clear a retry countdown once its deadline has passed, so the panel
    /// reverts to the normal "elapsed" line instead of freezing on "in 0s".
    pub(super) fn expire_check_retry(&mut self) {
        if let Some((_, deadline)) = &self.check_retry {
            if Instant::now() >= *deadline {
                self.check_retry = None;
            }
        }
    }

    pub(super) fn draw_panel(&mut self, final_draw: bool) {
        if !self.started {
            return;
        }
        // Record a speed sample for sparkline + ETA confidence (phase 21c/21d).
        let current_bps =
            self.done_bytes.saturating_sub(self.prev_done_bytes) as f64 * (1000.0 / 200.0); // per-tick delta → bytes/s (200 ms tick)
        self.prev_done_bytes = self.done_bytes;
        if !final_draw && self.done_bytes > 0 {
            self.push_speed_sample(current_bps);
        }
        if !final_draw {
            self.update_par2_rate_emas();
        }
        if !final_draw && self.verbose {
            self.poll_proc_stats();
        }
        self.expire_check_retry();
        // Every emitted line must fit within one physical terminal row, or
        // it wraps onto a second row the redraw logic below doesn't know
        // about: cursor-up moves by *logical* line count, so once any line
        // wraps, moving up N logical lines undershoots the true top of the
        // previous frame, leaving a stray fragment behind on every redraw
        // (reported as the header line — the one line not bounded by the
        // fixed-width box below it — repeating itself over and over on
        // narrow terminals, e.g. a phone SSH client). Truncating every line
        // to the actual detected width keeps the 1-logical-line-per-row
        // invariant the cursor arithmetic below depends on.
        let width = terminal_width().unwrap_or(80).max(20);
        let lines: Vec<String> = self
            .panel_lines(final_draw, width)
            .into_iter()
            .map(|l| truncate(&l, width))
            .collect();

        let mut out = String::new();
        // Move the cursor back to the top of the previous panel and wipe
        // everything below it, so a shorter panel leaves no stale lines.
        //
        // Cursor Previous Line (`\x1b[nF`) is supposed to move up *and*
        // return to column 1, but several terminals (some SSH clients,
        // minimal emulators) implement only plain Cursor Up (`\x1b[nA`),
        // leaving the column wherever the previous draw left it. Do the
        // column reset ourselves with a literal `\r` instead of relying on
        // `F`'s column-reset semantics — without this, a terminal that only
        // supports `A` never returns to column 0, so `\x1b[0J`'s erase
        // starts mid-line and every redraw's text lands right after the
        // previous one instead of overwriting it (looks like the header
        // line repeating itself dozens of times instead of updating).
        if self.lines_drawn > 0 {
            out.push_str(&format!("\x1b[{}A\r", self.lines_drawn));
        }
        out.push_str("\x1b[0J");
        for line in &lines {
            out.push_str(line);
            out.push('\n');
        }
        self.lines_drawn = lines.len();

        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }

    /// Build the panel for a terminal `width` columns wide. Every box sizes
    /// itself off that width rather than a compile-time constant.
    pub(super) fn panel_lines(&self, final_draw: bool, width: usize) -> Vec<String> {
        let mut lines = Vec::new();
        let body_w = body_width(width);
        let bar_w = bar_width(body_w);

        // --- header with phase indicator and elapsed time ----------------
        let elapsed_hdr = if self.started {
            format!(" · {}", format_duration(self.elapsed_secs()))
        } else {
            String::new()
        };
        // The header names the phase that dominates the *remaining* work, in
        // priority order. Ordering matters: uploads and PAR2 encoding run
        // concurrently, so once the upload bar is full the label must move on
        // to whatever is actually still running (the streaming check, then any
        // trailing PAR2 write) instead of freezing on "posting PAR2" while the
        // connections sit idle — the old chain did exactly that, and also lied
        // with "writing PAR2" during the data pass just because a (0/N) write
        // phase had been announced.
        let uploading = !self.files.is_empty() && self.done_segments < self.total_segments;
        let phase = if self.compress_active && self.files.is_empty() {
            ansi("compressing", "33") // amber
        } else if uploading && self.posting_par2 {
            ansi("uploading PAR2", "35")
        } else if uploading {
            ansi("uploading", "32")
        } else if self.check_active {
            ansi("verifying availability", "34")
        } else if self.recover_active {
            ansi("recovering availability", "33")
        } else if self.par2_compute_active {
            ansi("computing recovery data", "36")
        } else if self.par2_writing_active {
            ansi("writing recovery volumes", "36")
        } else if self.par2_encode_units_done() < self.par2_encode_units_total() {
            ansi("reading sources", "36")
        } else if self.par2_write_active || (!self.files.is_empty() && !self.finished) {
            ansi("writing PAR2", "36")
        } else if self.finished {
            ansi("done", "32")
        } else {
            "starting".to_string()
        };
        let verb_suffix = match self.mode {
            RunMode::Post => String::new(),
            RunMode::DryRun => format!(" · {}", ansi("dry run", "33")),
            RunMode::Par2Only => format!(" · {}", ansi("par2 only", "36")),
        };
        let file_count = self.files.len();
        let target_str = self
            .target
            .as_deref()
            .map(|t| format!(" → {t}"))
            .unwrap_or_default();
        lines.push(format!(
            "pesto  {phase}  {file_count} file(s){target_str}{verb_suffix}{elapsed_hdr}"
        ));

        // Keep proxy routing visible for the entire run instead of sharing the
        // transient status line used by retries and preparation phases.
        if let Some(proxy_status) = &self.proxy_status {
            lines.push(box_top("proxy", body_w));
            lines.push(box_line(&ansi(proxy_status, "36"), body_w));
            lines.push(box_bottom(body_w));
        }

        // --- compression box (shown while compressing) -------------------
        if self.compress_active || (final_draw && self.compress_total > 0 && self.files.is_empty())
        {
            let elapsed = self.compress_start.elapsed().as_secs_f64().max(0.001);
            let frac = if self.compress_total > 0 {
                (self.compress_written as f64 / self.compress_total as f64).clamp(0.0, 1.0)
            } else {
                0.0
            };
            let pct = (frac * 100.0).round() as u64;
            let bar = render_bar(frac, bar_w);
            let rate = self.compress_written as f64 / elapsed;
            let line1 = format!(
                "[{bar}] {pct:>3}%  {}/{}",
                format_size(self.compress_written),
                format_size(self.compress_total),
            );
            let eta = if final_draw {
                format!("elapsed {}", format_duration(elapsed))
            } else if rate > 1.0 && self.compress_total > self.compress_written {
                let remaining = (self.compress_total - self.compress_written) as f64 / rate;
                format!("ETA {}", format_duration(remaining))
            } else {
                "ETA —".to_string()
            };
            let line2 = format!("{}/s · {eta}", format_size(rate as u64));
            lines.push(box_top("compressing", body_w));
            lines.push(box_line(&line1, body_w));
            lines.push(box_line(&line2, body_w));
            lines.push(box_bottom(body_w));
        }

        // --- overall posting box (only after posting has started) --------
        if !self.files.is_empty() {
            // Segment ratio, shared with `-q` — see `progress_frac`.
            let frac = self.progress_frac();
            let pct = (frac * 100.0).round() as u64;
            // Trailing band: how much of the plan the streaming check queue
            // has already confirmed, on the same 0..=total_segments scale as
            // the upload's own leading edge — see `render_dual_bar`.
            let checked_frac = if self.total_segments > 0 {
                (self.check_checked as f64 / self.total_segments as f64).clamp(0.0, frac)
            } else {
                0.0
            };
            let bar = render_dual_bar(checked_frac, frac, bar_w);
            // Line 1: bar + percentage + segment count
            let line1 = format!(
                "[{bar}] {pct:>3}%  {}/{} seg",
                self.done_segments, self.total_segments
            );
            let rate = self.rate();
            let (line2, line3) = if final_draw {
                // On final draw: show total size, average speed, and elapsed time.
                // Average speed is more meaningful than the last-tick instantaneous rate.
                let avg_speed = if self.elapsed_secs() > 0.001 {
                    self.done_bytes as f64 / self.elapsed_secs()
                } else {
                    0.0
                };
                let summary = format!(
                    "{} · avg {}/s · elapsed {}",
                    format_size(self.done_bytes),
                    format_size(avg_speed as u64),
                    format_duration(self.elapsed_secs()),
                );
                (summary, None)
            } else {
                // While uploading: bytes/total · instantaneous speed · sparkline
                let spark = {
                    let samples = self.speed_samples();
                    // Suppress sparkline on narrow terminals (< 60 columns) to
                    // avoid truncating the speed/size figures that matter more.
                    let wide_enough = width >= 60;
                    if samples.len() >= 2 && wide_enough {
                        format!(" {}", render_sparkline(&samples))
                    } else {
                        String::new()
                    }
                };
                let l2 = format!(
                    "{}/{} · {}/s{}",
                    format_size(self.done_bytes),
                    format_size(self.total_bytes),
                    format_size(rate as u64),
                    spark,
                );
                let l3 = match self.overall_eta_secs() {
                    Some((secs, unstable)) => {
                        let mark = if unstable { "~" } else { "" };
                        format!("ETA {mark}{}", format_duration(secs))
                    }
                    None => "ETA —".to_string(),
                };
                (l2, Some(l3))
            };
            lines.push(box_top("upload", body_w));
            lines.push(box_line(&line1, body_w));
            lines.push(box_line(&line2, body_w));
            if let Some(l3) = line3 {
                lines.push(box_line(&l3, body_w));
            }
            lines.push(box_bottom(body_w));

            // --- streaming check queue box ---------------------------------
            // Runs concurrently with the upload above (its own dedicated
            // connections, started a few seconds after the first segment
            // posts). It has no bar of its own on purpose: the upload bar
            // already paints check progress as its trailing blue band (see
            // `render_dual_bar`), so a second bar here just duplicated it.
            // This box carries the numbers that band can't — the running
            // verified / pending / missing / reposted tally.
            if self.check_active || (final_draw && self.check_checked > 0) {
                let verified = self
                    .check_checked
                    .saturating_sub(self.check_failed)
                    .saturating_sub(self.check_inconclusive);
                let pending = self.done_segments.saturating_sub(self.check_checked);
                // Colour-match the upload bar's two bands: "verified" in the
                // checked band's colour, "pending" in the upload band's —
                // a legend for the bar that lives right next to the numbers
                // it explains instead of a separate line or header colouring.
                let verified_str =
                    ansi(&format!("{verified} confirmed available"), CHECK_BAND_COLOR);
                let pending_str = ansi(
                    &format!("{pending} pending confirmation"),
                    UPLOAD_BAND_COLOR,
                );
                let mut line1 = if self.check_failed > 0 || self.check_inconclusive > 0 {
                    let mut extra = String::new();
                    if self.check_failed > 0 {
                        extra.push_str(&format!(
                            " · {}",
                            ansi(&format!("{} missing", self.check_failed), "31")
                        ));
                    }
                    if self.check_inconclusive > 0 {
                        extra.push_str(&format!(
                            " · {}",
                            ansi(&format!("{} inconclusive", self.check_inconclusive), "33")
                        ));
                    }
                    format!("{verified_str} · {pending_str}{extra}")
                } else if !self.check_active {
                    format!("{verified_str} · all confirmed available")
                } else {
                    format!("{verified_str} · {pending_str}")
                };
                // Reposts had no counter at all before — the only trace was
                // a `Status` line shared (and instantly overwritten) by
                // every other kind of status message in the app.
                let retries_awaiting_confirmation = self
                    .check_reposted
                    .saturating_sub(self.recovered_check_retries);
                if retries_awaiting_confirmation > 0 {
                    line1.push_str(&format!(
                        " · {}",
                        ansi(
                            &format!(
                                "{retries_awaiting_confirmation} {} awaiting confirmation",
                                if retries_awaiting_confirmation == 1 {
                                    "retry"
                                } else {
                                    "retries"
                                }
                            ),
                            "33",
                        )
                    ));
                }
                // No ETA here: the check's throughput jumps once the upload
                // frees its connections, so any estimate would swing wildly
                // (see `overall_eta_secs`). Elapsed time is honest and steady.
                let elapsed = self.check_start.elapsed().as_secs_f64().max(0.001);
                let line2 = if let Some((label, deadline)) = &self.check_retry {
                    let remaining = deadline.saturating_duration_since(Instant::now()).as_secs();
                    format!("{label} in {remaining}s")
                } else {
                    format!("elapsed {}", format_duration(elapsed))
                };
                lines.push(box_top("availability", body_w));
                lines.push(box_line(&line1, body_w));
                lines.push(box_line(&line2, body_w));
                if self.check_inconclusive > 0 {
                    lines.push(box_line(
                        &ansi(&inconclusive_label(self.check_inconclusive), "33"),
                        body_w,
                    ));
                }
                if let Some((n, m)) = self.check_fast_repost {
                    lines.push(box_line(&fast_repost_label(n, m), body_w));
                }
                lines.push(box_bottom(body_w));
            }

            // --- final recovery pass box ------------------------------------
            // Only ever active once `check_active` above has already gone
            // false (see `poster::post_files_inner`: the streaming check
            // queue fully drains, *then* this bounded pass runs on whatever
            // small tail is left) — the two boxes never show at the same
            // time. Has its own bar (unlike the check box) because, unlike
            // the streaming check, this pass has a known, fixed total up
            // front (`CheckRecoverStarted`), so a real percentage is
            // meaningful here.
            if self.recover_active || (final_draw && self.recover_total > 0) {
                let frac = if self.recover_total > 0 {
                    (self.recover_done as f64 / self.recover_total as f64).clamp(0.0, 1.0)
                } else {
                    0.0
                };
                let pct = (frac * 100.0).round() as u64;
                let bar = render_bar(frac, bar_w);
                let line1 = format!(
                    "[{bar}] {pct:>3}%  {}/{} article(s)",
                    self.recover_done, self.recover_total
                );
                let ok_count = self.recover_done.saturating_sub(self.recover_failed);
                let mut line2 = ansi(&format!("{ok_count} recovered"), "33");
                if self.recover_failed > 0 {
                    line2.push_str(&format!(
                        " · {}",
                        ansi(&format!("{} still missing", self.recover_failed), "31")
                    ));
                }
                let elapsed = self.recover_start.elapsed().as_secs_f64().max(0.001);
                line2.push_str(&format!(" · elapsed {}", format_duration(elapsed)));
                lines.push(box_top("recover", body_w));
                lines.push(box_line(&line1, body_w));
                lines.push(box_line(&line2, body_w));
                lines.push(box_bottom(body_w));
            }

            // --- per-connection activity as a single dot row --------------
            // Was a two-per-line grid of `conn N ▸ file` cells: up to six rows
            // that, since every worker posts the *same* file, repeated one
            // truncated name N times and then sat as N rows of `idle` for the
            // whole check phase. A row of state-coloured dots carries the same
            // information (how many busy / retrying / idle) in one fixed line,
            // and stops the panel's height from lurching as workers drain.
            let conns = self.conn_files.len();
            if conns > 0 {
                let total_conns = conns + self.check_connections;
                let active = self.conn_files.iter().filter(|c| c.is_some()).count();
                let retrying = self
                    .conn_state
                    .iter()
                    .filter(|&&s| s == ConnState::Retrying)
                    .count();
                // Only draw an individual dot per connection while the count
                // stays legible; above that a plain tally reads better than a
                // wall of dots that would wrap or get truncated.
                let dots = if conns <= GRID_LIMIT {
                    let mut s = String::from("  ");
                    for st in &self.conn_state {
                        s.push_str(&conn_dot(*st));
                    }
                    s
                } else {
                    String::new()
                };
                // Real activity, not just the configured pool size: a check
                // pool sitting entirely on 20-second STAT-retry backoffs
                // used to look identical to one actively working through a
                // backlog — both just said "N check".
                let check_str = if self.check_connections > 0 {
                    let check_active = self
                        .check_conn_state
                        .iter()
                        .filter(|&&s| s == ConnState::Busy)
                        .count();
                    let check_dots = if self.check_connections <= GRID_LIMIT {
                        let mut s = String::from(" ");
                        for st in &self.check_conn_state {
                            s.push_str(&conn_dot(*st));
                        }
                        s
                    } else {
                        String::new()
                    };
                    format!(
                        " ·{check_dots} {check_active}/{} check",
                        self.check_connections
                    )
                } else {
                    String::new()
                };
                let retry_str = if retrying > 0 {
                    format!(
                        " · {}",
                        ansi(&format!("{retrying} temporarily retrying"), "33")
                    )
                } else {
                    String::new()
                };
                lines.push(format!(
                    "conns{dots}  {total_conns} total · {active}/{conns} active{check_str}{retry_str}"
                ));
            }

            // --- file tally + failures -----------------------------------
            let (done, in_flight) = self.file_tally();
            let total_files = self.files.len();
            let pending = total_files.saturating_sub(done + in_flight);
            // Show the name of the file currently being uploaded (first busy conn).
            let active_file = self
                .conn_files
                .iter()
                .find_map(|f| f.as_deref())
                .map(|name| format!("  ▸ {}", truncate(name, 28)))
                .unwrap_or_default();
            let failures_str = if self.post_retry_pending > 0 {
                format!(
                    "  {}",
                    ansi(
                        &format!("{} temporarily retrying", self.post_retry_pending),
                        "33"
                    )
                )
            } else {
                String::new()
            };
            lines.push(format!(
                "files  done {done}/{total_files}  uploading {in_flight}  waiting {pending}{failures_str}{active_file}"
            ));
            // --- buffer pool visualizer (phase 21h, shown under pressure) -
            if self.buf_total > 0 && self.buf_free * 4 < self.buf_total {
                let frac_free = self.buf_free as f64 / self.buf_total as f64;
                let bar = render_bar(1.0 - frac_free, 10);
                let buf_line = format!(
                    "buf [{bar}] {}/{} used",
                    self.buf_total - self.buf_free,
                    self.buf_total,
                );
                lines.push(ansi(&buf_line, "33")); // yellow when under pressure
            }
        }

        // --- PAR2 secondary indicator (encode + write as one bar) ---------
        // Shown below the upload box as a single dim, indented line so the
        // upload bar stays the focal point. Encode and write were two separate
        // free-floating bars with inconsistent grammar (only encode had a
        // percentage) and fixed non-responsive widths; to the user, though,
        // "generate PAR2" is one activity with two internal stages that run
        // essentially back to back. One monotonic bar over the combined slice
        // work — with the active stage named inline — reads as that single
        // activity, keeps the panel a fixed height, and matches the responsive
        // `[bar] pct%` grammar of the boxes above. No ETA here: its remaining
        // time already feeds the single overall ETA (`overall_eta_secs`).
        // Encode work is counted over every pass (see `par2_encode_units_*`),
        // so a memory-constrained multi-pass encode — which re-reads the whole
        // input per pass — advances the bar smoothly instead of resetting it
        // to zero once per re-read.
        let encode_done = self.par2_encode_units_done();
        let encode_total = self.par2_encode_units_total();
        let par2_total = encode_total + self.par2_recovery_total;
        let par2_done = encode_done + self.par2_write_done as usize;
        if !final_draw && par2_total > 0 && par2_done < par2_total {
            let frac = (par2_done as f64 / par2_total as f64).clamp(0.0, 1.0);
            // Floor, not round: this line only renders while PAR2 work is
            // still outstanding, so a rounded 99.8% displaying as "100%"
            // would claim the stage is done while slices are still being
            // written (seen as `100%  write 258/273`).
            let pct = (frac * 100.0).floor() as u64;
            let bar = render_bar(frac, bar_w);
            // Name the stage actually running. Encode is the long pole and
            // completes before writing meaningfully starts, so prefer it while
            // it is still going; otherwise report the write flush.
            let stage = if encode_done < encode_total {
                // With several passes the per-pass counter alone is confusing
                // ("encode 12/655" three separate times), so name the pass.
                let pass_note = if self.par2_passes > 1 {
                    format!(" — pass {}/{}", self.par2_pass_index + 1, self.par2_passes)
                } else {
                    String::new()
                };
                if self.par2_compute_active {
                    format!("Computing recovery data{pass_note}")
                } else if self.par2_writing_active {
                    format!(
                        "Writing recovery volumes{pass_note} · {}/{} slices",
                        self.par2_write_done, self.par2_write_total
                    )
                } else {
                    format!(
                        "Reading sources{pass_note} · {}/{} slices",
                        self.par2_encode_done, self.par2_encode_total
                    )
                }
            } else {
                format!(
                    "Writing recovery volumes · {}/{}",
                    self.par2_write_done, self.par2_write_total
                )
            };
            lines.push(ansi(&format!("  par2  [{bar}] {pct:>3}%  {stage}"), "2"));
        }

        // --- process resource stats (Linux /proc/self, `-v` only) --------
        #[cfg(target_os = "linux")]
        if self.verbose && self.proc_rss_bytes > 0 {
            let rss = format_size(self.proc_rss_bytes);
            let cpu = format!("{:.1}%", self.proc_cpu_pct);
            let res_line = format!("process  ram {}  cpu {}", rss, cpu);
            lines.push(ansi(&res_line, "2")); // dim — informational, not critical
        }

        // --- optional status / interrupt / failure note -------------------
        // Both branches below can carry information-dense free text (a
        // long filename plus a network error message; a multi-clause status
        // like the PAR2 memory-budget banner) that easily exceeds one
        // terminal row. `truncate`-ing it to fit used to just cut the back
        // half off with a bare "…" and no way to read the rest. Word-wrap
        // it across a few lines instead — bounded by `STATUS_MAX_LINES` so
        // a genuinely unbounded value (arbitrary hook output, a long list
        // of file names) still can't make the panel grow without limit.
        if let Some(desc) = &self.failed_description {
            lines.extend(wrapped_note("⚠ ", desc, width));
        } else if self.aborted {
            lines.push("⚠ abort — dropping connections, saving resume state".to_string());
        } else if self.interrupted {
            lines.push(
                "⚠ interrupt — finishing in-flight articles · Ctrl+C again to abort".to_string(),
            );
        } else if !self.status.is_empty() {
            let elapsed_str = if let Some(since) = self.status_since {
                format!(" · {}", format_duration(since.elapsed().as_secs_f64()))
            } else {
                String::new()
            };
            let full = format!("{}{}", self.status, elapsed_str);
            let marker = if self.status.starts_with("Temporarily retried") {
                ansi("⚠ ", "33")
            } else {
                "▸ ".to_string()
            };
            lines.extend(wrapped_note(&marker, &full, width));
        }

        lines
    }
}

/// One cell of the connection grid with colour-coded state (phase 21b).
/// One state-coloured dot for the connection row: filled `●` when the worker
/// is doing something (busy/auth/retrying), hollow dim `○` when idle.
fn conn_dot(state: ConnState) -> String {
    match state {
        ConnState::Busy => ansi("●", "32"),     // green — posting
        ConnState::Auth => ansi("●", "33"),     // yellow — authenticating
        ConnState::Retrying => ansi("●", "33"), // amber — temporarily retrying
        ConnState::Idle => ansi("○", "2"),      // dim — drained
    }
}
