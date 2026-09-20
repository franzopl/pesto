//! History NZB viewer overlay.

use crate::app::App;
use crate::ui::helpers::{category_color, centered_rect, format_bytes};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, Paragraph},
    Frame,
};

/// Centered popup showing the parsed contents of an archived NZB.
pub(in crate::ui) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let Some(ref viewer) = app.history.nzb_viewer else {
        return;
    };

    // Centered popup: 80% width, 80% height
    let popup = centered_rect(80, 80, area);
    f.render_widget(Clear, popup);

    let c = &viewer.contents;

    // Build header lines from meta
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
            Span::styled(pw.clone(), Style::default().fg(Color::Yellow)),
        ]));
    }
    meta_lines.push(Line::from(vec![
        Span::styled(" Files    ", Style::default().fg(Color::DarkGray)),
        Span::raw(format!(
            "{}  ({} segments, {})",
            c.files.len(),
            c.total_segments(),
            format_bytes(c.total_bytes())
        )),
    ]));
    meta_lines.push(Line::from(""));

    let header_h = meta_lines.len() as u16 + 2; // +2 for block borders

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(header_h), Constraint::Min(3)])
        .split(popup);

    // Meta block
    let meta_para = Paragraph::new(meta_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" NZB Archive  [Esc / q to close  ·  j/k to scroll] ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(meta_para, chunks[0]);

    // File list
    let visible_h = chunks[1].height.saturating_sub(2) as usize; // subtract borders
    let scroll = viewer.scroll;
    let files = &c.files;

    let items: Vec<ListItem> = files
        .iter()
        .enumerate()
        .skip(scroll)
        .take(visible_h)
        .map(|(i, file)| {
            let ext = std::path::Path::new(&file.name)
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("");
            let color = match ext {
                "par2" => Color::DarkGray,
                "nfo" | "sfv" => Color::Green,
                _ => Color::White,
            };
            let line = Line::from(vec![
                Span::styled(
                    format!("{:>3}. ", i + 1),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(file.name.clone(), Style::default().fg(color)),
                Span::styled(
                    format!(
                        "  {} segs  {}",
                        file.segment_count,
                        format_bytes(file.total_bytes)
                    ),
                    Style::default().fg(Color::DarkGray),
                ),
            ]);
            ListItem::new(line)
        })
        .collect();

    let scroll_indicator = if files.len() > visible_h {
        format!(" Files ({}/{}) ", scroll + 1, files.len())
    } else {
        format!(" Files ({}) ", files.len())
    };

    let file_list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(scroll_indicator)
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(file_list, chunks[1]);
}
