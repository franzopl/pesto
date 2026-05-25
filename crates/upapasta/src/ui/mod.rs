use crate::app::{App, AppState};
use crate::events::UploadPhase;
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
        // slim status: single line without borders
        let slim = Paragraph::new(app.status_bar.message.clone())
            .style(Style::default().fg(Color::DarkGray));
        f.render_widget(slim, chunks[2]);
    } else {
        draw_header(f, chunks[0]);
        draw_tabs(f, app, chunks[1]);
        draw_main(f, app, chunks[2]);
        draw_status_bar(f, app, chunks[3]);
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
    let titles = vec![" Dashboard ", " Browser ", " History ", " Config "];
    let selected = match app.state {
        AppState::Dashboard => 0,
        AppState::Browser => 1,
        AppState::History => 2,
        AppState::Config => 3,
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
        _ => {
            draw_config(f, app, area);
        }
    }

    // Upload confirmation modal (overlay over any screen)
    if app.show_upload_confirm {
        draw_upload_confirm_modal(f, app, area);
    }
}

fn draw_browser(f: &mut Frame, app: &mut App, area: Rect) {
    // Split: file tree (left ~65%) | queue panel (right ~35%)
    let has_queue = !app.upload_queue.items.is_empty();
    let chunks = if has_queue {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(65), Constraint::Percentage(35)])
            .split(area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100)])
            .split(area)
    };

    app.file_tree.render(f, chunks[0], true);

    if has_queue {
        draw_browser_queue(f, app, chunks[1]);
    }
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

fn draw_upload_confirm_modal(f: &mut Frame, app: &App, area: Rect) {
    let s = app.effective_upload_settings();
    let queue = &app.upload_queue.items;

    // Modal dimensions: 60% wide, ~18 rows tall
    let modal_w = (area.width * 60 / 100).max(50).min(area.width - 4);
    let modal_h = (queue.len() as u16 + 16).min(area.height - 4);
    let x = (area.width.saturating_sub(modal_w)) / 2 + area.x;
    let y = (area.height.saturating_sub(modal_h)) / 2 + area.y;
    let modal_rect = Rect::new(x, y, modal_w, modal_h);

    f.render_widget(Clear, modal_rect);

    let inner = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .title(" Confirm Upload ")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        );
    let inner_rect = inner.inner(modal_rect);
    f.render_widget(inner, modal_rect);

    // Build content lines
    let mut lines: Vec<Line> = vec![Line::from(Span::styled(
        "  Files to upload:",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ))];

    for item in queue.iter().take(8) {
        let name = std::path::Path::new(item)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(item);
        lines.push(Line::from(Span::styled(
            format!("    • {}", name),
            Style::default().fg(Color::White),
        )));
    }
    if queue.len() > 8 {
        lines.push(Line::from(Span::styled(
            format!("    … and {} more", queue.len() - 8),
            Style::default().fg(Color::DarkGray),
        )));
    }

    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  Settings:  (j/k: nav  ←/→ Space: edit)",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    let key_style = Style::default().fg(Color::DarkGray);
    let val_style = Style::default().fg(Color::Yellow);
    let sel_key_style = Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD);
    let sel_val_style = Style::default()
        .fg(Color::Green)
        .add_modifier(Modifier::BOLD);
    let edit_hint_style = Style::default().fg(Color::DarkGray);

    // Read-only settings (no cursor)
    let readonly = [
        (
            "  Server   ",
            app.pesto_config
                .as_ref()
                .map(|c| format!("{}:{}", c.host, c.port))
                .unwrap_or_else(|| "dry-run".to_string()),
        ),
        ("  Groups   ", s.groups.clone()),
        ("  From     ", s.from.clone()),
        ("  Compress ", s.compression.clone()),
    ];
    for (key, val) in &readonly {
        lines.push(Line::from(vec![
            Span::styled("    ", Style::default()),
            Span::styled(key.to_string(), key_style),
            Span::styled(": ", key_style),
            Span::styled(val.clone(), val_style),
        ]));
    }

    // Editable settings with cursor indicator
    // field 0 = obfuscate, 1 = par2, 2 = verify
    let editable = [
        ("  Obfuscate", s.obfuscate.clone(), 0usize),
        ("  PAR2     ", s.par2.clone(), 1usize),
        ("  Verify   ", s.verify.clone(), 2usize),
    ];
    for (key, val, field_idx) in &editable {
        let is_selected = app.confirm_field == *field_idx;
        let cursor = if is_selected { "▶ " } else { "  " };
        let hint = if is_selected { "  ←/→ Space" } else { "" };
        lines.push(Line::from(vec![
            Span::styled(cursor, Style::default().fg(Color::Green)),
            Span::styled(
                key.to_string(),
                if is_selected {
                    sel_key_style
                } else {
                    key_style
                },
            ),
            Span::styled(
                ": ",
                if is_selected {
                    sel_key_style
                } else {
                    key_style
                },
            ),
            Span::styled(
                val.clone(),
                if is_selected {
                    sel_val_style
                } else {
                    val_style
                },
            ),
            Span::styled(hint.to_string(), edit_hint_style),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled(
            "  To change settings: ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("Esc → Config tab", Style::default().fg(Color::Cyan)),
    ]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("  [ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            "Enter / y",
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            " ] Start upload    [ ",
            Style::default().fg(Color::DarkGray),
        ),
        Span::styled("Esc / n", Style::default().fg(Color::Red)),
        Span::styled(" ] Cancel", Style::default().fg(Color::DarkGray)),
    ]));

    let para = Paragraph::new(lines);
    f.render_widget(para, inner_rect);
}

fn draw_dashboard(f: &mut Frame, app: &mut App, area: Rect) {
    let mut constraints = vec![
        Constraint::Length(7), // Phase indicator + gauge + sparkline (only when uploading)
        Constraint::Min(8),    // Main split (Queue + Logs)
    ];

    if !app.upload_in_progress {
        constraints.remove(0); // no progress bar when idle
    }

    let main_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(area);

    let content_area = if app.upload_in_progress {
        // Draw progress bar at top
        draw_progress_section(f, app, main_chunks[0]);
        main_chunks[1]
    } else {
        main_chunks[0]
    };

    // Split remaining into Queue (left) + Logs (right)
    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(content_area);

    if app.upload_in_progress && !app.progress.files.is_empty() {
        draw_per_file_progress(f, app, chunks[0]);
    } else if !app.upload_queue.items.is_empty() {
        draw_upload_settings_summary(f, app, chunks[0]);
    } else {
        let idle = Paragraph::new(
            "No files in queue.\n\n\
             Go to Browser tab (press Tab) → navigate with j/k/Enter → add files with Enter.\n\
             Then come back here and press 'u' to start upload.",
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

fn draw_progress_section(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.progress;
    let is_paused = app.upload_paused;

    // Layout: phase bar (1 line) + main gauge (3 lines) + sparkline (3 lines)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1), // Phase indicator
            Constraint::Length(3), // Main upload gauge
            Constraint::Length(3), // Sparkline
        ])
        .split(area);

    // ── Phase indicator ──────────────────────────────────────────────────────
    let phases = [
        (
            "Compress",
            matches!(p.phase, UploadPhase::Compressing { .. }),
        ),
        (
            "PAR2 Gen",
            matches!(
                p.phase,
                UploadPhase::GeneratingPar2 { .. } | UploadPhase::WritingPar2 { .. }
            ),
        ),
        ("Upload", matches!(p.phase, UploadPhase::Uploading)),
        ("Verify", matches!(p.phase, UploadPhase::Verifying { .. })),
    ];
    let phase_line: Vec<Span> = {
        let mut spans = vec![Span::raw("  ")];
        for (i, (label, active)) in phases.iter().enumerate() {
            if i > 0 {
                spans.push(Span::styled(" → ", Style::default().fg(Color::DarkGray)));
            }
            if *active {
                spans.push(Span::styled(
                    format!("[{}]", label),
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
            } else {
                spans.push(Span::styled(
                    label.to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
        }
        // Phase-specific detail
        let detail = match &p.phase {
            UploadPhase::Compressing {
                done_bytes,
                total_bytes,
            } if *total_bytes > 0 => {
                let pct = (*done_bytes as f64 / *total_bytes as f64 * 100.0) as u8;
                format!("  ({pct}%)")
            }
            UploadPhase::GeneratingPar2 {
                done_slices,
                total_slices,
            } if *total_slices > 0 => {
                let pct = (*done_slices as f64 / *total_slices as f64 * 100.0) as u8;
                format!("  ({done_slices}/{total_slices} slices, {pct}%)")
            }
            UploadPhase::WritingPar2 { written, total } if *total > 0 => {
                format!("  (writing {written}/{total})")
            }
            UploadPhase::Verifying { checked, total } if *total > 0 => {
                let pct = (*checked as f64 / *total as f64 * 100.0) as u8;
                format!("  ({checked}/{total} articles, {pct}%)")
            }
            _ => String::new(),
        };
        if !detail.is_empty() {
            spans.push(Span::styled(detail, Style::default().fg(Color::Cyan)));
        }
        spans
    };
    f.render_widget(Paragraph::new(Line::from(phase_line)), chunks[0]);

    // ── Main upload gauge ────────────────────────────────────────────────────
    // During PAR2/compress phases, show phase progress instead of upload bytes
    let (pct, label) = match &p.phase {
        UploadPhase::GeneratingPar2 {
            done_slices,
            total_slices,
        } if *total_slices > 0 => {
            let phase_pct = (*done_slices as f64 / *total_slices as f64 * 100.0) as u16;
            let lbl = format!(
                "PAR2  {:.1}%  {}/{} slices",
                *done_slices as f64 / *total_slices as f64 * 100.0,
                done_slices,
                total_slices,
            );
            (phase_pct, lbl)
        }
        UploadPhase::WritingPar2 { written, total } if *total > 0 => {
            let phase_pct = (*written as f64 / *total as f64 * 100.0) as u16;
            let lbl = format!("PAR2 write  {:.1}%  {}/{}", phase_pct, written, total);
            (phase_pct, lbl)
        }
        UploadPhase::Compressing {
            done_bytes: db,
            total_bytes: tb,
        } if *tb > 0 => {
            let phase_pct = (*db as f64 / *tb as f64 * 100.0) as u16;
            let lbl = format!(
                "Compress  {:.1}%  {}/{}",
                phase_pct,
                pesto::progress::format_size(*db),
                pesto::progress::format_size(*tb),
            );
            (phase_pct, lbl)
        }
        UploadPhase::Verifying { checked, total } if *total > 0 => {
            let phase_pct = (*checked as f64 / *total as f64 * 100.0) as u16;
            let lbl = format!("Verify  {:.1}%  {}/{} articles", phase_pct, checked, total);
            (phase_pct, lbl)
        }
        _ => {
            // Uploading or Preparing: show segment/byte progress
            let upload_pct = p.progress_pct() as u16;
            let speed = if p.last_speed > 0.1 {
                format!("{:.1} MB/s", p.last_speed)
            } else {
                "calculating...".to_string()
            };
            let eta = if let Some(secs) = p.eta_seconds() {
                format!("ETA {}:{:02}", secs / 60, secs % 60)
            } else {
                "ETA --:--".to_string()
            };
            let lbl = format!(
                "{:.1}%  {}/{}  {}  {}",
                p.progress_pct(),
                pesto::progress::format_size(p.done_bytes),
                pesto::progress::format_size(p.total_bytes),
                speed,
                eta,
            );
            (upload_pct, lbl)
        }
    };

    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(if is_paused {
            " Upload Progress — PAUSED (p: resume, x: cancel) "
        } else {
            " Upload Progress (p: pause, x: cancel) "
        }))
        .gauge_style(if is_paused {
            Style::default().fg(Color::Yellow).bg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Green).bg(Color::DarkGray)
        })
        .percent(pct)
        .label(if is_paused {
            "PAUSED".to_string()
        } else {
            label
        });
    f.render_widget(gauge, chunks[1]);

    // ── Speed sparkline ──────────────────────────────────────────────────────
    let spark_data: Vec<u64> = p.speed_history.iter().map(|&s| (s * 10.0) as u64).collect();
    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                .title(format!(" Speed ({} samples) ", spark_data.len())),
        )
        .data(&spark_data)
        .style(if is_paused {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Cyan)
        });
    f.render_widget(sparkline, chunks[2]);
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
        let help = "Enter/y: confirm upload   •   Esc/n: cancel";
        let status = Paragraph::new(help)
            .style(Style::default().fg(Color::Yellow))
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .title(" Confirm Upload "),
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
    ]
}

fn draw_config(f: &mut Frame, app: &App, area: Rect) {
    // Split: server info (top) + editable overrides (bottom)
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(7), Constraint::Min(5)])
        .split(area);

    // ── Server info (read-only) ──────────────────────────────────────────
    let cfg = app.pesto_config.as_ref();
    let server_lines: Vec<Line> = if let Some(c) = cfg {
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

    let server_block = Paragraph::new(server_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Server (read-only) ")
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
