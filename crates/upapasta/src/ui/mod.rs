use crate::app::{App, AppState};
pub mod components;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Gauge, Paragraph, Tabs},
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
        Constraint::Length(4), // Progress bar area (only when uploading)
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

    app.upload_queue.render(f, chunks[0]);
    app.log_panel.render(f, chunks[1]);
}

fn draw_progress_section(f: &mut Frame, app: &App, area: Rect) {
    let p = &app.progress;
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

    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Upload Progress "),
        )
        .gauge_style(Style::default().fg(Color::Green).bg(Color::DarkGray))
        .percent(pct)
        .label(label);

    f.render_widget(gauge, area);
}

fn draw_status_bar(f: &mut Frame, app: &App, area: Rect) {
    app.status_bar.render(f, area);
}
