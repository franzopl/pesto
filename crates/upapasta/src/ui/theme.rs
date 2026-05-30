//! Central visual language for the TUI.
//!
//! One place to define the palette and glyphs so every screen reads as a single
//! coherent app instead of a patchwork of per-widget color choices. Prefer these
//! over hard-coded `Color::*` in the draw functions.

use ratatui::style::{Color, Modifier, Style};

// ── Palette ─────────────────────────────────────────────────────────────────

/// Brand / primary accent (header, focused panel borders).
pub const ACCENT: Color = Color::Cyan;
/// Selection and "you are editing this" highlight.
pub const FOCUS: Color = Color::Yellow;
/// Secondary text: field labels, hints, inactive elements.
pub const MUTED: Color = Color::DarkGray;
/// Directories in listings.
pub const DIR: Color = Color::Blue;

// Semantic states — used for status glyphs, gauges and badges.
pub const OK: Color = Color::Green;
pub const ERR: Color = Color::Red;
pub const ACTIVE: Color = Color::Cyan;

// ── Glyphs (all single-width, terminal-safe — no emoji) ───────────────────────

/// Prefix marker for a directory row (paired with a blank marker for files so
/// columns stay aligned regardless of type).
pub const DIR_MARK: &str = "▸ ";
pub const FILE_MARK: &str = "  ";

/// Status glyphs shared by the queue and per-file progress views.
pub const ST_ACTIVE: &str = "▶";
pub const ST_DONE: &str = "✓";
pub const ST_FAILED: &str = "✗";
pub const ST_PENDING: &str = "○";
pub const ST_PENDING_RUN: &str = "·";

// ── Style helpers ─────────────────────────────────────────────────────────────

/// Label / secondary-text style.
pub fn label() -> Style {
    Style::default().fg(MUTED)
}

/// List highlight (selected line), shared across every list screen so the
/// cursor looks the same everywhere.
pub fn highlight() -> Style {
    Style::default()
        .fg(Color::Black)
        .bg(FOCUS)
        .add_modifier(Modifier::BOLD)
}

/// `(glyph, color)` for a per-item upload status, used by the queue and the
/// per-file progress views. `running` distinguishes a pending item during an
/// active upload (a dot) from a pending item at rest (an open circle).
pub fn status_glyph(status: crate::app::FileStatus, running: bool) -> (&'static str, Color) {
    use crate::app::FileStatus;
    match status {
        FileStatus::Active => (ST_ACTIVE, ACTIVE),
        FileStatus::Done => (ST_DONE, OK),
        FileStatus::Failed => (ST_FAILED, ERR),
        FileStatus::Pending if running => (ST_PENDING_RUN, MUTED),
        FileStatus::Pending => (ST_PENDING, MUTED),
    }
}
