use crate::app::{App, AppState};
use pesto::config::ObfuscateMode;
pub mod components;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Clear, Gauge, List, ListItem, ListState, Paragraph, Sparkline, Tabs,
    },
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

    // Compact mode: skip header when height is tight
    let compact = area.height < 20;

    let constraints: Vec<Constraint> = if compact {
        vec![
            Constraint::Length(3), // Tabs only
            Constraint::Min(5),    // Main content
            Constraint::Length(1), // Status (slim)
        ]
    } else {
        vec![
            Constraint::Length(3), // Header
            Constraint::Length(3), // Tabs
            Constraint::Min(10),   // Main content
            Constraint::Length(3), // Status bar
        ]
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    if compact {
        draw_tabs(f, app, chunks[0]);
        draw_main(f, app, chunks[1]);
        let slim = Paragraph::new(app.status_bar.message.clone())
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(slim, chunks[2]);
    } else {
        draw_header(f, chunks[0]);
        draw_tabs(f, app, chunks[1]);
        draw_main(f, app, chunks[2]);
        draw_status_bar(f, app, chunks[3]);
    }

    // Prowlarr search overlay floats above everything
    if app.prowlarr.search.is_some() {
        draw_prowlarr_search_overlay(f, app, area);
    }
}

fn draw_header(f: &mut Frame, area: Rect) {
    let title = Line::from(vec![
        Span::styled(
            "UPA",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("PASTA "),
        Span::styled("v2", Style::default().fg(Color::Yellow)),
        Span::raw(" — Rust Edition"),
    ]);

    let header = Paragraph::new(title)
        .style(Style::default().fg(Color::White))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Blue))
                .title(" Welcome to UpaPasta "),
        );

    f.render_widget(header, area);
}

fn draw_tabs(f: &mut Frame, app: &App, area: Rect) {
    let titles = vec![
        " Dashboard ",
        " Browser ",
        " History ",
        " NZB Vault ",
        " Config ",
    ];
    let selected = match app.state {
        AppState::Dashboard => 0,
        AppState::Browser => 1,
        AppState::History => 2,
        AppState::NzbVault => 3,
        AppState::Config => 4,
    };

    let tabs = Tabs::new(titles)
        .select(selected)
        .style(Style::default().fg(Color::DarkGray))
        .highlight_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .block(Block::default().borders(Borders::ALL).title(" Navigation "));

    f.render_widget(tabs, area);
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    match app.state {
        AppState::Browser => {
            draw_browser(f, app, area);
        }
        AppState::Dashboard => {
            draw_dashboard(f, app, area);
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
            let lines = vec![
                Line::from(vec![
                    Span::styled(" File    ", Style::default().fg(Color::DarkGray)),
                    Span::raw(name.to_string()),
                ]),
                Line::from(""),
                Line::from(Span::styled(
                    " Marked for upload",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                )),
                Line::from(""),
                Line::from(Span::styled(
                    " Press u to open the upload panel",
                    Style::default().fg(Color::DarkGray),
                )),
                Line::from(Span::styled(
                    " Press Space to unmark",
                    Style::default().fg(Color::DarkGray),
                )),
            ];
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
        _ => Color::DarkGray,
    };

    let para = ratatui::widgets::Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(title)
            .border_style(Style::default().fg(border_color)),
    );
    f.render_widget(para, area);
}

fn draw_browser_queue(f: &mut Frame, app: &App, area: Rect) {
    let items: Vec<ratatui::widgets::ListItem> = app
        .upload_queue
        .items
        .iter()
        .enumerate()
        .map(|(i, item)| {
            let name = std::path::Path::new(item)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(item);
            let style = if i == app.upload_queue.selected {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            ratatui::widgets::ListItem::new(Line::from(vec![
                Span::styled("  ", Style::default()),
                Span::styled(name.to_string(), style),
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

fn draw_upload_config_panel(f: &mut Frame, app: &App, area: Rect) {
    use pesto::config::ObfuscateMode;

    let s = app.effective_upload_settings();
    let queue = &app.upload_queue.items;
    let cfg = app.pesto_config.as_ref();
    let ov = &app.config_state.overrides;

    let border_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);

    let outer = Block::default()
        .borders(Borders::ALL)
        .border_style(border_style)
        .title(Span::styled(
            " Upload Config  [y: start  Esc: cancel] ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    let inner_area = outer.inner(area);
    f.render_widget(outer, area);

    // Vertical sections inside the panel
    let file_count = queue.len().min(6) as u16;
    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(file_count + 2), // files block
            Constraint::Length(4),              // read-only info
            Constraint::Min(7),                 // editable fields
            Constraint::Length(1),              // bottom hint
        ])
        .split(inner_area);

    // ── Files list ──────────────────────────────────────────────────────────
    let file_items: Vec<Line> = queue
        .iter()
        .take(6)
        .map(|p| {
            let name = std::path::Path::new(p)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(p);
            let max = (vchunks[0].width as usize).saturating_sub(4);
            let short = if name.len() > max && max > 3 {
                format!("{}…", &name[..max - 1])
            } else {
                name.to_string()
            };
            Line::from(vec![
                Span::styled(" • ", Style::default().fg(Color::DarkGray)),
                Span::raw(short),
            ])
        })
        .collect();

    let extra = queue.len().saturating_sub(6);
    let file_title = if extra > 0 {
        format!(" Files ({}, +{} more) ", queue.len(), extra)
    } else {
        format!(" Files ({}) ", queue.len())
    };
    let mut all_file_lines = file_items;
    if extra > 0 {
        all_file_lines.push(Line::from(Span::styled(
            format!(" … and {} more", extra),
            Style::default().fg(Color::DarkGray),
        )));
    }
    f.render_widget(
        ratatui::widgets::Paragraph::new(all_file_lines).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .title(file_title)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        vchunks[0],
    );

    // ── Read-only info (server, compress) ──────────────────────────────────
    let server_str = cfg
        .map(|c| format!("{}:{}", c.host, c.port))
        .unwrap_or_else(|| "dry-run".to_string());

    let info_lines = vec![
        Line::from(vec![
            Span::styled(" Server   ", Style::default().fg(Color::DarkGray)),
            Span::raw(server_str),
        ]),
        Line::from(vec![
            Span::styled(" Compress ", Style::default().fg(Color::DarkGray)),
            Span::raw(s.compression.clone()),
        ]),
    ];
    f.render_widget(
        ratatui::widgets::Paragraph::new(info_lines).block(
            Block::default()
                .borders(Borders::BOTTOM)
                .border_style(Style::default().fg(Color::DarkGray)),
        ),
        vchunks[1],
    );

    // ── Editable fields ─────────────────────────────────────────────────────
    let obf_str = match ov
        .obfuscate
        .unwrap_or(cfg.map(|c| c.obfuscate).unwrap_or(ObfuscateMode::None))
    {
        ObfuscateMode::None => "none",
        ObfuscateMode::Subject => "subject",
        ObfuscateMode::Full => "full",
    };
    let par2_str = format!("{}%", ov.par2.unwrap_or(cfg.map(|c| c.par2).unwrap_or(10)));
    let verify_str = if ov.verify.unwrap_or(cfg.map(|c| c.verify).unwrap_or(false)) {
        "on"
    } else {
        "off"
    };

    let pw_raw = ov
        .nzb_password
        .as_deref()
        .or_else(|| cfg.and_then(|c| c.nzb_password.as_deref()))
        .unwrap_or("");
    let pw_display = if pw_raw.is_empty() {
        "—".to_string()
    } else if app.confirm_show_password {
        pw_raw.to_string()
    } else {
        "•".repeat(pw_raw.len().min(20))
    };

    let groups_str = ov
        .groups
        .clone()
        .or_else(|| cfg.map(|c| c.groups.join(", ")))
        .unwrap_or_else(|| "—".to_string());

    struct Field {
        label: &'static str,
        value: String,
        hint: &'static str,
    }
    let fields = [
        Field {
            label: " Obfuscate",
            value: obf_str.to_string(),
            hint: "←→ cycle",
        },
        Field {
            label: " PAR2 %  ",
            value: par2_str,
            hint: "←→ or Enter",
        },
        Field {
            label: " Verify  ",
            value: verify_str.to_string(),
            hint: "←→ toggle",
        },
        Field {
            label: " Password",
            value: pw_display,
            hint: "Enter edit  Tab show",
        },
        Field {
            label: " Groups  ",
            value: groups_str,
            hint: "Enter edit",
        },
    ];

    let field_area = vchunks[2];
    let header = Line::from(Span::styled(
        " Settings  (j/k navigate · Enter/e edit · ←→ cycle)",
        Style::default().fg(Color::DarkGray),
    ));

    let mut field_lines: Vec<Line> = vec![header, Line::from("")];

    for (i, field) in fields.iter().enumerate() {
        let is_sel = app.confirm_field == i;
        let is_editing = is_sel && app.confirm_editing;

        let cursor = if is_sel {
            Span::styled("▶", Style::default().fg(Color::Yellow))
        } else {
            Span::raw(" ")
        };

        let label_style = if is_sel {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let value_display = if is_editing {
            format!("{}_", app.confirm_edit_buf)
        } else {
            field.value.clone()
        };

        let value_style = if is_editing {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        } else if is_sel {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default().fg(Color::White)
        };

        let hint_style = Style::default().fg(Color::DarkGray);

        field_lines.push(Line::from(vec![
            cursor,
            Span::styled(format!("{:<10}", field.label), label_style),
            Span::styled(" ", Style::default()),
            Span::styled(value_display, value_style),
            if is_sel && !is_editing {
                Span::styled(format!("  {}", field.hint), hint_style)
            } else {
                Span::raw("")
            },
        ]));
    }

    f.render_widget(ratatui::widgets::Paragraph::new(field_lines), field_area);

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
             navigate with j/k/Enter →\n\
             mark with Space →\n\
             press u to queue & upload.",
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
    let is_paused = app.upload_paused;

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

    let label = if is_paused {
        "PAUSED".to_string()
    } else {
        format!(
            "{}%  {} / {}  {}  {}",
            upload_pct,
            pesto::progress::format_size(p.done_bytes),
            pesto::progress::format_size(p.total_bytes),
            speed_str,
            eta_str,
        )
    };

    let gauge_style = if is_paused {
        Style::default().fg(Color::Yellow).bg(Color::DarkGray)
    } else {
        Style::default()
            .fg(Color::Green)
            .bg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    };

    let title = if is_paused {
        " UPLOAD — PAUSED  [p: resume  x: cancel] "
    } else {
        " UPLOAD  [p: pause  x: cancel] "
    };

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
    let is_paused = app.upload_paused;

    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                .title(format!(" Speed history ({} samples) ", spark_data.len())),
        )
        .data(&spark_data)
        .style(if is_paused {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Cyan)
        });
    f.render_widget(sparkline, area);
}

fn draw_per_file_progress(f: &mut Frame, app: &App, area: Rect) {
    use crate::app::FileStatus;

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

        let (status_icon, icon_color) = match fp.status {
            FileStatus::Done => ("✓", Color::Green),
            FileStatus::Failed => ("✗", Color::Red),
            FileStatus::Active => ("▶", Color::Cyan),
            FileStatus::Pending => (" ", Color::DarkGray),
        };

        let name_row = rows[i * 2];
        let gauge_row = rows[i * 2 + 1];

        // Name line with status icon
        let max_name = (name_row.width as usize).saturating_sub(4);
        let short_name = if fp.name.len() > max_name && max_name > 3 {
            format!("{}…", &fp.name[..max_name - 1])
        } else {
            fp.name.clone()
        };
        let name_line = Line::from(vec![
            Span::styled(
                format!(" {} ", status_icon),
                Style::default().fg(icon_color),
            ),
            Span::raw(short_name),
        ]);
        f.render_widget(Paragraph::new(name_line), name_row);

        // Gauge
        let gauge_color = match fp.status {
            FileStatus::Done => Color::Green,
            FileStatus::Failed => Color::Red,
            FileStatus::Active => Color::Cyan,
            FileStatus::Pending => Color::DarkGray,
        };
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

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    if app.show_upload_confirm {
        let help = if app.confirm_editing {
            "Enter: confirm  •  Esc: cancel edit  •  Tab: toggle password visibility"
        } else {
            "j/k: navigate  •  Enter/←→: edit field  •  y: start upload  •  Esc: cancel"
        };
        let status = Paragraph::new(help)
            .style(Style::default().fg(Color::Yellow))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .title(" Upload Config "),
            );
        f.render_widget(status, area);
        return;
    }

    if app.upload_in_progress {
        let pause_resume = if app.upload_paused {
            "p: resume"
        } else {
            "p: pause"
        };
        let help = format!(
            "{}  •  {}  •  x: cancel  •  Tab: switch  •  q: quit",
            app.status_bar.message, pause_resume
        );
        let status = Paragraph::new(help)
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::TOP).title(" Status "));
        f.render_widget(status, area);
        return;
    }

    if app.state == AppState::Browser {
        let n = app.upload_queue.items.len();
        let marked = app.file_tree.marked_count();
        let hint = if marked > 0 {
            format!(
                "{}  •  Space: mark ({} marked)  •  u: queue & upload  •  Enter: navigate  •  Tab: switch",
                app.status_bar.message, marked
            )
        } else if n > 0 {
            format!(
                "{}  •  Space: mark files  •  u: upload queue ({} items)  •  Tab: switch",
                app.status_bar.message, n
            )
        } else {
            format!(
                "{}  •  Space: mark files  •  Enter: navigate  •  Tab: switch  •  q: quit",
                app.status_bar.message
            )
        };
        let status = Paragraph::new(hint)
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::TOP).title(" Status "));
        f.render_widget(status, area);
        return;
    }

    app.status_bar.render(f, area);
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
            let short_name = if r.original_name.len() > 34 {
                format!("{}…", &r.original_name[..33])
            } else {
                r.original_name.clone()
            };
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
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
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

    let obf_str = |m: ObfuscateMode| match m {
        ObfuscateMode::None => "none",
        ObfuscateMode::Subject => "subject",
        ObfuscateMode::Full => "full",
    };

    vec![
        ConfigField {
            label: "From",
            value: ov
                .from
                .clone()
                .or_else(|| cfg.map(|c| c.from.clone()))
                .unwrap_or_else(|| "—".into()),
            hint: "Sender address in posted articles",
            has_override: ov.from.is_some(),
        },
        ConfigField {
            label: "Groups",
            value: ov
                .groups
                .clone()
                .or_else(|| cfg.map(|c| c.groups.join(", ")))
                .unwrap_or_else(|| "—".into()),
            hint: "Comma-separated newsgroup list",
            has_override: ov.groups.is_some(),
        },
        ConfigField {
            label: "Obfuscate",
            value: ov
                .obfuscate
                .map(obf_str)
                .or_else(|| cfg.map(|c| obf_str(c.obfuscate)))
                .unwrap_or("none")
                .to_string(),
            hint: "Enter/e cycles: none → subject → full",
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
                .map(|v| if v { "true" } else { "false" })
                .or_else(|| cfg.map(|c| if c.verify { "true" } else { "false" }))
                .unwrap_or("false")
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
                .unwrap_or_else(|| "—".into()),
            hint: "Extraction password in the NZB <meta>",
            has_override: ov.nzb_password.is_some(),
        },
        ConfigField {
            label: "NZB category",
            value: ov
                .nzb_category
                .clone()
                .or_else(|| cfg.and_then(|c| c.nzb_category.clone()))
                .unwrap_or_else(|| "—".into()),
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
                .unwrap_or_else(|| "—".into()),
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
                .unwrap_or_else(|| "—".into()),
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
                .unwrap_or_else(|| "—".into()),
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
                Span::styled(format!("{:<14}", field.label), label_style),
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

    let title = if override_count > 0 {
        format!(
            " Overrides ({} active)  [j/k navigate · Enter/e edit · r reset field · R reset all] ",
            override_count
        )
    } else {
        " Overrides  [j/k navigate · Enter/e edit · r reset field] ".to_string()
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
                let name = if e.name.len() > name_width {
                    format!("{}…", &e.name[..name_width.saturating_sub(1)])
                } else {
                    e.name.clone()
                };
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
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

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
        let name = if nf.name.len() > 40 {
            format!("{}…", &nf.name[..39])
        } else {
            nf.name.clone()
        };
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
                let title_disp = if r.title.len() > max_title {
                    format!("{}…", &r.title[..max_title.saturating_sub(1)])
                } else {
                    r.title.clone()
                };
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
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );

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
