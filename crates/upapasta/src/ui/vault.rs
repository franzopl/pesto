//! NZB Vault screen rendering.

use crate::app::{App, NzbOrigin, VaultSort};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};

use super::{
    helpers::{category_color, format_bytes, truncate_str},
    theme,
};

pub(super) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    let sort_label = match app.vault.sort {
        VaultSort::Date => "date",
        VaultSort::Name => "name",
        VaultSort::Size => "size",
    };

    let count = app.vault.entries.len();
    let list_title = format!(
        " NZB Vault  {} file{}  [sort: {}]  [r reload · s sort · v view · d delete] ",
        count,
        if count == 1 { "" } else { "s" },
        sort_label,
    );

    let items: Vec<ListItem> = if let Some(ref err) = app.vault.load_error {
        vec![ListItem::new(Span::styled(
            format!(" {}", err),
            Style::default().fg(Color::Red),
        ))]
    } else if app.vault.entries.is_empty() {
        vec![ListItem::new(Span::styled(
            " No .nzb files found",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        app.vault
            .entries
            .iter()
            .map(|e| {
                let catalog_marker = if e.in_catalog { "✓" } else { "·" };
                let catalog_style = if e.in_catalog {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let (origin_sym, origin_style) = match e.origin {
                    NzbOrigin::Uploaded => ("↑", Style::default().fg(Color::Cyan)),
                    NzbOrigin::Downloaded => ("↓", Style::default().fg(Color::Yellow)),
                    NzbOrigin::Manual => ("m", Style::default().fg(Color::DarkGray)),
                };
                let size_str = format_bytes(e.file_size);
                // Reserve space for: " ✓ ↑ " (5) + "  123.4 KB" (11) = 16 cols
                let name_width = chunks[0].width.saturating_sub(18) as usize;
                let name = truncate_str(&e.name, name_width);
                ListItem::new(Line::from(vec![
                    Span::styled(format!(" {} ", catalog_marker), catalog_style),
                    Span::styled(format!("{} ", origin_sym), origin_style),
                    Span::raw(name),
                    Span::styled(
                        format!("  {:>9}", size_str),
                        Style::default().fg(Color::DarkGray),
                    ),
                ]))
            })
            .collect()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(list_title)
                .border_style(Style::default().fg(Color::Blue)),
        )
        .highlight_style(theme::highlight());

    let mut list_state = ListState::default();
    list_state.select(if app.vault.entries.is_empty() {
        None
    } else {
        Some(app.vault.selected)
    });
    f.render_stateful_widget(list, chunks[0], &mut list_state);

    draw_detail(f, app, chunks[1]);
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let entry = app.vault.selected_entry();

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Detail ")
        .border_style(Style::default().fg(Color::DarkGray));

    let Some(entry) = entry else {
        let p = Paragraph::new("No file selected").block(block);
        f.render_widget(p, area);
        return;
    };

    let mut lines: Vec<Line> = Vec::new();

    lines.push(Line::from(vec![
        Span::styled(" File  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            entry.name.clone(),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" Size  ", Style::default().fg(Color::DarkGray)),
        Span::raw(format_bytes(entry.file_size)),
    ]));
    lines.push(Line::from(vec![
        Span::styled(" Path  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            entry.path.to_string_lossy().into_owned(),
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let catalog_text = if entry.in_catalog {
        Span::styled("✓ in catalog", Style::default().fg(Color::Green))
    } else {
        Span::styled("· not in catalog", Style::default().fg(Color::DarkGray))
    };
    lines.push(Line::from(vec![
        Span::styled(" Catalog ", Style::default().fg(Color::DarkGray)),
        catalog_text,
    ]));

    lines.push(Line::raw(""));

    if let Some(ref contents) = entry.contents {
        if let Some(ref name) = contents.meta_name {
            lines.push(Line::from(vec![
                Span::styled(" Name  ", Style::default().fg(Color::DarkGray)),
                Span::raw(name.clone()),
            ]));
        }
        if let Some(ref cat) = contents.meta_category {
            lines.push(Line::from(vec![
                Span::styled(" Cat   ", Style::default().fg(Color::DarkGray)),
                Span::styled(cat.clone(), Style::default().fg(category_color(cat))),
            ]));
        }
        if let Some(ref pw) = contents.meta_password {
            lines.push(Line::from(vec![
                Span::styled(" Pass  ", Style::default().fg(Color::DarkGray)),
                Span::styled(format!("🔒 {}", pw), Style::default().fg(Color::Yellow)),
            ]));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            Span::styled(" Files    ", Style::default().fg(Color::DarkGray)),
            Span::raw(contents.files.len().to_string()),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Segments ", Style::default().fg(Color::DarkGray)),
            Span::raw(contents.total_segments().to_string()),
        ]));
        lines.push(Line::from(vec![
            Span::styled(" Total    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format_bytes(contents.total_bytes())),
        ]));
        lines.push(Line::raw(""));

        if let Some(first) = contents.files.first() {
            for group in &first.groups {
                lines.push(Line::from(vec![
                    Span::styled(" Group ", Style::default().fg(Color::DarkGray)),
                    Span::styled(group.clone(), Style::default().fg(Color::Cyan)),
                ]));
            }
        }

        lines.push(Line::raw(""));
        lines.push(Line::styled(
            " [v] open viewer",
            Style::default().fg(Color::DarkGray),
        ));
    } else {
        lines.push(Line::styled(
            " Press Enter to parse",
            Style::default().fg(Color::DarkGray),
        ));
    }

    let p = Paragraph::new(lines)
        .block(block)
        .wrap(Wrap { trim: false });
    f.render_widget(p, area);
}
