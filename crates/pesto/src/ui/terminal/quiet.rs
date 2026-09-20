use crate::progress::RunMode;
use crate::ui::format::inconclusive_label;
use crate::ui::render::{format_duration, terminal_width, truncate};
use crate::ui::state::RenderState;
use std::io::Write;

impl RenderState {
    /// Draw quiet single-line mode (phase 21f).
    pub(super) fn draw_quiet(&mut self, final_draw: bool) {
        if !self.started {
            return;
        }
        const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let spinner = if final_draw {
            // Reserve the red-style cross for unresolved upload/availability
            // failures. Cancellation remains a warning rather than looking
            // like a server rejected data.
            if self.failures > 0 || self.check_failed > 0 || self.check_inconclusive > 0 {
                '✗'
            } else if self.interrupted || self.failed_description.is_some() {
                '⚠'
            } else {
                '✓'
            }
        } else {
            let ch = SPINNER[self.spinner_frame % SPINNER.len()];
            self.spinner_frame += 1;
            ch
        };

        let pct = (self.progress_frac() * 100.0).round() as u64;

        let eta_str = if final_draw {
            let recovered = self.recovered_retries();
            let recovered_note = if recovered > 0 {
                format!(
                    " · {recovered} {} recovered automatically",
                    if recovered == 1 { "retry" } else { "retries" }
                )
            } else {
                String::new()
            };
            if self.failures + self.check_failed > 0 {
                let inconclusive_note = if self.check_inconclusive > 0 {
                    format!(" · {}", inconclusive_label(self.check_inconclusive))
                } else {
                    String::new()
                };
                format!(
                    "upload incomplete · {} unresolved{inconclusive_note}",
                    self.failures + self.check_failed
                )
            } else if self.mode == RunMode::Post && self.checks_enabled {
                format!(
                    "complete · {}/{} confirmed{recovered_note}",
                    self.confirmed_articles(),
                    self.total_segments
                )
            } else if self.mode == RunMode::Post {
                format!(
                    "complete · {}/{} accepted · not verified{recovered_note}",
                    self.accepted_articles(),
                    self.total_segments
                )
            } else {
                format!("done {}", format_duration(self.elapsed_secs()))
            }
        } else if self.recover_active {
            // `pct` is already 100% here (it tracks upload segments, and the
            // recovery pass only starts once every one of those is done),
            // so without this the quiet line reads "100% · ETA —" for
            // however long the sequential recovery batch takes — visually
            // indistinguishable from a hang.
            format!(
                "recovering availability {}/{}",
                self.recover_done, self.recover_total
            )
        } else if self.par2_compute_active {
            format!(
                "computing recovery data · pass {}/{}",
                self.par2_pass_index + 1,
                self.par2_passes
            )
        } else if self.par2_writing_active {
            format!(
                "writing recovery volumes · pass {}/{}",
                self.par2_pass_index + 1,
                self.par2_passes
            )
        } else if self.par2_encode_units_done() < self.par2_encode_units_total() {
            format!(
                "reading sources · pass {}/{}",
                self.par2_pass_index + 1,
                self.par2_passes
            )
        } else if let Some((secs, unstable)) = self.overall_eta_secs() {
            let mark = if unstable { "~" } else { "" };
            format!("ETA {mark}{}", format_duration(secs))
        } else {
            "ETA —".to_string()
        };

        let width = terminal_width().unwrap_or(80).max(20);
        let line = truncate(&format!("{spinner}  {pct:>3}% · {eta_str}"), width);

        let mut out = String::new();
        if self.lines_drawn > 0 {
            // Cursor Previous Line (`\x1b[nF`) is supposed to move up *and*
            // return to column 1, but several terminals (some SSH clients,
            // minimal emulators) implement only plain Cursor Up (`\x1b[nA`),
            // leaving the column wherever it was. Do the column reset
            // ourselves with a literal `\r` — a bare control character every
            // terminal honours — instead of relying on `F`'s column-reset
            // semantics. Without this, a terminal that only supports `A`
            // never returns to column 0, so each redraw's text lands right
            // after the previous one instead of overwriting it.
            out.push_str("\x1b[1A\r\x1b[2K");
        }
        out.push_str(&line);
        out.push('\n');
        self.lines_drawn = 1;

        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }
}
