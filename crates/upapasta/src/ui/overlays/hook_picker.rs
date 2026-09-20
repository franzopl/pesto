//! Hook picker overlay.

use crate::app::App;
use crate::ui::{helpers::centered_rect, theme};

use ratatui::{
    layout::Rect,
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState},
    Frame,
};

/// Overlay listing the user's hooks so they can run exactly one against the
/// selected release. Mirrors the Prowlarr search overlay style.
pub(in crate::ui) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(ref picker) = app.hook_picker else {
        return;
    };

    let popup = centered_rect(70, 60, area);
    f.render_widget(Clear, popup);

    let confirming = picker.pending_confirm == Some(picker.selected);
    let (title, border) = if confirming {
        (
            format!(
                " Re-send to \"{}\"?  [Enter confirm · Esc cancel] ",
                picker.release_name
            ),
            Color::Yellow,
        )
    } else {
        (
            format!(
                " Run hook on \"{}\"  [j/k · Enter run · Esc close] ",
                picker.release_name
            ),
            Color::Cyan,
        )
    };

    let items: Vec<ListItem> = picker
        .hooks
        .iter()
        .map(|p| {
            let name = p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string());
            let mut spans = vec![Span::raw(format!(" {name}"))];
            // Flag hooks this release was already sent through, with the date.
            if let Some(dt) = picker.sent_at(p) {
                spans.push(Span::styled(
                    format!("   ✓ sent {}", dt.format("%Y-%m-%d %H:%M")),
                    Style::default().fg(Color::Magenta),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(border)),
        )
        .highlight_style(theme::highlight());

    let mut list_state = ListState::default();
    list_state.select(Some(picker.selected));
    f.render_stateful_widget(list, popup, &mut list_state);
}
