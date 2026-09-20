use crate::app::{App, AppState};
mod browser;
pub mod components;
mod config;
mod dashboard;
mod helpers;
mod history;
mod overlays;
mod queue;
pub mod theme;
mod vault;
mod watch;

use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
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
        overlays::prowlarr::draw_search(f, app, area);
    }

    // Queue batch-search progress floats above everything too.
    if app.prowlarr.batch.is_some() {
        overlays::prowlarr::draw_batch(f, app, area);
    }

    // Hook picker floats above everything.
    if app.hook_picker.is_some() {
        overlays::hook_picker::draw(f, app, area);
    }
}

/// Single-line top bar: brand on the left, tab strip in the middle, version on
/// the right. When `rule` is true a thin separator is drawn on the row below,
/// giving structure without stacking bordered boxes.
fn draw_top_bar(f: &mut Frame, app: &App, area: Rect, rule: bool) {
    const TABS: [(&str, AppState); 7] = [
        ("Dashboard", AppState::Dashboard),
        ("Queue", AppState::Queue),
        ("Browser", AppState::Browser),
        ("History", AppState::History),
        ("Vault", AppState::NzbVault),
        ("Config", AppState::Config),
        ("Watch", AppState::Watch),
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

    // Right-aligned version tag (with a WATCH chip when watch mode is on),
    // drawn first so the tab strip can overlay the left.
    let mut right = Vec::new();
    if app.watch.enabled {
        right.push(Span::styled(
            "WATCH ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ));
    }
    right.push(Span::styled("v2 ", theme::label()));
    f.render_widget(
        Paragraph::new(Line::from(right)).alignment(Alignment::Right),
        strip_area,
    );
    f.render_widget(Paragraph::new(Line::from(spans)), strip_area);
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    match app.state {
        AppState::Browser => {
            browser::draw(f, app, area);
        }
        AppState::Dashboard => {
            dashboard::draw(f, app, area);
        }
        AppState::Queue => {
            queue::draw(f, app, area);
        }
        AppState::History => {
            history::draw(f, app, area);
            if app.history.nzb_viewer.is_some() {
                overlays::nzb_viewer::draw(f, app, area);
            }
        }
        AppState::NzbVault => {
            vault::draw(f, app, area);
            if app.vault.viewer.is_some() {
                overlays::vault_viewer::draw(f, app, area);
            }
        }
        AppState::Config => {
            config::draw(f, app, area);
        }
        AppState::Watch => {
            watch::draw(f, app, area);
        }
    }
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
        if app.progress.is_paused {
            "p resume · x cancel · Tab switch · q quit".into()
        } else {
            "p pause · x cancel · Tab switch · q quit".into()
        }
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
    } else if app.state == AppState::Watch && app.watch.editing {
        "Enter confirm · Esc cancel edit".into()
    } else if app.state == AppState::Watch {
        let toggle = if app.watch.enabled {
            "w stop"
        } else {
            "w start"
        };
        format!("j/k move · Enter/e edit · {toggle} · Tab switch")
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
