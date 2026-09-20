use crate::progress::RunMode;
use crate::ui::format::{format_size, inconclusive_label};
use crate::ui::render::{ansi, format_duration, terminal_width, visible_len, wrap};
use crate::ui::state::RenderState;
use std::io::Write;

impl RenderState {
    /// The compact run summary that replaces the live panel once the run is
    /// over. A recovered retry gets its own optional third line so the primary
    /// success message stays readable at ordinary terminal widths. The binary
    /// follows it with the `wrote
    /// nzb`/`wrote nfo` paths, so this covers only what the renderer itself
    /// knows — outcome, throughput and verification.
    pub(super) fn summary_lines(&self, width: usize) -> Vec<String> {
        let unresolved = self.failures + self.check_failed + self.check_inconclusive;
        let ok = unresolved == 0 && !self.interrupted && self.failed_description.is_none();
        let glyph = if self.interrupted || self.failed_description.is_some() {
            ansi("⚠", "33")
        } else if ok {
            ansi("✓", "32")
        } else {
            ansi("✗", "31")
        };

        let mut lines = match self.mode {
            RunMode::Post if self.interrupted || self.failed_description.is_some() => vec![
                format!("{glyph} Upload stopped before completion."),
                format!(
                    "  {}/{} articles accepted · elapsed {}",
                    self.accepted_articles(),
                    self.total_segments,
                    format_duration(self.elapsed_secs())
                ),
            ],
            RunMode::Post if ok && self.checks_enabled => vec![
                format!(
                    "{glyph} Upload complete — {}/{} articles confirmed",
                    self.confirmed_articles(),
                    self.total_segments
                ),
                format!(
                    "  {} in {} · avg {}/s",
                    format_size(self.done_bytes),
                    format_duration(self.elapsed_secs()),
                    format_size((self.done_bytes as f64 / self.elapsed_secs()) as u64)
                ),
            ],
            RunMode::Post if ok => vec![
                format!(
                    "{glyph} Upload complete — {}/{} articles accepted by the server",
                    self.accepted_articles(),
                    self.total_segments
                ),
                format!(
                    "  Not independently verified · elapsed {}",
                    format_duration(self.elapsed_secs())
                ),
            ],
            RunMode::Post => {
                let count_note = if self.checks_enabled {
                    format!(
                        "{}/{} articles uploaded; {}/{} confirmed available",
                        self.accepted_articles(),
                        self.total_segments,
                        self.confirmed_articles(),
                        self.accepted_articles()
                    )
                } else {
                    format!(
                        "{}/{} articles accepted",
                        self.accepted_articles(),
                        self.total_segments
                    )
                };
                vec![
                    format!(
                        "{glyph} Upload incomplete — {count_note}; {}.",
                        ansi(
                            &format!(
                                "{unresolved} unresolved {}",
                                if unresolved == 1 {
                                    "failure"
                                } else {
                                    "failures"
                                }
                            ),
                            "31"
                        )
                    ),
                    "  Retry with --resume; use -v or the session log for server details."
                        .to_string(),
                ]
            }
            RunMode::DryRun => vec![
                format!(
                    "{glyph} Dry run complete — {}/{} articles prepared.",
                    self.accepted_articles(),
                    self.total_segments
                ),
                format!("  elapsed {}", format_duration(self.elapsed_secs())),
            ],
            RunMode::Par2Only => vec![
                format!(
                    "{glyph} PAR2 generation complete — {}/{} source segments processed.",
                    self.accepted_articles(),
                    self.total_segments
                ),
                format!("  elapsed {}", format_duration(self.elapsed_secs())),
            ],
        };

        if ok && self.mode == RunMode::Post {
            let recovered = self.recovered_retries();
            if recovered > 0 {
                lines.push(format!(
                    "  {}",
                    ansi(
                        &format!(
                            "↻ Recovered automatically after {recovered} temporary {}",
                            if recovered == 1 { "retry" } else { "retries" }
                        ),
                        "33"
                    )
                ));
            }
        }

        if self.check_inconclusive > 0 {
            lines.push(format!(
                "  {}",
                ansi(&inconclusive_label(self.check_inconclusive), "33")
            ));
        }
        if let Some(desc) = &self.failed_description {
            lines.push(format!("  {}", ansi(&format!("⚠ {desc}"), "33")));
        }
        lines
            .into_iter()
            .flat_map(|line| {
                if visible_len(&line) <= width {
                    vec![line]
                } else {
                    wrap(&line, width)
                }
            })
            .collect()
    }

    /// Erase the live panel and print the compact final summary in its place.
    pub(super) fn draw_summary(&mut self) {
        if !self.started {
            return;
        }
        let width = terminal_width().unwrap_or(80).max(20);
        let mut out = String::new();
        // Same erase dance as `draw_panel` — move to the top of the last
        // frame and clear downward before writing the (shorter) summary.
        if self.lines_drawn > 0 {
            out.push_str(&format!("\x1b[{}A\r", self.lines_drawn));
        }
        out.push_str("\x1b[0J");
        for line in self.summary_lines(width) {
            out.push_str(&line);
            out.push('\n');
        }
        self.lines_drawn = 0;

        let mut err = std::io::stderr().lock();
        let _ = err.write_all(out.as_bytes());
        let _ = err.flush();
    }
}
