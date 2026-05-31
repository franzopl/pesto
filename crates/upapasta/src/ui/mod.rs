use crate::app::{App, AppState};
pub mod components;
pub mod theme;

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Sparkline},
    Frame,
};

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();

    // Graceful degradation: terminal too small to render meaningfully
    if area.width < 40 || area.height < 10 {
        let msg = Paragraph::new(format!(
            "Terminal too small\n{}x{} — need 40x10",
            area.width, area.height
        ))
        .style(Style::default().fg(Color::Red))
        .block(Block::default().borders(Borders::ALL));
        f.render_widget(msg, area);
        return;
    }

    // Compact mode: drop the separator rule under the tab strip when height is tight
    let compact = area.height < 20;

    let constraints: Vec<Constraint> = if compact {
        vec![
            Constraint::Length(1), // Tab strip (no rule)
            Constraint::Min(5),    // Main content
            Constraint::Length(1), // Status (slim)
        ]
    } else {
        vec![
            Constraint::Length(2), // Tab strip + separator rule
            Constraint::Min(10),   // Main content
            Constraint::Length(1), // Status bar (single line)
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_top_bar(f, app, chunks[0], !compact);
    draw_main(f, app, chunks[1]);
    draw_status_bar(f, app, chunks[2]);

    // Prowlarr search overlay floats above everything
    if app.prowlarr.search.is_some() {
        draw_prowlarr_search_overlay(f, app, area);
    }

    // Queue batch-search progress floats above everything too.
    if app.prowlarr.batch.is_some() {
        draw_prowlarr_batch_overlay(f, app, area);
    }

    // Hook picker floats above everything.
    if app.hook_picker.is_some() {
        draw_hook_picker_overlay(f, app, area);
    }
}

/// Overlay listing the user's hooks so they can run exactly one against the
/// selected release. Mirrors the Prowlarr search overlay style.
fn draw_hook_picker_overlay(f: &mut Frame, app: &App, area: Rect) {
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

/// Single-line top bar: brand on the left, tab strip in the middle, version on
/// the right. When `rule` is true a thin separator is drawn on the row below,
/// giving structure without stacking bordered boxes.
fn draw_top_bar(f: &mut Frame, app: &App, area: Rect, rule: bool) {
    const TABS: [(&str, AppState); 6] = [
        ("Dashboard", AppState::Dashboard),
        ("Queue", AppState::Queue),
        ("Browser", AppState::Browser),
        ("History", AppState::History),
        ("Vault", AppState::NzbVault),
        ("Config", AppState::Config),
    ];

    let mut spans: Vec<Span> = vec![
        Span::styled(
            " UPAPASTA",
            Style::default()
                .fg(theme::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("  ", theme::label()),
    ];

    for (label, state) in TABS {
        if app.state == state {
            spans.push(Span::styled(
                format!(" {label} "),
                Style::default()
                    .fg(Color::Black)
                    .bg(theme::FOCUS)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(format!(" {label} "), theme::label()));
        }
        spans.push(Span::raw(" "));
    }

    let strip_area = if rule {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(1), Constraint::Length(1)])
            .split(area);
        // Separator rule under the strip.
        let rule_line = "─".repeat(parts[1].width as usize);
        f.render_widget(Paragraph::new(rule_line).style(theme::label()), parts[1]);
        parts[0]
    } else {
        area
    };

    // Right-aligned version tag, drawn first so the tab strip can overlay the left.
    f.render_widget(
        Paragraph::new(Line::from(Span::styled("v2 ", theme::label()))).alignment(Alignment::Right),
        strip_area,
    );
    f.render_widget(Paragraph::new(Line::from(spans)), strip_area);
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    match app.state {
        AppState::Browser => {
            draw_browser(f, app, area);
        }
        AppState::Dashboard => {
            draw_dashboard(f, app, area);
        }
        AppState::Queue => {
            draw_queue(f, app, area);
        }
        AppState::History => {
            draw_history(f, app, area);
            if app.history.nzb_viewer.is_some() {
                draw_nzb_viewer_overlay(f, app, area);
            }
        }
        AppState::NzbVault => {
            draw_nzb_vault(f, app, area);
            if app.vault.viewer.is_some() {
                draw_vault_viewer_overlay(f, app, area);
            }
        }
        AppState::Config => {
            draw_config(f, app, area);
        }
    }
}

fn draw_browser(f: &mut Frame, app: &mut App, area: Rect) {
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
        draw_upload_config_panel(f, app, right);
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

/// Dedicated full-height Queue screen (F2): the single home for reviewing,
/// reordering, removing and launching the upload queue built in the Browser.
fn draw_queue(f: &mut Frame, app: &mut App, area: Rect) {
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

fn draw_upload_config_panel(f: &mut Frame, app: &App, area: Rect) {
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

fn draw_dashboard(f: &mut Frame, app: &mut App, area: Rect) {
    if app.upload_in_progress {
        draw_upload_progress_screen(f, app, area);
    } else {
        draw_dashboard_idle(f, app, area);
    }
}

fn draw_dashboard_idle(f: &mut Frame, app: &mut App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(area);

    if !app.upload_queue.items.is_empty() {
        draw_upload_settings_summary(f, app, chunks[0]);
    } else {
        let idle = Paragraph::new(
            "No files in queue.\n\n\
             Go to Browser tab (Tab) →\n\
             navigate with j/k, Enter to open →\n\
             queue a file or folder with Space →\n\
             press u to upload.",
        )
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Dashboard — Ready "),
        );
        f.render_widget(idle, chunks[0]);
    }

    app.log_panel.render(f, chunks[1]);
}

fn draw_upload_progress_screen(f: &mut Frame, app: &mut App, area: Rect) {
    // Layout: three progress bars + sparkline on top; per-file + log below.
    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Compress bar
            Constraint::Length(3), // PAR2 bar
            Constraint::Length(3), // Upload bar (primary)
            Constraint::Length(3), // Speed sparkline
            Constraint::Min(4),    // Per-file (left) + Log (right)
        ])
        .split(area);

    draw_compress_bar(f, app, vchunks[0]);
    draw_par2_bar(f, app, vchunks[1]);
    draw_upload_bar(f, app, vchunks[2]);
    draw_speed_sparkline(f, app, vchunks[3]);

    // Bottom: per-file list left + log right
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(vchunks[4]);

    if !app.progress.files.is_empty() {
        draw_per_file_progress(f, app, bottom[0]);
    } else {
        // Fallback: show queue list while upload is spinning up
        let items: Vec<ListItem> = app
            .upload_queue
            .items
            .iter()
            .map(|p| {
                let name = std::path::Path::new(p)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(p);
                ListItem::new(Line::from(vec![
                    Span::styled(" ○ ", Style::default().fg(Color::DarkGray)),
                    Span::raw(name.to_string()),
                ]))
            })
            .collect();
        f.render_widget(
            List::new(items).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Files (preparing…) "),
            ),
            bottom[0],
        );
    }

    app.log_panel.render(f, bottom[1]);
}

fn draw_compress_bar(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.progress;

    let (pct, label, style) = if p.compress_total_bytes == 0 {
        // Compression not configured
        (
            0u16,
            "not configured".to_string(),
            Style::default().fg(Color::DarkGray).bg(Color::Reset),
        )
    } else if p.compress_finished {
        (
            100u16,
            format!(
                "done  {}",
                pesto::progress::format_size(p.compress_total_bytes)
            ),
            Style::default().fg(Color::DarkGray).bg(Color::Reset),
        )
    } else {
        let pct = if p.compress_total_bytes > 0 {
            (p.compress_done_bytes as f64 / p.compress_total_bytes as f64 * 100.0).min(100.0) as u16
        } else {
            0
        };
        (
            pct,
            format!(
                "{}%  {} / {}",
                pct,
                pesto::progress::format_size(p.compress_done_bytes),
                pesto::progress::format_size(p.compress_total_bytes)
            ),
            Style::default().fg(Color::Blue).bg(Color::DarkGray),
        )
    };

    let border_style = if p.compress_total_bytes > 0 && !p.compress_finished {
        Style::default().fg(Color::Blue)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Compress ")
                .border_style(border_style),
        )
        .gauge_style(style)
        .percent(pct)
        .label(label);
    f.render_widget(gauge, area);
}

fn draw_par2_bar(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.progress;

    let par2_done =
        p.par2_finished || (p.par2_total_slices > 0 && p.par2_done_slices >= p.par2_total_slices);
    let par2_active = p.par2_total_slices > 0 && !par2_done;

    const SPINNER: [char; 8] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧'];
    let spin = SPINNER[(app.tick_count / 2) as usize % SPINNER.len()];

    let (pct, label, style) = if p.par2_total_slices == 0 {
        (
            0u16,
            "pending…".to_string(),
            Style::default().fg(Color::DarkGray).bg(Color::Reset),
        )
    } else if par2_done {
        (
            100u16,
            format!("done  {} slices", p.par2_total_slices),
            Style::default().fg(Color::DarkGray).bg(Color::Reset),
        )
    } else {
        let pct =
            (p.par2_done_slices as f64 / p.par2_total_slices as f64 * 100.0).min(100.0) as u16;
        (
            pct,
            format!(
                "{} {}%  {}/{} slices",
                spin, pct, p.par2_done_slices, p.par2_total_slices
            ),
            Style::default().fg(Color::Yellow).bg(Color::DarkGray),
        )
    };

    let border_style = if par2_active {
        Style::default().fg(Color::Yellow)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let par2_title = if par2_active {
        format!(" PAR2 {} ", spin)
    } else {
        " PAR2 ".to_string()
    };

    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(par2_title)
                .border_style(border_style),
        )
        .gauge_style(style)
        .percent(pct)
        .label(label);
    f.render_widget(gauge, area);
}

fn draw_upload_bar(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.progress;

    let upload_pct = p.progress_pct() as u16;

    let speed_str = if p.last_speed > 0.1 {
        format!("{:.1} MB/s", p.last_speed)
    } else {
        "connecting…".to_string()
    };
    let eta_str = if let Some(secs) = p.eta_seconds() {
        format!("ETA {}:{:02}", secs / 60, secs % 60)
    } else {
        "ETA --:--".to_string()
    };

    let label = format!(
        "{}%  {} / {}  {}  {}",
        upload_pct,
        pesto::progress::format_size(p.done_bytes),
        pesto::progress::format_size(p.total_bytes),
        speed_str,
        eta_str,
    );

    let gauge_style = Style::default()
        .fg(Color::Green)
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);

    let title = " UPLOAD  [x: cancel] ";

    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    title,
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ))
                .border_style(Style::default().fg(Color::Green)),
        )
        .gauge_style(gauge_style)
        .percent(upload_pct)
        .label(label);
    f.render_widget(gauge, area);
}

fn draw_speed_sparkline(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.progress;
    let spark_data: Vec<u64> = p.speed_history.iter().map(|&s| (s * 10.0) as u64).collect();

    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                .title(format!(" Speed history ({} samples) ", spark_data.len())),
        )
        .data(&spark_data)
        .style(Style::default().fg(Color::Cyan));
    f.render_widget(sparkline, area);
}

fn draw_per_file_progress(f: &mut Frame, app: &App, area: Rect) {
    let files = &app.progress.files;
    let n = files.len();

    // Outer block
    let outer = Block::default()
        .borders(Borders::ALL)
        .title(format!(" Files ({}) ", n));
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    if n == 0 || inner.height == 0 {
        return;
    }

    // Allocate up to 3 lines per file (name + gauge + gap), constrained by height
    let rows_available = inner.height as usize;
    let per_file = 2usize; // name line + gauge line
    let max_files = (rows_available / per_file).max(1);
    let shown = n.min(max_files);

    // Build constraints: alternating name (1) + gauge (1) rows
    let constraints: Vec<Constraint> = (0..shown)
        .flat_map(|_| [Constraint::Length(1), Constraint::Length(1)])
        .collect();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(inner);

    for (i, fp) in files.iter().take(shown).enumerate() {
        let pct = if fp.total_segments > 0 {
            (fp.done_segments as f64 / fp.total_segments as f64 * 100.0).min(100.0) as u16
        } else {
            0
        };

        // Upload is in progress on this screen, so pending shows the running dot.
        let (status_icon, icon_color) = theme::status_glyph(fp.status, true);

        let name_row = rows[i * 2];
        let gauge_row = rows[i * 2 + 1];

        // Name line with status icon. fp.name is the queue path; show its
        // basename so a long absolute path does not crowd the gauge.
        let display_name = std::path::Path::new(&fp.name)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&fp.name);
        let max_name = (name_row.width as usize).saturating_sub(4);
        let short_name = truncate_str(display_name, max_name);
        let name_line = Line::from(vec![
            Span::styled(
                format!(" {} ", status_icon),
                Style::default().fg(icon_color),
            ),
            Span::raw(short_name),
        ]);
        f.render_widget(Paragraph::new(name_line), name_row);

        // Gauge — same color as the status glyph.
        let gauge_color = icon_color;
        let label = if fp.total_segments > 0 {
            format!("{pct}%  {}/{}", fp.done_segments, fp.total_segments)
        } else {
            "waiting…".to_string()
        };
        let gauge = Gauge::default()
            .gauge_style(Style::default().fg(gauge_color).bg(Color::DarkGray))
            .percent(pct)
            .label(label);
        f.render_widget(gauge, gauge_row);
    }

    // If we couldn't show all files, show a summary line
    if shown < n {
        // There's no room; the outer block title already shows the count
    }
}

fn draw_upload_settings_summary(f: &mut Frame, app: &App, area: Rect) {
    let s = app.effective_upload_settings();

    let lines = vec![
        Line::from(" Obfuscation : ".to_string() + &s.obfuscate),
        Line::from(" Compression : ".to_string() + &s.compression),
        Line::from(" PAR2        : ".to_string() + &s.par2),
        Line::from(" Groups      : ".to_string() + &s.groups),
        Line::from(" From        : ".to_string() + &s.from),
        Line::from(" Article     : ".to_string() + &s.article_size),
        Line::from(" Verify      : ".to_string() + &s.verify),
    ];

    let title = if app.pesto_config.is_some() {
        " Effective Upload Settings (from config) "
    } else {
        " Effective Upload Settings (dry-run defaults) "
    };

    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));

    f.render_widget(para, area);
}

/// Single-line status bar: the live message in white, then context-sensitive
/// key hints in muted grey. No border — it sits flush at the bottom.
fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    // Hints shown after the message, tailored to the current screen/mode.
    let hints: String = if app.show_upload_confirm {
        if app.confirm_editing {
            "Enter confirm · Esc cancel edit · Tab show password".into()
        } else {
            "j/k move · Enter/←→ edit · y start · Esc cancel".into()
        }
    } else if app.upload_in_progress {
        "x cancel · Tab switch · q quit".into()
    } else if app.state == AppState::Queue && !app.upload_queue.items.is_empty() {
        "u upload · d remove · c clear · J/K reorder · p fetch NZBs · Tab switch".into()
    } else if app.state == AppState::Browser {
        let filter = if app.file_tree.filter_unbacked {
            "n all"
        } else {
            "n unbacked"
        };
        let n = app.upload_queue.items.len();
        if n > 0 {
            format!("Space queue · u upload ({n}) · {filter} · p search · r hooks · Tab switch")
        } else {
            format!("Space queue · Enter open · {filter} · p search · r hooks · Tab switch")
        }
    } else if app.state == AppState::Config {
        "j/k move · Enter/e edit · r reset · R reset all · C check Prowlarr · Tab switch".into()
    } else {
        "Tab switch · q quit".into()
    };

    // The upload-config hints read better in the focus color; everything else
    // is muted so the eye lands on the message first.
    let hint_style = if app.show_upload_confirm {
        Style::default().fg(theme::FOCUS)
    } else {
        theme::label()
    };

    let line = Line::from(vec![
        Span::styled(format!(" {}  ", app.status_bar.message), Style::default()),
        Span::styled(hints, hint_style),
    ]);
    f.render_widget(Paragraph::new(line), area);
}

// ── History screen ─────────────────────────────────────────────────────────

fn draw_history(f: &mut Frame, app: &mut App, area: Rect) {
    if app.catalog.is_none() {
        let msg = Paragraph::new(
            "No catalog available.\n\nThe catalog could not be opened.\nCheck permissions for ~/.local/share/upapasta/",
        )
        .block(Block::default().borders(Borders::ALL).title(" History "));
        f.render_widget(msg, area);
        return;
    }

    // Layout: search bar on top, list left + detail right, stats at bottom
    let show_stats = app.history.show_stats;
    let mut constraints = vec![
        Constraint::Length(3), // search bar
        Constraint::Min(6),    // list + detail
    ];
    if show_stats {
        constraints.push(Constraint::Length(10)); // stats panel
    }

    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    draw_history_search(f, app, vchunks[0]);

    let hchunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(vchunks[1]);

    draw_history_list(f, app, hchunks[0]);
    draw_history_detail(f, app, hchunks[1]);

    if show_stats {
        draw_history_stats(f, app, vchunks[2]);
    }
}

fn draw_history_search(f: &mut Frame, app: &App, area: Rect) {
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

fn draw_history_list(f: &mut Frame, app: &mut App, area: Rect) {
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
            let short_name = truncate_str(&r.original_name, 34);
            let line = Line::from(vec![
                Span::styled(format!("{} ", date), Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{:<35}", short_name),
                    Style::default().fg(Color::White),
                ),
                Span::styled(format!("{:<8}", r.category), Style::default().fg(cat_color)),
                Span::styled(size, Style::default().fg(Color::Cyan)),
            ]);
            ListItem::new(line)
        })
        .collect();

    let selected = app.history.selected;
    let mut state = ListState::default();
    if !rows.is_empty() {
        state.select(Some(selected));
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

fn draw_history_detail(f: &mut Frame, app: &App, area: Rect) {
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

    let lines = vec![
        Line::from(vec![
            Span::styled(" Name    ", Style::default().fg(Color::DarkGray)),
            Span::raw(r.original_name.clone()),
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

    let para =
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Detail "));
    f.render_widget(para, area);
}

fn draw_history_stats(f: &mut Frame, app: &App, area: Rect) {
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

    // Categories
    let cats: Vec<String> = stats
        .by_category
        .iter()
        .map(|(cat, n)| format!("{}: {}", cat, n))
        .collect();
    lines.push(Line::from(format!(" By category — {}", cats.join("  "))));

    // Monthly bytes
    if !stats.bytes_by_month.is_empty() {
        lines.push(Line::from(""));
        let month_strs: Vec<String> = stats
            .bytes_by_month
            .iter()
            .map(|(m, b)| format!("{}: {:.1}GB", m, *b as f64 / 1024.0 / 1024.0 / 1024.0))
            .collect();
        lines.push(Line::from(format!(" Monthly — {}", month_strs.join("  "))));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Catalog Stats "),
    );
    f.render_widget(para, area);
}

// ── helpers ────────────────────────────────────────────────────────────────

/// Truncate `s` to at most `max` *characters* (not bytes), appending an ellipsis
/// when shortened. Char-safe so names with accents or other multibyte UTF-8
/// (e.g. "Programação") never panic on a non-char-boundary slice.
fn truncate_str(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else if max == 0 {
        String::new()
    } else {
        let kept: String = s.chars().take(max - 1).collect();
        format!("{kept}…")
    }
}

fn format_bytes(b: u64) -> String {
    if b >= 1_073_741_824 {
        format!("{:.1}GB", b as f64 / 1_073_741_824.0)
    } else if b >= 1_048_576 {
        format!("{:.0}MB", b as f64 / 1_048_576.0)
    } else if b >= 1024 {
        format!("{:.0}KB", b as f64 / 1024.0)
    } else {
        format!("{}B", b)
    }
}

// ── Config screen ─────────────────────────────────────────────────────────────

/// A single row in the Config screen field list.
struct ConfigField {
    label: &'static str,
    value: String,
    hint: &'static str,
    has_override: bool,
}

fn build_config_fields(app: &App) -> Vec<ConfigField> {
    let cfg = app.pesto_config.as_ref();
    let ov = &app.config_state.overrides;

    let masked = |s: &str| "*".repeat(s.len().min(12));
    use crate::app::{obf_label, on_off, UNSET};

    vec![
        ConfigField {
            label: "From",
            value: ov
                .from
                .clone()
                .or_else(|| cfg.map(|c| c.from.clone()))
                .unwrap_or_else(|| UNSET.into()),
            hint: "Sender address in posted articles",
            has_override: ov.from.is_some(),
        },
        ConfigField {
            label: "Groups",
            value: ov
                .groups
                .clone()
                .or_else(|| cfg.map(|c| c.groups.join(", ")))
                .unwrap_or_else(|| UNSET.into()),
            hint: "Comma-separated newsgroup list",
            has_override: ov.groups.is_some(),
        },
        ConfigField {
            label: "Obfuscate",
            value: ov
                .obfuscate
                .map(obf_label)
                .or_else(|| cfg.map(|c| obf_label(c.obfuscate)))
                .unwrap_or("None")
                .to_string(),
            hint: "Enter/e cycles: None → Subject → Full",
            has_override: ov.obfuscate.is_some(),
        },
        ConfigField {
            label: "PAR2 %",
            value: ov
                .par2
                .map(|v| format!("{}%", v))
                .or_else(|| cfg.map(|c| format!("{}%", c.par2)))
                .unwrap_or_else(|| "10%".into()),
            hint: "Recovery data percentage (0–50)",
            has_override: ov.par2.is_some(),
        },
        ConfigField {
            label: "Article size",
            value: ov
                .article_size_kb
                .map(|v| format!("{} KB", v))
                .or_else(|| cfg.map(|c| format!("{} KB", c.article_size / 1024)))
                .unwrap_or_else(|| "750 KB".into()),
            hint: "Enter value in KB",
            has_override: ov.article_size_kb.is_some(),
        },
        ConfigField {
            label: "Verify",
            value: ov
                .verify
                .map(on_off)
                .or_else(|| cfg.map(|c| on_off(c.verify)))
                .unwrap_or("Off")
                .to_string(),
            hint: "Enter/e toggles: STAT-check each article after posting",
            has_override: ov.verify.is_some(),
        },
        ConfigField {
            label: "NZB password",
            value: ov
                .nzb_password
                .as_deref()
                .map(masked)
                .or_else(|| cfg.and_then(|c| c.nzb_password.as_deref()).map(masked))
                .unwrap_or_else(|| UNSET.into()),
            hint: "Extraction password in the NZB <meta>",
            has_override: ov.nzb_password.is_some(),
        },
        ConfigField {
            label: "NZB category",
            value: ov
                .nzb_category
                .clone()
                .or_else(|| cfg.and_then(|c| c.nzb_category.clone()))
                .unwrap_or_else(|| UNSET.into()),
            hint: "Category tag in the NZB (e.g. Movies > HD)",
            has_override: ov.nzb_category.is_some(),
        },
        ConfigField {
            label: "Compress password",
            value: ov
                .compress_password
                .as_deref()
                .map(masked)
                .or_else(|| cfg.and_then(|c| c.compress_password.as_deref()).map(masked))
                .unwrap_or_else(|| UNSET.into()),
            hint: "Password for RAR/ZIP compression",
            has_override: ov.compress_password.is_some(),
        },
        ConfigField {
            label: "── Prowlarr ──",
            value: String::new(),
            hint: "",
            has_override: false,
        },
        ConfigField {
            label: "Prowlarr URL",
            value: app
                .prowlarr
                .url_override
                .clone()
                .or_else(|| cfg?.indexer_url.clone())
                .unwrap_or_else(|| UNSET.into()),
            hint: "Base URL, e.g. http://localhost:9696",
            has_override: app.prowlarr.url_override.is_some(),
        },
        ConfigField {
            label: "Prowlarr API key",
            value: app
                .prowlarr
                .api_key_override
                .as_deref()
                .map(masked)
                .or_else(|| cfg?.indexer_api_key.as_deref().map(masked))
                .unwrap_or_else(|| UNSET.into()),
            hint: "API key from Prowlarr Settings > General",
            has_override: app.prowlarr.api_key_override.is_some(),
        },
    ]
}

fn draw_config(f: &mut Frame, app: &App, area: Rect) {
    use crate::prowlarr::ConnectionStatus;

    // Split: server info (top) + editable overrides (bottom)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(10), Constraint::Min(5)])
        .split(area);

    // ── Server + Prowlarr info (read-only) ──────────────────────────────
    let cfg = app.pesto_config.as_ref();
    let mut server_lines: Vec<Line> = if let Some(c) = cfg {
        vec![
            Line::from(vec![
                Span::styled(" Host       ", Style::default().fg(Color::DarkGray)),
                Span::raw(format!("{}:{} (ssl={})", c.host, c.port, c.ssl)),
            ]),
            Line::from(vec![
                Span::styled(" Connections", Style::default().fg(Color::DarkGray)),
                Span::raw(format!(
                    " {}  (total: {})",
                    c.connections,
                    c.total_connections()
                )),
            ]),
            Line::from(vec![
                Span::styled(" Auth       ", Style::default().fg(Color::DarkGray)),
                Span::raw(if c.username.is_some() {
                    " configured".to_string()
                } else {
                    " anonymous".to_string()
                }),
            ]),
            Line::from(vec![
                Span::styled(" Extra srvrs", Style::default().fg(Color::DarkGray)),
                Span::raw(format!(" {}", c.extra_servers.len())),
            ]),
        ]
    } else {
        vec![Line::from(Span::styled(
            " No config file loaded — using dry-run mode.",
            Style::default().fg(Color::Yellow),
        ))]
    };

    // Prowlarr status line
    server_lines.push(Line::raw(""));
    let (prowlarr_label, prowlarr_style) = match &app.prowlarr.status {
        ConnectionStatus::Unknown => (
            " Prowlarr   not tested  [C to check connection]".to_string(),
            Style::default().fg(Color::DarkGray),
        ),
        ConnectionStatus::Checking => (
            " Prowlarr   checking…".to_string(),
            Style::default().fg(Color::Yellow),
        ),
        ConnectionStatus::Ok(ver) => (
            format!(" Prowlarr   ✓ connected  v{}", ver),
            Style::default().fg(Color::Green),
        ),
        ConnectionStatus::Failed(err) => (
            format!(" Prowlarr   ✗ {}  [C to retry]", err),
            Style::default().fg(Color::Red),
        ),
    };
    server_lines.push(Line::styled(prowlarr_label, prowlarr_style));

    let server_block = Paragraph::new(server_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Server & Integrations (read-only) ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(server_block, chunks[0]);

    // ── Editable overrides ───────────────────────────────────────────────
    let fields = build_config_fields(app);
    let selected = app.config_state.selected;
    let editing = app.config_state.editing;

    let items: Vec<ListItem> = fields
        .iter()
        .enumerate()
        .map(|(i, field)| {
            let is_sel = i == selected;
            let label_style = if is_sel {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };

            let override_indicator = if field.has_override {
                Span::styled("* ", Style::default().fg(Color::Cyan))
            } else {
                Span::raw("  ")
            };

            let value_display = if is_sel && editing {
                // Show edit buffer
                format!("{}_", app.config_state.edit_buf)
            } else {
                field.value.clone()
            };

            let value_style = if is_sel && editing {
                Style::default().fg(Color::Green)
            } else if field.has_override {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default().fg(Color::White)
            };

            let hint_style = Style::default().fg(Color::DarkGray);

            let line = Line::from(vec![
                override_indicator,
                // Wide enough for the longest label ("Compress password") so the
                // value column never butts up against the label.
                Span::styled(format!("{:<18}", field.label), label_style),
                Span::styled(value_display, value_style),
                if is_sel {
                    Span::styled(format!("   ← {}", field.hint), hint_style)
                } else {
                    Span::raw("")
                },
            ]);
            ListItem::new(line)
        })
        .collect();

    let override_count = {
        let ov = &app.config_state.overrides;
        [
            ov.from.is_some(),
            ov.groups.is_some(),
            ov.obfuscate.is_some(),
            ov.par2.is_some(),
            ov.article_size_kb.is_some(),
            ov.verify.is_some(),
            ov.nzb_password.is_some(),
            ov.nzb_category.is_some(),
            ov.compress_password.is_some(),
        ]
        .iter()
        .filter(|&&v| v)
        .count()
    };

    // Keystroke hints live in the status bar; the title stays short so it never
    // overflows the panel.
    let title = if override_count > 0 {
        format!(" Overrides ({override_count} active) ")
    } else {
        " Overrides ".to_string()
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(Color::Blue)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));

    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    f.render_stateful_widget(list, chunks[1], &mut list_state);
}

fn draw_nzb_viewer_overlay(f: &mut Frame, app: &App, area: Rect) {
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

/// Returns a centered `Rect` with the given percentage of width/height.
fn centered_rect(percent_x: u16, percent_y: u16, r: Rect) -> Rect {
    let popup_layout = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage((100 - percent_y) / 2),
            Constraint::Percentage(percent_y),
            Constraint::Percentage((100 - percent_y) / 2),
        ])
        .split(r);

    Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage((100 - percent_x) / 2),
            Constraint::Percentage(percent_x),
            Constraint::Percentage((100 - percent_x) / 2),
        ])
        .split(popup_layout[1])[1]
}

fn category_color(cat: &str) -> Color {
    match cat {
        "Movie" => Color::Magenta,
        "TV" => Color::Blue,
        "Anime" => Color::Yellow,
        _ => Color::DarkGray,
    }
}

// ── NZB Vault ─────────────────────────────────────────────────────────────────

fn draw_nzb_vault(f: &mut Frame, app: &App, area: Rect) {
    use crate::app::VaultSort;

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(55), Constraint::Percentage(45)])
        .split(area);

    // ── Left: file list ──────────────────────────────────────────────────────
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
                use crate::app::NzbOrigin;
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

    // ── Right: detail panel ──────────────────────────────────────────────────
    draw_vault_detail(f, app, chunks[1]);
}

fn draw_vault_detail(f: &mut Frame, app: &App, area: Rect) {
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

        // Groups from the first file
        if let Some(first) = contents.files.first() {
            for g in &first.groups {
                lines.push(Line::from(vec![
                    Span::styled(" Group ", Style::default().fg(Color::DarkGray)),
                    Span::styled(g.clone(), Style::default().fg(Color::Cyan)),
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
        .wrap(ratatui::widgets::Wrap { trim: false });
    f.render_widget(p, area);
}

fn draw_vault_viewer_overlay(f: &mut Frame, app: &App, area: Rect) {
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

// ── Prowlarr queue batch-search overlay ─────────────────────────────────────────

fn draw_prowlarr_batch_overlay(f: &mut Frame, app: &App, area: Rect) {
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

fn draw_prowlarr_search_overlay(f: &mut Frame, app: &App, area: Rect) {
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

#[cfg(test)]
mod tests {
    use super::truncate_str;

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate_str("abc", 5), "abc");
        assert_eq!(truncate_str("abcde", 5), "abcde");
    }

    #[test]
    fn truncate_adds_ellipsis_when_too_long() {
        assert_eq!(truncate_str("abcdef", 4), "abc\u{2026}");
    }

    #[test]
    fn truncate_is_char_safe_with_multibyte() {
        // Must not panic on a multibyte boundary (regression: vault crash on
        // "Programa\u{e7}\u{e3}o"-style names).
        let s = "Programa\u{e7}\u{e3}o_e_IA";
        let out = truncate_str(s, 10);
        assert!(out.chars().count() <= 10);
        assert!(out.ends_with('\u{2026}'));
    }
}
