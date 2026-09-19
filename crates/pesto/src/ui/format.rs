//! Pure formatting policy used by the terminal progress renderer.

use super::render::{ansi, truncate, visible_len, wrap, SUBCHAR};

/// Bounds on the panel box interior width. The width itself tracks the
/// terminal (see [`body_width`]) instead of being fixed: a 50-column window
/// used to have every box border truncated away by the `truncate` call in
/// [`RenderState::draw_panel`], and a 120-column one left half the screen
/// empty.
pub(super) const MIN_BODY_W: usize = 24;
const MAX_BODY_W: usize = 100;

/// Interior width of the panel boxes on a terminal `width` columns wide.
///
/// [`box_top`], [`box_line`] and [`box_bottom`] all render `body + 4` visible
/// columns (`│ ` … ` │`), so this is just the terminal minus that frame.
pub(super) fn body_width(width: usize) -> usize {
    width.saturating_sub(4).clamp(MIN_BODY_W, MAX_BODY_W)
}

/// Bar width for a given box interior, proportional so the figures to the
/// bar's right keep their room on a narrow terminal. The ratio reproduces the
/// 26-in-56 bar the fixed-width panel used.
pub(super) fn bar_width(body_w: usize) -> usize {
    (body_w * 46 / 100).clamp(10, 40)
}

/// Maximum physical lines a wrapped status/failure note may occupy. Bounds
/// the panel's height against a status text with no natural upper size (raw
/// hook output, a long list of file names) — past this many lines the
/// remainder is dropped with a trailing `…` on the last line rather than
/// letting one bad status push the rest of the panel off screen.
const STATUS_MAX_LINES: usize = 4;

/// Remove SGR color sequences from a summary reused by append-only plain mode.
pub(super) fn strip_ansi_for_plain(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\x1b' && chars.peek() == Some(&'[') {
            chars.next();
            for code in chars.by_ref() {
                if code == 'm' {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

/// Word-wrap `text` (via [`wrap`]) into panel lines prefixed with `marker` on
/// the first line and matching indentation on the rest, capped at
/// [`STATUS_MAX_LINES`]. See the call site in `panel_lines` for why this
/// exists: a `truncate`-to-one-line status used to silently cut long,
/// information-dense messages in half with no way to read the rest.
pub(super) fn wrapped_note(marker: &str, text: &str, width: usize) -> Vec<String> {
    let marker_w = visible_len(marker);
    let wrap_width = width.saturating_sub(marker_w).max(1);
    let mut lines = wrap(text, wrap_width);
    let overflowed = lines.len() > STATUS_MAX_LINES;
    lines.truncate(STATUS_MAX_LINES);
    if overflowed {
        if let Some(last) = lines.last_mut() {
            *last = truncate(&format!("{last}…"), wrap_width);
        }
    }
    let indent = " ".repeat(marker_w);
    lines
        .into_iter()
        .enumerate()
        .map(|(i, line)| format!("{}{line}", if i == 0 { marker } else { &indent }))
        .collect()
}

// Colours for the two upload-bar bands: the leading edge (segments posted,
// not yet check-confirmed) and the trailing edge (segments the streaming
// check queue has already resolved). Kept distinct from every other colour
// in use (see the `ansi(` call sites above) so the two meanings never blur
// into an existing one.
pub(super) const UPLOAD_BAND_COLOR: &str = "32"; // green — posting data
pub(super) const CHECK_BAND_COLOR: &str = "34"; // blue — STAT-confirmed

/// Draw a two-colour proportional bar: a `checked_frac` portion in
/// [`CHECK_BAND_COLOR`] (how much of the upload the streaming check queue has
/// already confirmed), followed by the rest of the `total_frac` portion in
/// [`UPLOAD_BAND_COLOR`] (posted but not yet confirmed), followed by the
/// unfilled `░` remainder — unchanged from [`render_bar`], so the bar's empty
/// state looks exactly as it always has.
///
/// `checked_frac` is always `<= total_frac` in practice (the check queue can
/// only confirm what has already been posted); this is enforced defensively
/// with `clamp` so an out-of-order event can never render a checked band
/// past the upload band's own leading edge.
pub(super) fn render_dual_bar(checked_frac: f64, total_frac: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let checked_frac = checked_frac.clamp(0.0, 1.0);
    let total_frac = total_frac.clamp(checked_frac, 1.0);

    // The checked/upload split renders at whole-cell granularity; only the
    // outer (upload) leading edge gets the smooth sub-character treatment,
    // exactly like the single-colour bar.
    let checked_cells = ((checked_frac * width as f64).round() as usize).min(width);

    let total_eighths = (total_frac * width as f64 * 8.0).round() as usize;
    let total_full = (total_eighths / 8).min(width).max(checked_cells);
    let remainder = if total_full < width {
        total_eighths % 8
    } else {
        0
    };

    let mut checked_part = String::with_capacity(checked_cells * 3);
    for _ in 0..checked_cells {
        checked_part.push('█');
    }

    let mut upload_part = String::with_capacity((total_full - checked_cells + 1) * 3);
    for _ in checked_cells..total_full {
        upload_part.push('█');
    }
    if remainder > 0 {
        upload_part.push(SUBCHAR[remainder - 1]);
    }

    let filled_cells = if remainder > 0 {
        total_full + 1
    } else {
        total_full
    };
    let pending_cells = width.saturating_sub(filled_cells);

    let mut s = String::new();
    if !checked_part.is_empty() {
        s.push_str(&ansi(&checked_part, CHECK_BAND_COLOR));
    }
    if !upload_part.is_empty() {
        s.push_str(&ansi(&upload_part, UPLOAD_BAND_COLOR));
    }
    for _ in 0..pending_cells {
        s.push('░');
    }
    s
}

/// Inconclusive STAT-path failures: not a confirmed 430 gap.
pub(super) fn inconclusive_label(count: u64) -> String {
    format!("{count} inconclusive (check path failed — not a confirmed gap)")
}

pub(super) fn fast_repost_label(first_checks: u64, first_misses: u64) -> String {
    let pct = (first_misses as f64 / first_checks.max(1) as f64 * 100.0).round() as u64;
    format!("fast-repost: isolated miss (miss rate {pct}% of {first_checks} first checks)")
}

/// Human-readable byte size with binary (IEC) units.
pub(super) fn format_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_width_keeps_the_historical_ratio() {
        assert_eq!(bar_width(56), 25);
        assert!(bar_width(24) >= 10);
        assert!(bar_width(MAX_BODY_W) <= 40);
    }
}
