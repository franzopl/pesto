//! Configuration screen rendering and field presentation.

use crate::app::App;

use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

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
            label: "Check",
            value: ov
                .check
                .map(on_off)
                .or_else(|| cfg.map(|c| on_off(c.check)))
                .unwrap_or("On")
                .to_string(),
            hint: "Enter/e toggles: streaming STAT check during upload",
            has_override: ov.check.is_some(),
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

pub(super) fn draw(f: &mut Frame, app: &App, area: Rect) {
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
            ov.check.is_some(),
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
