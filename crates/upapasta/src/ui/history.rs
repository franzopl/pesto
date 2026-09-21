//! History screen rendering.

use crate::app::App;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use super::{
    helpers::{category_color, format_bytes, truncate_str},
    theme,
};

pub(super) fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    if app.catalog.is_none() {
        let msg = Paragraph::new(
            "No catalog available.\n\nThe catalog could not be opened.\nCheck permissions for ~/.local/share/upapasta/",
        )
        .block(Block::default().borders(Borders::ALL).title(" History "));
        f.render_widget(msg, area);
        return;
    }

    let show_stats = app.history.show_stats;
    let mut constraints = vec![Constraint::Length(3), Constraint::Min(6)];
    if show_stats {
        constraints.push(Constraint::Length(10));
    }

    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_search(f, app, vchunks[0]);

    let hchunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(vchunks[1]);

    draw_list(f, app, hchunks[0]);
    draw_detail(f, app, hchunks[1]);

    if show_stats {
        draw_stats(f, app, vchunks[2]);
    }
}

fn draw_search(f: &mut Frame, app: &App, area: Rect) {
    let is_searching = app.history.searching;
    let query = &app.history.query;

    let content = if is_searching {
        format!(" /{}_", query)
    } else if query.is_empty() {
        " Press / to search, s for stats, Tab to switch tab".to_string()
    } else {
        format!(" Filter: {}  (/ to edit, Esc to clear)", query)
    };

    let border_style = if is_searching {
        Style::default().fg(Color::Yellow)
    } else if !query.is_empty() {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let title = format!(" History ({} records) ", app.history.rows.len());
    let para = Paragraph::new(content).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(border_style),
    );
    f.render_widget(para, area);
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = &app.history.rows;
    let items: Vec<ListItem> = rows
        .iter()
        .map(|r| {
            let date = r.uploaded_at.format("%Y-%m-%d").to_string();
            let size = r
                .size_bytes
                .map(|b| format_bytes(b as u64))
                .unwrap_or_else(|| "—".to_string());
            let cat_color = category_color(&r.category);
            let short_name = truncate_str(&r.original_name, 33);
            let (status_glyph, status_color) = if r.had_failures {
                (theme::ST_FAILED, Color::Red)
            } else {
                (theme::ST_DONE, Color::Green)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", date), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} ", status_glyph),
                    Style::default().fg(status_color),
                ),
                Span::styled(
                    format!("{:<34}", short_name),
                    Style::default().fg(Color::White),
                ),
                Span::styled(format!("{:<8}", r.category), Style::default().fg(cat_color)),
                Span::styled(size, Style::default().fg(Color::Cyan)),
            ]))
        })
        .collect();

    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(app.history.selected));
    }

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Uploads (j/k to navigate) "),
        )
        .highlight_style(theme::highlight())
        .highlight_symbol("▶ ");
    f.render_stateful_widget(list, area, &mut state);
}

fn draw_detail(f: &mut Frame, app: &App, area: Rect) {
    let rows = &app.history.rows;
    if rows.is_empty() || app.history.selected >= rows.len() {
        let msg = Paragraph::new(" No record selected.")
            .block(Block::default().borders(Borders::ALL).title(" Detail "));
        f.render_widget(msg, area);
        return;
    }

    let r = &rows[app.history.selected];
    let date = r.uploaded_at.format("%Y-%m-%d %H:%M UTC").to_string();
    let size = r
        .size_bytes
        .map(|b| format_bytes(b as u64))
        .unwrap_or_else(|| "unknown".to_string());
    let dur = r
        .upload_duration_s
        .map(|s| {
            let m = s as u64 / 60;
            let sec = s as u64 % 60;
            if m > 0 {
                format!("{}m {:02}s", m, sec)
            } else {
                format!("{:.1}s", s)
            }
        })
        .unwrap_or_else(|| "—".to_string());
    let group = r.usenet_group.as_deref().unwrap_or("—");

    let (status_text, status_color) = if r.had_failures {
        (" ✗ incomplete (segment failures)", Color::Red)
    } else {
        (" ✓ complete", Color::Green)
    };
    let lines = vec![
        Line::from(vec![
            Span::styled(" Name    ", Style::default().fg(Color::DarkGray)),
            Span::raw(r.original_name.clone()),
        ]),
        Line::from(vec![
            Span::styled(" Status  ", Style::default().fg(Color::DarkGray)),
            Span::styled(status_text, Style::default().fg(status_color)),
        ]),
        Line::from(vec![
            Span::styled(" Date    ", Style::default().fg(Color::DarkGray)),
            Span::raw(date),
        ]),
        Line::from(vec![
            Span::styled(" Category", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(" {}", r.category),
                Style::default().fg(category_color(&r.category)),
            ),
        ]),
        Line::from(vec![
            Span::styled(" Size    ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(" {}", size)),
        ]),
        Line::from(vec![
            Span::styled(" Duration", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(" {}", dur)),
        ]),
        Line::from(vec![
            Span::styled(" Group   ", Style::default().fg(Color::DarkGray)),
            Span::raw(format!(" {}", group)),
        ]),
    ];

    f.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Detail ")),
        area,
    );
}

fn draw_stats(f: &mut Frame, app: &App, area: Rect) {
    let Some(ref stats) = app.history.stats else {
        let msg = Paragraph::new(" Loading stats…")
            .block(Block::default().borders(Borders::ALL).title(" Stats "));
        f.render_widget(msg, area);
        return;
    };

    let total_gb = stats.total_bytes as f64 / 1024.0 / 1024.0 / 1024.0;
    let mut lines = vec![
        Line::from(format!(
            " Total: {} uploads  |  {:.2} GB",
            stats.total_uploads, total_gb
        )),
        Line::from(""),
    ];
    let cats: Vec<String> = stats
        .by_category
        .iter()
        .map(|(cat, n)| format!("{}: {}", cat, n))
        .collect();
    lines.push(Line::from(format!(" By category — {}", cats.join("  "))));

    if !stats.bytes_by_month.is_empty() {
        lines.push(Line::from(""));
        let month_strs: Vec<String> = stats
            .bytes_by_month
            .iter()
            .map(|(m, b)| format!("{}: {:.1}GB", m, *b as f64 / 1024.0 / 1024.0 / 1024.0))
            .collect();
        lines.push(Line::from(format!(" Monthly — {}", month_strs.join("  "))));
    }

    f.render_widget(
        Paragraph::new(lines).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Catalog Stats "),
        ),
        area,
    );
}
