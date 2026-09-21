use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

use crate::app::NzbOrigin;
use crate::catalog::NzbStatusEntry;

use super::{FileTree, NzbBadge};

impl FileTree {
    /// Render this FileTree into the given area.
    pub fn render(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        // Keep visible_height in sync so navigation knows how many rows fit.
        self.visible_height = (area.height as usize).saturating_sub(2).max(1);
        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let badge = self.badge_for(path);
                let is_dir = path.is_dir();
                let raw_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                // Directories carry a single-width marker + trailing slash so
                // they read at a glance without relying on (double-width) emoji.
                let marker = if is_dir {
                    crate::ui::theme::DIR_MARK
                } else {
                    crate::ui::theme::FILE_MARK
                };
                let name = if is_dir {
                    format!("{raw_name}/")
                } else {
                    raw_name.to_string()
                };
                let is_selected = i == self.selected;

                let (check, check_style, name_style) = match &badge {
                    NzbBadge::Marked => (
                        "[x] ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                        if is_selected {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Green)
                        },
                    ),
                    NzbBadge::Uploading => (
                        "[▶] ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Cyan),
                    ),
                    NzbBadge::Uploaded(entry) => {
                        let (sym, color) = badge_symbol(entry);
                        (
                            sym,
                            Style::default().fg(color).add_modifier(Modifier::DIM),
                            if is_selected {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(color).add_modifier(Modifier::DIM)
                            },
                        )
                    }
                    NzbBadge::OnDisk {
                        origin,
                        has_password,
                    } => {
                        let (sym, color) = disk_badge_symbol(*origin, *has_password);
                        (
                            sym,
                            Style::default().fg(color).add_modifier(Modifier::DIM),
                            if is_selected {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(color).add_modifier(Modifier::DIM)
                            },
                        )
                    }
                    NzbBadge::None => (
                        "[ ] ",
                        Style::default().fg(Color::DarkGray),
                        if is_selected {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else if is_dir {
                            Style::default().fg(Color::Blue)
                        } else {
                            Style::default()
                        },
                    ),
                };

                // Marker takes the directory accent unless the row is selected
                // (then the highlight bg owns the styling).
                let marker_style = if is_dir && !is_selected {
                    Style::default().fg(Color::Blue)
                } else {
                    name_style
                };

                let mut spans = vec![
                    Span::styled(check, check_style),
                    Span::styled(marker, marker_style),
                    Span::styled(name, name_style),
                ];
                // Trailing marker for releases already sent through a hook (e.g.
                // uploaded to an indexer). Distinct from the upload/disk badge.
                if self.is_hooked(raw_name) {
                    spans.push(Span::styled(
                        " ↑sent",
                        Style::default()
                            .fg(Color::Magenta)
                            .add_modifier(Modifier::BOLD),
                    ));
                }

                ListItem::new(Line::from(spans))
            })
            .collect();

        let n_queued = self.queued.len();
        let queued_hint = if n_queued > 0 {
            format!(" — {} queued", n_queued)
        } else {
            String::new()
        };

        let (total, unbacked, bytes) = self.summary;
        let summary = if !self.summary_ready && total > 0 {
            " — scanning…".to_string()
        } else if unbacked > 0 {
            format!(" — {} unbacked · {} to upload", unbacked, fmt_bytes(bytes))
        } else if total > 0 {
            " — all backed ✓".to_string()
        } else {
            String::new()
        };
        let filter_tag = if self.filter_unbacked {
            " • filter:unbacked"
        } else {
            ""
        };

        let title = format!(
            " Browser — {} ({} items{}{}{}{}) ",
            self.current_dir.display(),
            total,
            if self.show_hidden { " • hidden" } else { "" },
            filter_tag,
            queued_hint,
            summary,
        );

        let border_style = if self.filter_unbacked {
            Style::default().fg(Color::Magenta)
        } else if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(border_style),
            )
            .highlight_style(crate::ui::theme::highlight());

        let mut state = ListState::default();
        state.select(Some(self.selected));
        *state.offset_mut() = self.scroll_offset;

        f.render_stateful_widget(list, area, &mut state);
    }
}

/// Returns `(badge_string, color)` for a catalog entry.
/// Badge is always 4 chars wide so the list stays aligned.
fn badge_symbol(entry: &NzbStatusEntry) -> (&'static str, Color) {
    match (entry.obfuscated, entry.has_password) {
        (false, false) => ("[✓] ", Color::Green),
        (true, false) => ("[~] ", Color::Yellow),
        (false, true) => ("[P] ", Color::Magenta),
        (true, true) => ("[*] ", Color::Cyan),
    }
}

/// Returns `(badge_string, color)` for an `.nzb` matched on disk but not in the
/// catalog. A password-protected release is always flagged with a magenta `P`
/// for maximum visibility; otherwise a Prowlarr download shows a yellow `↓` and
/// a prior upload (or manual file) a green `✓`. Badge stays 4 chars wide.
fn disk_badge_symbol(origin: NzbOrigin, has_password: bool) -> (&'static str, Color) {
    match (origin, has_password) {
        (NzbOrigin::Downloaded, false) => ("[↓] ", Color::Yellow),
        (NzbOrigin::Downloaded, true) => ("[↓P]", Color::Magenta),
        (_, false) => ("[✓] ", Color::Green),
        (_, true) => ("[✓P]", Color::Magenta),
    }
}

/// Compact human-readable byte size (e.g. `3.2 GB`) for the summary line.
pub(super) fn fmt_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit = 0;
    while size >= 1024.0 && unit < UNITS.len() - 1 {
        size /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", bytes, UNITS[unit])
    } else {
        format!("{:.1} {}", size, UNITS[unit])
    }
}
