//! Upload Queue screen rendering.

use crate::app::App;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

use super::{
    helpers::{format_bytes, truncate_str},
    theme,
};

/// Dedicated full-height Queue screen (F2): the single home for reviewing,
/// reordering, removing and launching the upload queue built in the Browser.
pub(super) fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    use crate::app::FileStatus;

    // When the upload config panel is open (after `u`), it takes over the area.
    if app.show_upload_confirm {
        draw_upload_config_panel(f, app, area);
        return;
    }

    if app.upload_queue.items.is_empty() {
        let empty = Paragraph::new(
            "The upload queue is empty.\n\n\
             Go to the Browser tab (Tab / F3) →\n\
             navigate with j/k, Enter to open a folder →\n\
             press Space to queue a file or folder →\n\
             come back here (F2) to review and upload.",
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Upload Queue (empty) "),
        );
        f.render_widget(empty, area);
        return;
    }

    let mut total_bytes = 0u64;
    let mut done = 0usize;
    let mut failed = 0usize;
    let items: Vec<ListItem> = app
        .upload_queue
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let info = app.queue_info(item);
            total_bytes += info.size_bytes;
            let status = app.item_status(item);
            match status {
                FileStatus::Done => done += 1,
                FileStatus::Failed => failed += 1,
                _ => {}
            }
            let (glyph, color) = theme::status_glyph(status, app.upload_in_progress);
            let (marker, marker_style) = if info.is_dir {
                (theme::DIR_MARK, Style::default().fg(theme::DIR))
            } else {
                (theme::FILE_MARK, Style::default())
            };
            let detail = if info.is_dir {
                format!("  {} files → 1 NZB", info.files_label())
            } else {
                String::new()
            };
            let name_style = if i == app.upload_queue.selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{} ", glyph), Style::default().fg(color)),
                Span::styled(marker, marker_style),
                Span::styled(info.nzb_name, name_style),
                Span::styled(detail, Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("  ({})", format_bytes(info.size_bytes)),
                    Style::default().fg(Color::DarkGray),
                ),
            ]))
        })
        .collect();

    let n = app.upload_queue.items.len();
    let title = if app.upload_in_progress {
        format!(" Upload Queue ({n}) — {done} done · {failed} failed ")
    } else {
        format!(" Upload Queue ({n}) — {} total ", format_bytes(total_bytes))
    };
    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(theme::OK)),
        )
        .highlight_style(theme::highlight());
    let mut state = ListState::default();
    state.select(Some(app.upload_queue.selected));
    f.render_stateful_widget(list, area, &mut state);
}

pub(super) fn draw_upload_config_panel(f: &mut Frame, app: &App, area: Rect) {
    let queue = &app.upload_queue.items;
    let cfg = app.pesto_config.as_ref();

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(theme::FOCUS))
        .title(Span::styled(
            " Upload Config ",
            Style::default()
                .fg(theme::FOCUS)
                .add_modifier(Modifier::BOLD),
        ));
    let inner_area = outer.inner(area);
    f.render_widget(outer, area);

    // One row of horizontal padding inside the border for breathing room.
    let inner_area = inner_area.inner(ratatui::layout::Margin {
        horizontal: 1,
        vertical: 0,
    });

    // Vertical sections: each opens with a muted section header line instead of
    // an edge-to-edge rule, so the panel reads as grouped fields, not a form.
    let file_rows = queue.len().min(6) as u16;
    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(file_rows + 2), // FILES header + rows + spacer
            Constraint::Length(4),             // DESTINATION header + 2 + spacer
            Constraint::Min(7),                // SETTINGS
            Constraint::Length(1),             // bottom hint
        ])
        .split(inner_area);

    let section = |text: String| {
        Line::from(Span::styled(
            text,
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ))
    };

    // ── FILES ─────────────────────────────────────────────────────────────────
    let extra = queue.len().saturating_sub(6);
    let mut file_lines: Vec<Line> = vec![section(if extra > 0 {
        format!("FILES ({}, +{} more)", queue.len(), extra)
    } else {
        format!("FILES ({})", queue.len())
    })];
    for p in queue.iter().take(6) {
        let info = app.queue_info(p);
        let suffix = if info.is_dir {
            format!(" → {}.nzb ({} files)", info.nzb_name, info.files_label())
        } else {
            format!(" → {}.nzb", info.nzb_name)
        };
        let (marker, marker_style) = if info.is_dir {
            (theme::DIR_MARK, Style::default().fg(theme::DIR))
        } else {
            (theme::FILE_MARK, Style::default())
        };
        let name = std::path::Path::new(p)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(p);
        let prefix_len = marker.chars().count() + suffix.chars().count();
        let max = (vchunks[0].width as usize).saturating_sub(prefix_len + 2);
        let short = truncate_str(name, max);
        file_lines.push(Line::from(vec![
            Span::styled(marker, marker_style),
            Span::raw(short),
            Span::styled(suffix, theme::label()),
        ]));
    }
    f.render_widget(ratatui::widgets::Paragraph::new(file_lines), vchunks[0]);

    // ── DESTINATION (read-only) ────────────────────────────────────────────────
    let server_str = cfg
        .map(|c| format!("{}:{}", c.host, c.port))
        .unwrap_or_else(|| "dry-run".to_string());
    let info_lines = vec![
        section("DESTINATION".to_string()),
        Line::from(vec![
            Span::styled("  Server    ", theme::label()),
            Span::raw(server_str),
        ]),
    ];
    f.render_widget(ratatui::widgets::Paragraph::new(info_lines), vchunks[1]);

    // ── SETTINGS (editable) ─────────────────────────────────────────────────────
    // Field labels, values and hints all come from one source in `app`, so the
    // panel and the key handlers can never disagree on order or behaviour.
    let fields = app.confirm_field_views();
    let mut field_lines: Vec<Line> = vec![
        section("SETTINGS".to_string()),
        Line::from(Span::styled(
            format!("  {}", app.obfuscate_legend()),
            theme::label(),
        )),
        Line::from(""),
    ];

    for (i, field) in fields.iter().enumerate() {
        let is_sel = app.confirm_field == i;
        let is_editing = is_sel && app.confirm_editing;

        // A left bar marks the selected field instead of a stray arrow glyph.
        let bar = if is_sel {
            Span::styled("▌ ", Style::default().fg(theme::FOCUS))
        } else {
            Span::raw("  ")
        };

        let label_style = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            theme::label()
        };

        let value_display = if is_editing {
            format!("{}_", app.confirm_edit_buf)
        } else {
            field.value.clone()
        };

        let value_style = if is_editing {
            Style::default().fg(theme::OK).add_modifier(Modifier::BOLD)
        } else if is_sel {
            Style::default().fg(theme::FOCUS)
        } else {
            Style::default().fg(Color::White)
        };

        field_lines.push(Line::from(vec![
            bar,
            Span::styled(format!("{:<10}", field.label), label_style),
            Span::styled(value_display, value_style),
            if is_sel && !is_editing {
                Span::styled(format!("   {}", field.hint), theme::label())
            } else {
                Span::raw("")
            },
        ]));
    }

    f.render_widget(ratatui::widgets::Paragraph::new(field_lines), vchunks[2]);

    // ── Bottom hint line ────────────────────────────────────────────────────
    let hint = Line::from(vec![
        Span::styled(
            " y",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" start upload  ", Style::default().fg(Color::DarkGray)),
        Span::styled("Esc", Style::default().fg(Color::Red)),
        Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
    ]);
    f.render_widget(ratatui::widgets::Paragraph::new(hint), vchunks[3]);
}
