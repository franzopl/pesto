//! Dashboard screen and upload progress rendering.

use crate::app::{App, FileProgress, FileStatus};

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Block, Borders, Gauge, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
        ScrollbarState, Sparkline,
    },
    Frame,
};

use super::{theme, truncate_str};

pub(super) fn draw(f: &mut Frame, app: &mut App, area: Rect) {
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
    // The streaming check queue runs concurrently with the upload rather
    // than as its own phase, so its bar shows up as soon as the first
    // article has been checked and stays up alongside the upload bar.
    let is_checking = app.progress.check_checked > 0;

    // Layout: three progress bars + optional check bar + sparkline on top; per-file + log below.
    let vchunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),                               // Compress bar
            Constraint::Length(3),                               // PAR2 bar
            Constraint::Length(3),                               // Upload bar (primary)
            Constraint::Length(if is_checking { 3 } else { 0 }), // Check bar
            Constraint::Length(3),                               // Speed sparkline
            Constraint::Min(4),                                  // Per-file + Log
        ])
        .split(area);

    draw_compress_bar(f, app, vchunks[0]);
    draw_par2_bar(f, app, vchunks[1]);
    draw_upload_bar(f, app, vchunks[2]);
    if is_checking {
        draw_check_bar(f, app, vchunks[3]);
    }
    draw_speed_sparkline(f, app, vchunks[4]);

    // Bottom: per-file list left + log right
    let bottom = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(38), Constraint::Percentage(62)])
        .split(vchunks[5]);

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

    let label = if p.is_paused {
        format!(
            "PAUSED  {}%  {} / {}",
            upload_pct,
            pesto::progress::format_size(p.done_bytes),
            pesto::progress::format_size(p.total_bytes),
        )
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

    let accent = if p.is_paused {
        Color::Yellow
    } else {
        Color::Green
    };

    let gauge_style = Style::default()
        .fg(accent)
        .bg(Color::DarkGray)
        .add_modifier(Modifier::BOLD);

    let title = if p.is_paused {
        " UPLOAD  [p: resume] [x: cancel] "
    } else {
        " UPLOAD  [p: pause] [x: cancel] "
    };

    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    title,
                    Style::default().fg(accent).add_modifier(Modifier::BOLD),
                ))
                .border_style(Style::default().fg(accent)),
        )
        .gauge_style(gauge_style)
        .percent(upload_pct)
        .label(label);
    f.render_widget(gauge, area);
}

fn draw_check_bar(f: &mut Frame, app: &App, area: Rect) {
    // The streaming check queue has no fixed total known upfront (it runs
    // concurrently with, and for the lifetime of, the upload), so this
    // shows a running count instead of a total-based percentage. The bar
    // fraction reflects the verified share of articles checked so far —
    // it stays full and green under normal conditions, dipping only when
    // articles actually go missing.
    let checked = app.progress.check_checked;
    let failed = app.progress.check_failed;
    let verified = checked.saturating_sub(failed);
    let pending = app.progress.done_segments.saturating_sub(checked);
    let pct = (verified * 100)
        .checked_div(checked)
        .unwrap_or(100)
        .min(100) as u16;
    let (label, color) = if failed > 0 {
        (
            format!("{verified} verified · {pending} pending · {failed} missing"),
            Color::Red,
        )
    } else {
        (
            format!("{verified} verified · {pending} pending"),
            Color::Cyan,
        )
    };

    let gauge = Gauge::default()
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " CHECK ",
                    Style::default().fg(color).add_modifier(Modifier::BOLD),
                ))
                .border_style(Style::default().fg(color)),
        )
        .gauge_style(
            Style::default()
                .fg(color)
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .percent(pct)
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
    // Only the content files belong here; PAR2 recovery volumes have their own
    // dedicated bar at the top of the Dashboard, so listing each `.par2` volume
    // as a row just clutters the panel and buries the real files.
    let files: Vec<&FileProgress> = app
        .progress
        .files
        .iter()
        .filter(|fp| !fp.name.to_ascii_lowercase().ends_with(".par2"))
        .collect();
    let n = files.len();

    // Each file takes two rows: name line + gauge line.
    let per_file = 2usize;
    let rows_available = area.height.saturating_sub(2) as usize; // minus block borders
    let max_files = (rows_available / per_file).max(1);
    let shown = n.min(max_files);
    let overflow = n > shown;

    // Follow the upload: anchor the view on the active file (or, between files,
    // the last completed one) and keep it centred so completed history scrolls
    // up while the next files stay visible. Without this the panel is pinned to
    // index 0 and the work happening further down is never seen.
    let frontier = files
        .iter()
        .rposition(|fp| fp.status == FileStatus::Active)
        .or_else(|| files.iter().rposition(|fp| fp.status == FileStatus::Done))
        .unwrap_or(0);
    let offset = frontier
        .saturating_sub(shown / 2)
        .min(n.saturating_sub(shown));

    // Outer block; title shows the visible window when the list overflows.
    let title = if overflow {
        format!(" Files ({}-{}/{}) ", offset + 1, offset + shown, n)
    } else {
        format!(" Files ({}) ", n)
    };
    let outer = Block::default().borders(Borders::ALL).title(title);
    let inner = outer.inner(area);
    f.render_widget(outer, area);

    if n == 0 || inner.height == 0 {
        return;
    }

    // Reserve a one-column gutter on the right for the scrollbar when needed.
    let (list_area, scrollbar_area) = if overflow {
        let cols = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(0), Constraint::Length(1)])
            .split(inner);
        (cols[0], Some(cols[1]))
    } else {
        (inner, None)
    };

    // Build constraints: alternating name (1) + gauge (1) rows
    let constraints: Vec<Constraint> = (0..shown)
        .flat_map(|_| [Constraint::Length(1), Constraint::Length(1)])
        .collect();

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints(constraints)
        .split(list_area);

    for (i, fp) in files.iter().skip(offset).take(shown).enumerate() {
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

    // Scrollbar showing where the visible window sits within the full list.
    if let Some(sb_area) = scrollbar_area {
        let mut sb_state = ScrollbarState::new(n)
            .viewport_content_length(shown)
            .position(offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight)
            .begin_symbol(None)
            .end_symbol(None);
        f.render_stateful_widget(scrollbar, sb_area, &mut sb_state);
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
        Line::from(" Check       : ".to_string() + &s.check),
    ];

    let title = if app.pesto_config.is_some() {
        " Effective Upload Settings (from config) "
    } else {
        " Effective Upload Settings (dry-run defaults) "
    };

    let para = Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title));

    f.render_widget(para, area);
}
