//! File Browser screen rendering.

use crate::app::App;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListState},
    Frame,
};

use super::{
    helpers::{category_color, format_bytes},
    queue, theme,
};

pub(super) fn draw(f: &mut Frame, app: &mut App, area: Rect) {
    let has_queue = !app.upload_queue.items.is_empty();

    // Always show file tree (left 60%) + right panel (40%).
    let hchunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
        .split(area);

    app.file_tree
        .render(f, hchunks[0], !app.show_upload_confirm);

    let right = hchunks[1];
    if app.show_upload_confirm {
        // Upload config panel replaces the NZB detail + queue panels
        queue::draw_upload_config_panel(f, app, right);
    } else if has_queue {
        // NZB detail (top ~60%) + queue (bottom ~40%)
        let vchunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
            .split(right);
        draw_nzb_detail_panel(f, app, vchunks[0]);
        draw_browser_queue(f, app, vchunks[1]);
    } else {
        draw_nzb_detail_panel(f, app, right);
    }
}

fn draw_nzb_detail_panel(f: &mut Frame, app: &App, area: Rect) {
    use crate::ui::components::file_tree::NzbBadge;

    let selected_path = app.file_tree.get_selected();
    let badge = app.file_tree.selected_badge();

    let (title, lines) = match (&badge, selected_path) {
        (Some(NzbBadge::Uploaded(entry)), Some(path)) => {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");

            let date = entry.uploaded_at.format("%Y-%m-%d %H:%M UTC").to_string();

            let mode = match (entry.obfuscated, entry.has_password) {
                (false, false) => ("Public", Color::Green),
                (true, false) => ("Obfuscated", Color::Yellow),
                (false, true) => ("Password protected", Color::Magenta),
                (true, true) => ("Obfuscated + password", Color::Cyan),
            };

            let nzb_line = if let Some(ref p) = entry.nzb_path {
                let exists = std::path::Path::new(p).exists();
                let label = std::path::Path::new(p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(p);
                let indicator = if exists {
                    " [✓ on disk]"
                } else {
                    " [! missing]"
                };
                let color = if exists { Color::Green } else { Color::Red };
                (label.to_string(), color, indicator)
            } else {
                ("—".to_string(), Color::DarkGray, "")
            };

            let size_str = entry
                .size_bytes
                .map(|b| format_bytes(b as u64))
                .unwrap_or_else(|| "—".to_string());

            let group = entry.usenet_group.as_deref().unwrap_or("—");

            let mut lines: Vec<Line> = vec![
                Line::from(vec![
                    Span::styled(" File    ", Style::default().fg(Color::DarkGray)),
                    Span::raw(name.to_string()),
                ]),
                Line::from(vec![
                    Span::styled(" Status  ", Style::default().fg(Color::DarkGray)),
                    Span::styled("✓ Uploaded", Style::default().fg(Color::Green)),
                ]),
                Line::from(vec![
                    Span::styled(" Date    ", Style::default().fg(Color::DarkGray)),
                    Span::raw(date),
                ]),
                Line::from(vec![
                    Span::styled(" Mode    ", Style::default().fg(Color::DarkGray)),
                    Span::styled(mode.0, Style::default().fg(mode.1)),
                ]),
                Line::from(vec![
                    Span::styled(" Pass    ", Style::default().fg(Color::DarkGray)),
                    if entry.has_password {
                        Span::styled("Set", Style::default().fg(Color::Magenta))
                    } else {
                        Span::styled("None", Style::default().fg(Color::DarkGray))
                    },
                ]),
                Line::from(vec![
                    Span::styled(" Group   ", Style::default().fg(Color::DarkGray)),
                    Span::raw(group.to_string()),
                ]),
                Line::from(vec![
                    Span::styled(" Size    ", Style::default().fg(Color::DarkGray)),
                    Span::raw(size_str),
                ]),
                Line::from(vec![
                    Span::styled(" Category", Style::default().fg(Color::DarkGray)),
                    Span::styled(
                        format!(" {}", entry.category),
                        Style::default().fg(category_color(&entry.category)),
                    ),
                ]),
                Line::from(vec![
                    Span::styled(" NZB     ", Style::default().fg(Color::DarkGray)),
                    Span::styled(nzb_line.0, Style::default().fg(nzb_line.1)),
                    Span::styled(nzb_line.2, Style::default().fg(nzb_line.1)),
                ]),
            ];

            // Legend row
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                " [✓] pub  [~] obf  [P] pass  [*] obf+pass",
                Style::default().fg(Color::DarkGray),
            )]));

            (" NZB Status ".to_string(), lines)
        }

        (Some(NzbBadge::Marked), Some(path)) => {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let info = app.queue_info(&path.to_string_lossy());
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(
                        if info.is_dir {
                            " Folder  "
                        } else {
                            " File    "
                        },
                        Style::default().fg(Color::DarkGray),
                    ),
                    Span::raw(name.to_string()),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    " ✓ Queued for upload",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
            ];
            if info.is_dir {
                lines.push(Line::from(vec![
                    Span::styled(" NZB     ", Style::default().fg(Color::DarkGray)),
                    Span::raw(format!(
                        "{}.nzb  ({} files in one release)",
                        info.nzb_name,
                        info.files_label()
                    )),
                ]));
            } else {
                lines.push(Line::from(vec![
                    Span::styled(" NZB     ", Style::default().fg(Color::DarkGray)),
                    Span::raw(format!("{}.nzb", info.nzb_name)),
                ]));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                " Press u to open the upload panel",
                Style::default().fg(Color::DarkGray),
            )));
            lines.push(Line::from(Span::styled(
                " Press Space to unqueue",
                Style::default().fg(Color::DarkGray),
            )));
            (" NZB Status ".to_string(), lines)
        }

        (Some(NzbBadge::Uploading), Some(path)) => {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let lines = vec![
                Line::from(vec![
                    Span::styled(" File    ", Style::default().fg(Color::DarkGray)),
                    Span::raw(name.to_string()),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    " ▶ Uploading now...",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    " See Dashboard tab for progress",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            (" NZB Status ".to_string(), lines)
        }

        (Some(NzbBadge::None), Some(path)) | (None, Some(path)) => {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let is_dir = path.is_dir();
            let lines = if is_dir {
                vec![
                    Line::from(vec![
                        Span::styled(" Dir     ", Style::default().fg(Color::DarkGray)),
                        Span::raw(name.to_string()),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Enter to navigate into directory",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(Span::styled(
                        " Space to mark the whole directory",
                        Style::default().fg(Color::DarkGray),
                    )),
                ]
            } else {
                vec![
                    Line::from(vec![
                        Span::styled(" File    ", Style::default().fg(Color::DarkGray)),
                        Span::raw(name.to_string()),
                    ]),
                    Line::from(""),
                    Line::from(Span::styled(
                        " No NZB record found",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(""),
                    Line::from(Span::styled(
                        " Space  mark for upload",
                        Style::default().fg(Color::DarkGray),
                    )),
                    Line::from(Span::styled(
                        " u      upload queue",
                        Style::default().fg(Color::DarkGray),
                    )),
                ]
            };
            (" NZB Status ".to_string(), lines)
        }

        (
            Some(NzbBadge::OnDisk {
                origin,
                has_password,
            }),
            Some(path),
        ) => {
            use crate::app::NzbOrigin;
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
            let (status_text, status_color) = match origin {
                NzbOrigin::Downloaded => ("↓ Downloaded via Prowlarr", Color::Yellow),
                _ => ("✓ NZB found on disk", Color::Green),
            };
            let mut lines = vec![
                Line::from(vec![
                    Span::styled(" File    ", Style::default().fg(Color::DarkGray)),
                    Span::raw(name.to_string()),
                ]),
                Line::from(vec![
                    Span::styled(" Status  ", Style::default().fg(Color::DarkGray)),
                    Span::styled(status_text, Style::default().fg(status_color)),
                ]),
                Line::from(vec![
                    Span::styled(" Pass    ", Style::default().fg(Color::DarkGray)),
                    if *has_password {
                        Span::styled("Set", Style::default().fg(Color::Magenta))
                    } else {
                        Span::styled("None", Style::default().fg(Color::DarkGray))
                    },
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    " Not recorded in this catalog (matched by release name).",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
            // Legend row
            lines.push(Line::from(""));
            lines.push(Line::from(vec![Span::styled(
                " [✓] upload  [↓] download  [P] password",
                Style::default().fg(Color::DarkGray),
            )]));
            (" NZB Status ".to_string(), lines)
        }

        _ => {
            let lines = vec![Line::from(Span::styled(
                " Navigate to a file to see its NZB status.",
                Style::default().fg(Color::DarkGray),
            ))];
            (" NZB Status ".to_string(), lines)
        }
    };

    let border_color = match &badge {
        Some(NzbBadge::Uploaded(e)) => match (e.obfuscated, e.has_password) {
            (false, false) => Color::Green,
            (true, false) => Color::Yellow,
            (false, true) => Color::Magenta,
            (true, true) => Color::Cyan,
        },
        Some(NzbBadge::Marked) => Color::Green,
        Some(NzbBadge::Uploading) => Color::Cyan,
        Some(NzbBadge::OnDisk { has_password, .. }) if *has_password => Color::Magenta,
        Some(NzbBadge::OnDisk {
            origin: crate::app::NzbOrigin::Downloaded,
            ..
        }) => Color::Yellow,
        Some(NzbBadge::OnDisk { .. }) => Color::Green,
        _ => Color::DarkGray,
    };

    let para = ratatui::widgets::Paragraph::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(border_color)),
        )
        .wrap(ratatui::widgets::Wrap { trim: false });
    f.render_widget(para, area);
}

fn draw_browser_queue(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ratatui::widgets::ListItem> = app
        .upload_queue
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let info = app.queue_info(item);
            let style = if i == app.upload_queue.selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let (glyph, status_color) =
                theme::status_glyph(app.item_status(item), app.upload_in_progress);
            let (marker, marker_style, suffix) = if info.is_dir {
                (
                    theme::DIR_MARK,
                    Style::default().fg(theme::DIR),
                    format!("  ({} files → 1 NZB)", info.files_label()),
                )
            } else {
                (theme::FILE_MARK, Style::default(), String::new())
            };
            ratatui::widgets::ListItem::new(Line::from(vec![
                Span::styled(format!("{glyph} "), Style::default().fg(status_color)),
                Span::styled(marker, marker_style),
                Span::styled(info.nzb_name, style),
                Span::styled(suffix, Style::default().fg(Color::DarkGray)),
            ]))
        })
        .collect();

    let title = format!(" Queue ({}) — u: upload ", app.upload_queue.items.len());
    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(Color::Green)),
    );

    let mut state = ListState::default();
    if !app.upload_queue.items.is_empty() {
        state.select(Some(app.upload_queue.selected));
    }
    f.render_stateful_widget(list, area, &mut state);
}
