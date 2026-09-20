//! Watch screen field projection and rendering.

use crate::app::App;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

struct WatchField {
    label: &'static str,
    value: String,
    hint: &'static str,
}

fn build_watch_fields(app: &App) -> Vec<WatchField> {
    use crate::app::UNSET;
    vec![
        WatchField {
            label: "Directory",
            value: app
                .watch
                .dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| UNSET.into()),
            hint: "Folder to monitor for new files/folders",
        },
        WatchField {
            label: "Done directory",
            value: app
                .watch
                .done_dir
                .as_ref()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| format!("{UNSET}  (source left in place after upload)")),
            hint: "Move source here after a successful upload",
        },
        WatchField {
            label: "Extensions",
            value: if app.watch.ext_filter.is_empty() {
                format!("{UNSET}  (no filtering)")
            } else {
                app.watch.ext_filter.clone()
            },
            hint: "Comma-separated, e.g. mkv,mp4 — folders always match",
        },
        WatchField {
            label: "Interval",
            value: format!("{}s", app.watch.interval_secs),
            hint: "Seconds between directory scans",
        },
    ]
}

/// Watch-mode screen (F7): configure the monitored directory and see its
/// live status (baselining, stabilizing, queued, uploading).
pub(super) fn draw(f: &mut Frame, app: &App, area: Rect) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(8), Constraint::Min(6)])
        .split(area);

    // ── Editable fields ──────────────────────────────────────────────────
    let fields = build_watch_fields(app);
    let selected = app.watch.selected;
    let editing = app.watch.editing;

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
            let value_display = if is_sel && editing {
                format!("{}_", app.watch.edit_buf)
            } else {
                field.value.clone()
            };
            let value_style = if is_sel && editing {
                Style::default().fg(Color::Green)
            } else {
                Style::default().fg(Color::White)
            };
            let line = Line::from(vec![
                Span::styled(format!("{:<16}", field.label), label_style),
                Span::styled(value_display, value_style),
                if is_sel {
                    Span::styled(
                        format!("   ← {}", field.hint),
                        Style::default().fg(Color::DarkGray),
                    )
                } else {
                    Span::raw("")
                },
            ]);
            ListItem::new(line)
        })
        .collect();

    let title = if app.watch.enabled {
        " Watch settings — RUNNING (w to stop) "
    } else {
        " Watch settings — stopped (w to start) "
    };
    let accent = if app.watch.enabled {
        Color::Yellow
    } else {
        Color::Blue
    };

    let list = List::new(items)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(title)
                .border_style(Style::default().fg(accent)),
        )
        .highlight_style(Style::default().bg(Color::DarkGray));
    let mut list_state = ListState::default();
    list_state.select(Some(selected));
    f.render_stateful_widget(list, chunks[0], &mut list_state);

    // ── Live status ──────────────────────────────────────────────────────
    let mut lines: Vec<Line> = Vec::new();

    if let Some(current) = &app.watch.current {
        let name = current
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| current.display().to_string());
        lines.push(Line::styled(
            format!(" ▶ uploading: {name}"),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        ));
    }

    if !app.watch.pending.is_empty() {
        lines.push(Line::styled(
            format!(" ⏳ stabilizing ({}):", app.watch.pending.len()),
            Style::default().fg(Color::DarkGray),
        ));
        for path in app.watch.pending.keys().take(5) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            lines.push(Line::raw(format!("    {name}")));
        }
    }

    if !app.watch.ready.is_empty() {
        lines.push(Line::styled(
            format!(" ◷ queued ({}):", app.watch.ready.len()),
            Style::default().fg(Color::Cyan),
        ));
        for path in app.watch.ready.iter().take(5) {
            let name = path
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string());
            lines.push(Line::raw(format!("    {name}")));
        }
    }

    if lines.is_empty() {
        let msg = if !app.watch.enabled {
            "Set a directory above, then press w to start watching."
        } else {
            "Idle — watching for new files or folders."
        };
        lines.push(Line::styled(
            format!(" {msg}"),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let status = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Status ")
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    f.render_widget(status, chunks[1]);
}
