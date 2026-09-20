//! Prowlarr search and queue batch-search overlays.

use crate::app::App;
use crate::ui::{centered_rect, format_bytes, theme, truncate_str};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
    Frame,
};

// ── Prowlarr queue batch-search overlay ─────────────────────────────────────────

pub(in crate::ui) fn draw_batch(f: &mut Frame, app: &App, area: Rect) {
    let Some(ref batch) = app.prowlarr.batch else {
        return;
    };

    let popup = centered_rect(55, 30, area);
    f.render_widget(Clear, popup);

    let lines = vec![
        Line::from(Span::styled(
            format!(" Searching queue — {}/{}", batch.done, batch.total),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )),
        Line::from(Span::styled(
            format!(
                " {}",
                truncate_str(&batch.current, popup.width.saturating_sub(4) as usize)
            ),
            Style::default().fg(Color::Yellow),
        )),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                format!(" ✓ {} fetched", batch.downloaded),
                Style::default().fg(Color::Green),
            ),
            Span::raw("   "),
            Span::styled(
                format!("– {} no match", batch.no_match),
                Style::default().fg(Color::DarkGray),
            ),
            Span::raw("   "),
            Span::styled(
                format!("✗ {} failed", batch.failed),
                Style::default().fg(Color::Red),
            ),
        ]),
    ];

    let p = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Prowlarr — queue auto-fetch ")
            .border_style(Style::default().fg(Color::Cyan)),
    );
    f.render_widget(p, popup);
}

// ── Prowlarr search overlay ────────────────────────────────────────────────────

pub(in crate::ui) fn draw_search(f: &mut Frame, app: &App, area: Rect) {
    let Some(ref search) = app.prowlarr.search else {
        return;
    };

    let popup = centered_rect(88, 75, area);
    f.render_widget(Clear, popup);

    // Split: results list (left ~65%) + detail panel (right ~35%)
    let h_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
        .split(popup);

    // ── Left: results list ───────────────────────────────────────────────────
    let title = if search.searching {
        format!(" Prowlarr — searching \"{}\"… ", search.query)
    } else if let Some(ref e) = search.error {
        format!(" Prowlarr — error: {}  [Esc close] ", e)
    } else {
        format!(
            " Prowlarr — {} result{} for \"{}\"  [j/k · d download · Esc close] ",
            search.results.len(),
            if search.results.len() == 1 { "" } else { "s" },
            search.query
        )
    };

    let items: Vec<ListItem> = if search.searching {
        vec![ListItem::new(Span::styled(
            " Searching…",
            Style::default().fg(Color::Yellow),
        ))]
    } else if search.results.is_empty() {
        vec![ListItem::new(Span::styled(
            " No results — try a shorter release name",
            Style::default().fg(Color::DarkGray),
        ))]
    } else {
        search
            .results
            .iter()
            .map(|r| {
                let size_str = if r.size > 0 {
                    format_bytes(r.size)
                } else {
                    "—".to_string()
                };
                // Truncate title to fit
                let max_title = h_chunks[0].width.saturating_sub(20) as usize;
                let title_disp = truncate_str(&r.title, max_title);
                ListItem::new(Line::from(vec![
                    Span::raw(format!(" {:<width$}", title_disp, width = max_title)),
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
                .title(title)
                .border_style(Style::default().fg(Color::Cyan)),
        )
        .highlight_style(theme::highlight());

    let mut list_state = ListState::default();
    list_state.select(if search.results.is_empty() {
        None
    } else {
        Some(search.selected)
    });
    f.render_stateful_widget(list, h_chunks[0], &mut list_state);

    // ── Right: detail panel ───────────────────────────────────────────────────
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Detail ")
        .border_style(Style::default().fg(Color::DarkGray));

    let lines: Vec<Line> = if let Some(r) = search.selected_result() {
        let mut v: Vec<Line> = Vec::new();

        // Title (may wrap)
        v.push(Line::from(vec![
            Span::styled(" Title  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                r.title.clone(),
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));
        v.push(Line::raw(""));

        if !r.indexer.is_empty() {
            v.push(Line::from(vec![
                Span::styled(" Indexer ", Style::default().fg(Color::DarkGray)),
                Span::styled(r.indexer.clone(), Style::default().fg(Color::Cyan)),
            ]));
        }
        if r.size > 0 {
            v.push(Line::from(vec![
                Span::styled(" Size    ", Style::default().fg(Color::DarkGray)),
                Span::raw(format_bytes(r.size)),
            ]));
        }
        if !r.publish_date.is_empty() {
            // Show only the date part (ISO-8601 may include time)
            let date = r.publish_date.get(..10).unwrap_or(&r.publish_date);
            v.push(Line::from(vec![
                Span::styled(" Date    ", Style::default().fg(Color::DarkGray)),
                Span::raw(date.to_string()),
            ]));
        }
        if !r.categories.is_empty() {
            let cats: Vec<&str> = r.categories.iter().map(|c| c.name.as_str()).collect();
            v.push(Line::from(vec![
                Span::styled(" Category ", Style::default().fg(Color::DarkGray)),
                Span::raw(cats.join(", ")),
            ]));
        }
        if r.password_protected {
            v.push(Line::from(vec![
                Span::styled(" Password ", Style::default().fg(Color::DarkGray)),
                Span::styled("🔒 protected", Style::default().fg(Color::Yellow)),
            ]));
        }

        v.push(Line::raw(""));

        // Exact match indicator
        let q_lower = search.query.to_lowercase();
        let t_lower = r.title.to_lowercase();
        let t_stem = t_lower.strip_suffix(".nzb").unwrap_or(&t_lower);
        let match_label = if t_stem == q_lower {
            Span::styled("✓ exact release match", Style::default().fg(Color::Green))
        } else if t_stem.starts_with(&q_lower) || q_lower.starts_with(t_stem) {
            Span::styled("~ prefix match", Style::default().fg(Color::Yellow))
        } else {
            Span::styled("· partial match", Style::default().fg(Color::DarkGray))
        };
        v.push(Line::from(vec![
            Span::styled(" Match   ", Style::default().fg(Color::DarkGray)),
            match_label,
        ]));

        if search.downloading {
            v.push(Line::raw(""));
            v.push(Line::styled(
                " Downloading…",
                Style::default().fg(Color::Yellow),
            ));
        } else {
            v.push(Line::raw(""));
            v.push(Line::styled(
                " [d] download NZB",
                Style::default().fg(Color::DarkGray),
            ));
        }

        v
    } else {
        vec![Line::styled(
            " No result selected",
            Style::default().fg(Color::DarkGray),
        )]
    };

    let detail = Paragraph::new(lines)
        .block(block)
        .wrap(ratatui::widgets::Wrap { trim: false });
    f.render_widget(detail, h_chunks[1]);
}
