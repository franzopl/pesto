use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
    Frame,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Normal,
    Error,
    Warn,
}

#[derive(Debug)]
pub struct LogLine {
    pub text: String,
    pub level: LogLevel,
}

#[derive(Debug)]
pub struct LogPanel {
    pub lines: Vec<LogLine>,
    pub max_lines: usize,
    /// 0 = showing the newest lines (auto-scroll)
    /// higher values = scrolled up into history
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    /// Active search query (empty = no filter)
    pub filter: String,
    /// Whether the search input is active (user is typing)
    pub searching: bool,
    list_state: ListState,
}

impl Default for LogPanel {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            max_lines: 500,
            scroll_offset: 0,
            auto_scroll: true,
            filter: String::new(),
            searching: false,
            list_state: ListState::default(),
        }
    }
}

impl LogPanel {
    pub fn new(max_lines: usize) -> Self {
        Self {
            max_lines,
            ..Default::default()
        }
    }

    pub fn push(&mut self, msg: String) {
        self.push_with_level(msg, LogLevel::Normal);
    }

    pub fn push_error(&mut self, msg: String) {
        self.push_with_level(msg, LogLevel::Error);
    }

    pub fn push_warn(&mut self, msg: String) {
        self.push_with_level(msg, LogLevel::Warn);
    }

    fn push_with_level(&mut self, msg: String, level: LogLevel) {
        self.lines.push(LogLine { text: msg, level });
        if self.lines.len() > self.max_lines {
            let excess = self.lines.len() - self.max_lines;
            self.lines.drain(0..excess);
        }
        if self.auto_scroll && self.filter.is_empty() {
            self.scroll_offset = 0;
        }
        self.update_list_state();
    }

    pub fn scroll_up(&mut self, amount: usize) {
        self.auto_scroll = false;
        self.scroll_offset = (self.scroll_offset + amount).min(self.max_scroll());
        self.update_list_state();
    }

    pub fn scroll_down(&mut self, amount: usize) {
        if self.scroll_offset > amount {
            self.scroll_offset -= amount;
        } else {
            self.scroll_offset = 0;
        }
        if self.scroll_offset == 0 {
            self.auto_scroll = true;
        }
        self.update_list_state();
    }

    pub fn scroll_to_top(&mut self) {
        self.auto_scroll = false;
        self.scroll_offset = self.max_scroll();
        self.update_list_state();
    }

    pub fn scroll_to_bottom(&mut self) {
        self.auto_scroll = true;
        self.scroll_offset = 0;
        self.update_list_state();
    }

    pub fn toggle_auto_scroll(&mut self) {
        self.auto_scroll = !self.auto_scroll;
        if self.auto_scroll {
            self.scroll_offset = 0;
        }
        self.update_list_state();
    }

    /// Start search mode.
    pub fn start_search(&mut self) {
        self.searching = true;
        self.scroll_offset = 0;
        self.update_list_state();
    }

    /// Append a character to the active search filter.
    pub fn search_push(&mut self, c: char) {
        self.filter.push(c);
        self.scroll_offset = 0;
        self.update_list_state();
    }

    /// Remove last character from the filter.
    pub fn search_pop(&mut self) {
        self.filter.pop();
        self.scroll_offset = 0;
        self.update_list_state();
    }

    /// Confirm search (keep filter, exit typing mode).
    pub fn search_confirm(&mut self) {
        self.searching = false;
        self.update_list_state();
    }

    /// Clear filter and exit search mode.
    pub fn search_clear(&mut self) {
        self.searching = false;
        self.filter.clear();
        self.scroll_offset = 0;
        self.auto_scroll = true;
        self.update_list_state();
    }

    fn filtered_lines(&self) -> Vec<&LogLine> {
        if self.filter.is_empty() {
            self.lines.iter().collect()
        } else {
            let q = self.filter.to_lowercase();
            self.lines
                .iter()
                .filter(|l| l.text.to_lowercase().contains(&q))
                .collect()
        }
    }

    fn max_scroll(&self) -> usize {
        self.filtered_lines().len().saturating_sub(1)
    }

    fn update_list_state(&mut self) {
        let total = self.filtered_lines().len();
        if total == 0 {
            self.list_state.select(None);
            return;
        }
        let selected = total.saturating_sub(1).saturating_sub(self.scroll_offset);
        self.list_state.select(Some(selected.min(total - 1)));
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        // When searching, reserve one line at the bottom for the input bar
        let (list_area, search_area) = if self.searching || !self.filter.is_empty() {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(1)])
                .split(area);
            (chunks[0], Some(chunks[1]))
        } else {
            (area, None)
        };

        // Collect owned data before the mutable borrow of list_state
        let filter_q = self.filter.to_lowercase();
        let filter_active = !self.filter.is_empty();
        let scroll = self.scroll_offset;

        let snapshot: Vec<(String, LogLevel)> = self
            .filtered_lines()
            .into_iter()
            .map(|ll| (ll.text.clone(), ll.level))
            .collect();

        let total = snapshot.len();
        let height = list_area.height.saturating_sub(2) as usize;
        let start = total.saturating_sub(height).saturating_sub(scroll);

        let items: Vec<ListItem> = snapshot
            .iter()
            .skip(start)
            .take(height)
            .map(|(text, level)| {
                let base_color = match level {
                    LogLevel::Error => Color::Red,
                    LogLevel::Warn => Color::Yellow,
                    LogLevel::Normal => Color::Reset,
                };

                if filter_active {
                    let lower = text.to_lowercase();
                    if let Some(idx) = lower.find(filter_q.as_str()) {
                        let before = &text[..idx];
                        let matched = &text[idx..idx + filter_q.len()];
                        let after = &text[idx + filter_q.len()..];
                        ListItem::new(Line::from(vec![
                            Span::styled(before.to_owned(), Style::default().fg(base_color)),
                            Span::styled(
                                matched.to_owned(),
                                Style::default()
                                    .fg(Color::Black)
                                    .bg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD),
                            ),
                            Span::styled(after.to_owned(), Style::default().fg(base_color)),
                        ]))
                    } else {
                        ListItem::new(Line::from(Span::styled(
                            text.clone(),
                            Style::default().fg(Color::DarkGray),
                        )))
                    }
                } else {
                    ListItem::new(Line::from(Span::styled(
                        text.clone(),
                        Style::default().fg(base_color),
                    )))
                }
            })
            .collect();

        let scroll_hint = if !self.auto_scroll {
            "· manual"
        } else {
            "· auto"
        };
        let filter_hint = if filter_active {
            format!(" · filter:{}", self.filter)
        } else {
            String::new()
        };
        let title = format!(
            " Logs ({}/{}){}  {} ",
            if total == 0 { 0 } else { start + 1 },
            total,
            filter_hint,
            scroll_hint,
        );

        let border_color = if filter_active {
            Color::Yellow
        } else if self.auto_scroll {
            Color::Green
        } else {
            Color::DarkGray
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(border_color)),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        f.render_stateful_widget(list, list_area, &mut self.list_state);

        // Render inline search bar
        if let Some(sa) = search_area {
            let prompt = if self.searching {
                format!("/{}_", self.filter)
            } else {
                format!("/{}  (Enter: confirm · Esc: clear)", self.filter)
            };
            let bar = Paragraph::new(prompt).style(Style::default().fg(Color::Yellow));
            f.render_widget(bar, sa);
        }
    }
}
