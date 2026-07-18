//! Generic terminal-rendering primitives shared by every progress panel in
//! the workspace: bars, sparklines, ANSI-aware width math, and the box-drawing
//! frame. Nothing here knows about posting, downloading, or any specific
//! [`crate::progress::ProgressEvent`] — that's what keeps it reusable by
//! `penne`'s own renderer as well as `pesto`'s (see `CLAUDE.md`: "shared
//! types... live in `pesto`... `penne` reuses them").

// Eight sub-character blocks from narrowest to fullest. `pub(crate)` since
// `ui::terminal::render_dual_bar` (posting-specific two-band bar) also needs
// direct access to render its own fractional leading edge the same way.
pub(crate) const SUBCHAR: [char; 8] = ['▏', '▎', '▍', '▌', '▋', '▊', '▉', '█'];

/// Draw a smooth proportional bar using sub-character block rendering.
///
/// The fractional leading cell uses one of `▏▎▍▌▋▊▉█` so the bar moves
/// continuously instead of jumping whole-cell steps.
pub fn render_bar(frac: f64, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let total_eighths = (frac.clamp(0.0, 1.0) * width as f64 * 8.0).round() as usize;
    let full_blocks = (total_eighths / 8).min(width);
    let remainder = total_eighths % 8;

    let mut s = String::with_capacity(width * 3); // UTF-8 multi-byte
    for _ in 0..full_blocks {
        s.push('█');
    }
    if full_blocks < width {
        if remainder > 0 {
            s.push(SUBCHAR[remainder - 1]);
            for _ in 0..width - full_blocks - 1 {
                s.push('░');
            }
        } else {
            for _ in 0..width - full_blocks {
                s.push('░');
            }
        }
    }
    s
}

// Nine-level sparkline characters.
const SPARK: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// Render a sparkline string from a slice of f64 speed samples.
pub fn render_sparkline(samples: &[f64]) -> String {
    if samples.is_empty() {
        return String::new();
    }
    let max = samples.iter().cloned().fold(0.0_f64, f64::max);
    if max < 1.0 {
        return SPARK[0].to_string().repeat(samples.len());
    }
    samples
        .iter()
        .map(|&v| {
            let idx = ((v / max) * 8.0).round() as usize;
            SPARK[idx.min(8)]
        })
        .collect()
}

/// Returns the terminal column count queried via `TIOCGWINSZ`, or `None` on
/// non-TTY fds or if the query fails.
#[cfg(unix)]
pub fn terminal_width() -> Option<usize> {
    use std::os::fd::AsRawFd;

    #[repr(C)]
    struct Winsize {
        ws_row: u16,
        ws_col: u16,
        ws_xpixel: u16,
        ws_ypixel: u16,
    }

    extern "C" {
        fn ioctl(
            fd: std::ffi::c_int,
            request: std::ffi::c_ulong,
            out: *mut Winsize,
        ) -> std::ffi::c_int;
    }

    // TIOCGWINSZ: Linux = 0x5413, macOS = 0x40087468
    #[cfg(target_os = "linux")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x5413;
    #[cfg(target_os = "macos")]
    const TIOCGWINSZ: std::ffi::c_ulong = 0x4008_7468;

    let mut ws = Winsize {
        ws_row: 0,
        ws_col: 0,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // Safety: ioctl(TIOCGWINSZ) writes exactly sizeof(Winsize) bytes into `ws`.
    let ret = unsafe { ioctl(std::io::stderr().as_raw_fd(), TIOCGWINSZ, &mut ws) };
    if ret == 0 && ws.ws_col > 0 {
        Some(ws.ws_col as usize)
    } else {
        None
    }
}

#[cfg(not(unix))]
pub fn terminal_width() -> Option<usize> {
    None
}

/// Returns true when ANSI colour should be used (TTY + NO_COLOR not set).
pub fn use_color() -> bool {
    use std::io::IsTerminal;
    std::io::stderr().is_terminal() && std::env::var("NO_COLOR").is_err()
}

/// Wrap `s` in the given ANSI SGR codes, or return `s` unchanged when colours
/// are disabled.
pub fn ansi(s: &str, code: &str) -> String {
    if use_color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Count of *visible* characters in `s`, skipping ANSI SGR escape sequences
/// (`\x1b[...m`) so width math reflects what's actually drawn on screen —
/// see [`ansi`], whose invisible colour codes would otherwise inflate a plain
/// `.chars().count()` and desync box borders from coloured content.
pub fn visible_len(s: &str) -> usize {
    let mut len = 0;
    let mut in_escape = false;
    for c in s.chars() {
        if in_escape {
            if c == 'm' {
                in_escape = false;
            }
        } else if c == '\x1b' {
            in_escape = true;
        } else {
            len += 1;
        }
    }
    len
}

/// Pad `s` with spaces (or truncate it) to exactly `width` *visible*
/// characters. ANSI escape sequences pass through untouched and don't count
/// against `width`.
pub fn pad(s: &str, width: usize) -> String {
    let s = truncate(s, width);
    let len = visible_len(&s);
    let mut out = String::with_capacity(s.len() + width.saturating_sub(len));
    out.push_str(&s);
    for _ in 0..width.saturating_sub(len) {
        out.push(' ');
    }
    out
}

/// Truncate `s` to at most `width` *visible* characters, marking a cut with
/// `…`. ANSI escape sequences are preserved rather than being sliced
/// through; if truncation cuts off before a colour's closing `\x1b[0m`, a
/// reset is appended so colour never bleeds past the truncated line.
pub fn truncate(s: &str, width: usize) -> String {
    if visible_len(s) <= width {
        return s.to_string();
    }
    let budget = width.saturating_sub(1);
    let mut out = String::new();
    let mut visible = 0;
    let mut escape_buf = String::new();
    let mut in_escape = false;
    let mut color_active = false;
    for c in s.chars() {
        if in_escape {
            escape_buf.push(c);
            if c == 'm' {
                in_escape = false;
                color_active = escape_buf != "\x1b[0m";
                out.push_str(&escape_buf);
                escape_buf.clear();
            }
            continue;
        }
        if c == '\x1b' {
            in_escape = true;
            escape_buf.push(c);
            continue;
        }
        if visible >= budget {
            break;
        }
        out.push(c);
        visible += 1;
    }
    out.push('…');
    if color_active {
        out.push_str("\x1b[0m");
    }
    out
}

/// Format a duration in seconds as `m:ss` (or `h:mm:ss` past an hour).
pub fn format_duration(secs: f64) -> String {
    let total = secs.round() as u64;
    let (h, m, s) = (total / 3600, (total % 3600) / 60, total % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Top border of a labelled box: `┌─ {label} {dashes}┐`, sized so the whole
/// line is `width + 2` visible characters wide (matching [`box_bottom`] and
/// a `width`-wide [`box_line`] interior).
pub fn box_top(label: &str, width: usize) -> String {
    let dashes = width
        .saturating_sub(1)
        .saturating_sub(label.chars().count());
    format!("┌─ {label} {}┐", "─".repeat(dashes))
}

/// Bottom border of a labelled box, matching [`box_top`]'s width.
pub fn box_bottom(width: usize) -> String {
    format!("└{}┘", "─".repeat(width + 2))
}

/// Format a `│ … │` box content line, padding/truncating to the interior
/// `width`.
pub fn box_line(body: &str, width: usize) -> String {
    format!("│ {} │", pad(body, width))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A line wider than the terminal wraps onto a second physical row that
    // the cursor-up-by-logical-line-count redraw logic in `ui::terminal`
    // doesn't know about, corrupting every subsequent redraw. The fix is to
    // always truncate to the detected width before writing, so these tests
    // pin the invariant that makes that safe: truncated output never exceeds
    // the requested visible width, with or without embedded ANSI colour
    // codes.
    #[test]
    fn truncate_never_exceeds_requested_width_plain() {
        let long =
            "pesto  posting data  1 file(s) → usnews.blocknews.net + news.newshosting.com · 0:12";
        for width in [10, 20, 40, 60, 80] {
            let out = truncate(long, width);
            assert!(
                visible_len(&out) <= width,
                "width={width} produced {} visible chars: {out:?}",
                visible_len(&out)
            );
        }
    }

    #[test]
    fn truncate_never_exceeds_requested_width_with_ansi_colour() {
        // Mirrors the posting panel's header: a coloured phase word embedded
        // in otherwise plain text. Hardcoded escape (rather than calling
        // `ansi()`, which no-ops when stderr isn't a TTY — as under `cargo
        // test`) so this actually exercises the escape-aware branch.
        let long = "pesto  \x1b[36mwriting PAR2\x1b[0m  1 file(s) → usnews.blocknews.net + news.newshosting.com · 0:12";
        for width in [10, 20, 40, 60, 80] {
            let out = truncate(long, width);
            assert!(
                visible_len(&out) <= width,
                "width={width} produced {} visible chars: {out:?}",
                visible_len(&out)
            );
        }
    }

    #[test]
    fn truncate_leaves_short_lines_untouched() {
        assert_eq!(truncate("short", 80), "short");
    }

    #[test]
    fn box_top_matches_previous_hand_written_dash_counts() {
        // These pin the exact output the old hand-rolled call sites in
        // `ui::terminal` produced (`BODY_W + 2 - 14/9/8`), so the refactor
        // to a shared helper is verified behaviour-preserving.
        const BODY_W: usize = 56;
        assert_eq!(
            box_top("compressing", BODY_W),
            format!("┌─ compressing {}┐", "─".repeat(44))
        );
        assert_eq!(
            box_top("upload", BODY_W),
            format!("┌─ upload {}┐", "─".repeat(49))
        );
        assert_eq!(
            box_top("check", BODY_W),
            format!("┌─ check {}┐", "─".repeat(50))
        );
    }
}
