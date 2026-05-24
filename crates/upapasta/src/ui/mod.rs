use crate::app::{App, AppState};
pub mod components;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Sparkline, Tabs},
    Frame,
};

pub fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Header
            Constraint::Length(3), // Tabs
            Constraint::Min(10),   // Main content
            Constraint::Length(3), // Status bar
        ])
        .split(f.area());

    draw_header(f, chunks[0]);
    draw_tabs(f, app, chunks[1]);
    draw_main(f, app, chunks[2]);
    draw_status_bar(f, app, chunks[3]);
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
            app.file_tree.render(f, area, true);
        }
        AppState::Dashboard => {
            draw_dashboard(f, app, area);
        }
        _ => {
            let title = match app.state {
                AppState::History => "History & Catalog",
                AppState::Config => "Configuration",
                _ => "Screen",
            };

            let content = Paragraph::new(
                "This screen is under construction.\n\n\
                 Current focus is on the **Browser** (file picker) and **Dashboard** (queue + live logs).\n\
                 Press Tab to cycle screens.",
            )
            .block(Block::default().borders(Borders::ALL).title(title));

            f.render_widget(content, area);
        }
    }
}

fn draw_dashboard(f: &mut Frame, app: &mut App, area: Rect) {
    let mut constraints = vec![
        Constraint::Length(7), // Progress bar + sparkline (only when uploading)
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
        app.upload_queue.render(f, chunks[0]);
    }

    app.log_panel.render(f, chunks[1]);
}

fn draw_progress_section(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.progress;

    // Split the progress area: Gauge on top, Sparkline below
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3), // Gauge
            Constraint::Length(3), // Sparkline + stats
        ])
        .split(area);

    let pct = p.progress_pct() as u16;

    let speed = if p.last_speed > 0.1 {
        format!("{:.1} MB/s", p.last_speed)
    } else {
        "calculating...".to_string()
    };

    let eta = if let Some(secs) = p.eta_seconds() {
        let m = secs / 60;
        let s = secs % 60;
        format!("ETA {}:{:02}", m, s)
    } else {
        "ETA --:--".to_string()
    };

    let label = format!(
        "{:.1}%  ({}/{} seg)  {}  {}",
        p.progress_pct(),
        p.done_segments,
        p.total_segments,
        speed,
        eta
    );

    let is_paused = app.upload_paused;

    let gauge = Gauge::default()
        .block(Block::default().borders(Borders::ALL).title(if is_paused {
            " Upload Progress — PAUSED "
        } else {
            " Upload Progress "
        }))
        .gauge_style(if is_paused {
            Style::default().fg(Color::Yellow).bg(Color::DarkGray)
        } else {
            Style::default().fg(Color::Green).bg(Color::DarkGray)
        })
        .percent(pct)
        .label(if is_paused {
            "PAUSED — Press 'p' to resume".to_string()
        } else {
            label
        });

    f.render_widget(gauge, chunks[0]);

    // Sparkline of recent throughput
    let spark_data: Vec<u64> = p
        .speed_history
        .iter()
        .map(|&s| (s * 10.0) as u64) // scale for visibility
        .collect();

    let spark_style = if is_paused {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Cyan)
    };

    let sparkline = Sparkline::default()
        .block(
            Block::default()
                .borders(Borders::LEFT | Borders::RIGHT | Borders::BOTTOM)
                .title(format!(
                    " Throughput History ({} samples) ",
                    spark_data.len()
                )),
        )
        .data(&spark_data)
        .style(spark_style);

    f.render_widget(sparkline, chunks[1]);
}

fn draw_per_file_progress(f: &mut Frame, app: &App, area: Rect) {
    use ratatui::text::Line;

    let mut lines: Vec<Line> = vec![Line::from(" Per-file Progress:")];

    for fp in &app.progress.files {
        let pct = if fp.total_segments > 0 {
            (fp.done_segments as f64 / fp.total_segments as f64 * 100.0) as u16
        } else {
            0
        };

        let status_icon = match fp.status {
            crate::app::FileStatus::Done => "✓",
            crate::app::FileStatus::Failed => "✗",
            crate::app::FileStatus::Active => "▶",
            _ => " ",
        };

        let short_name = if fp.name.len() > 28 {
            format!("{}...", &fp.name[..25])
        } else {
            fp.name.clone()
        };

        lines.push(Line::from(format!(
            " {} {} {:3}% ({}/{})",
            status_icon, short_name, pct, fp.done_segments, fp.total_segments
        )));
    }

    let para = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Upload Files "),
    );

    f.render_widget(para, area);
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
    // Dynamic help when upload is active
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

    app.status_bar.render(f, area);
}
