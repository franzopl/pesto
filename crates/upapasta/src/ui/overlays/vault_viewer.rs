//! NZB Vault viewer overlay.

use crate::app::App;
use crate::ui::helpers::{category_color, centered_rect, format_bytes, truncate_str};

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
    Frame,
};

/// Centered popup listing every file in the selected vault NZB.
pub(in crate::ui) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(ref viewer) = app.vault.viewer else {
        return;
    };

    let popup = centered_rect(80, 80, area);
    f.render_widget(Clear, popup);

    let c = &viewer.contents;

    let mut meta_lines: Vec<Line> = Vec::new();
    if let Some(ref name) = c.meta_name {
        meta_lines.push(Line::from(vec![
            Span::styled(" Name     ", Style::default().fg(Color::DarkGray)),
            Span::raw(name.clone()),
        ]));
    }
    if let Some(ref cat) = c.meta_category {
        meta_lines.push(Line::from(vec![
            Span::styled(" Category ", Style::default().fg(Color::DarkGray)),
            Span::styled(cat.clone(), Style::default().fg(category_color(cat))),
        ]));
    }
    if let Some(ref pw) = c.meta_password {
        meta_lines.push(Line::from(vec![
            Span::styled(" Password ", Style::default().fg(Color::DarkGray)),
            Span::styled(format!("🔒 {}", pw), Style::default().fg(Color::Yellow)),
        ]));
    }

    let total_bytes = c.total_bytes();
    let total_segments = c.total_segments();
    meta_lines.push(Line::from(vec![
        Span::styled(" Total    ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            "{}  ({} segments)",
            format_bytes(total_bytes),
            total_segments
        )),
    ]));
    meta_lines.push(Line::raw(""));

    // File list
    let header = Line::from(vec![Span::styled(
        format!(" {:<40}  {:>9}  {:>6}", "Filename", "Size", "Segs"),
        Style::default()
            .fg(Color::Yellow)
            .add_modifier(Modifier::BOLD),
    )]);
    meta_lines.push(header);

    for nf in &c.files {
        let name = truncate_str(&nf.name, 40);
        meta_lines.push(Line::from(Span::raw(format!(
            " {:<40}  {:>9}  {:>6}",
            name,
            format_bytes(nf.total_bytes),
            nf.segment_count
        ))));
    }

    let scrollable_count = meta_lines.len();
    let visible = popup.height.saturating_sub(4) as usize;
    let scroll = viewer.scroll.min(scrollable_count.saturating_sub(visible));

    let title = format!(
        " NZB Vault — {} files  [j/k scroll · Esc close] ",
        c.files.len()
    );

    let p = Paragraph::new(meta_lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .scroll((scroll as u16, 0));
    f.render_widget(p, popup);
}
