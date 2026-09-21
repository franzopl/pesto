//! Shared rendering helpers used across screens and overlays.

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::Color,
};

/// Truncate `s` to at most `max` *characters* (not bytes), appending an ellipsis
/// when shortened. Char-safe so names with accents or other multibyte UTF-8
/// (e.g. "Programação") never panic on a non-char-boundary slice.
pub(super) fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let kept: String = s.chars().take(max - 1).collect();
        format!("{kept}…")
    }
}

pub(super) fn format_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.1}GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.0}MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.0}KB", b as f64 / 1024.0)
    } else {
        format!("{}B", b)
    }
}

/// Returns a centered `Rect` with the given percentage of width/height.
pub(super) fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

pub(super) fn category_color(cat: &str) -> Color {
    match cat {
        "Movie" => Color::Magenta,
        "TV" => Color::Blue,
        "Anime" => Color::Yellow,
        _ => Color::DarkGray,
    }
}

#[cfg(test)]
mod tests {
    use super::truncate_str;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate_str("abc", 5), "abc");
        assert_eq!(truncate_str("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_adds_ellipsis_when_too_long() {
        assert_eq!(truncate_str("abcdef", 4), "abc\u{2026}");
    }

    #[test]
    fn truncate_is_char_safe_with_multibyte() {
        // Must not panic on a multibyte boundary (regression: vault crash on
        // "Programa\u{e7}\u{e3}o"-style names).
        let s = "Programa\u{e7}\u{e3}o_e_IA";
        let out = truncate_str(s, 10);
        assert!(out.chars().count() <= 10);
        assert!(out.ends_with('\u{2026}'));
    }
}
