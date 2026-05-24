use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Line,
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

#[derive(Debug)]
pub struct LogPanel {
    pub lines: Vec<String>,
    pub max_lines: usize,
    /// 0 = showing the newest lines (auto-scroll)
    /// higher values = scrolled up into history
    pub scroll_offset: usize,
    pub auto_scroll: bool,
    list_state: ListState,
}

impl Default for LogPanel {
    fn default() -> Self {
        Self {
            lines: Vec::new(),
            max_lines: 200,
            scroll_offset: 0,
            auto_scroll: true,
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
        self.lines.push(msg);
        if self.lines.len() > self.max_lines {
            let excess = self.lines.len() - self.max_lines;
            self.lines.drain(0..excess);
        }

        if self.auto_scroll {
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

    fn max_scroll(&self) -> usize {
        self.lines.len().saturating_sub(1)
    }

    fn update_list_state(&mut self) {
        let total = self.lines.len();
        if total == 0 {
            self.list_state.select(None);
            return;
        }
        let selected = total.saturating_sub(1).saturating_sub(self.scroll_offset);
        self.list_state.select(Some(selected.min(total - 1)));
    }

    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let total = self.lines.len();

        // Simple window calculation
        let height = area.height.saturating_sub(2) as usize; // borders
        let start = total
            .saturating_sub(height)
            .saturating_sub(self.scroll_offset);

        let items: Vec<ListItem> = self
            .lines
            .iter()
            .skip(start)
            .take(height)
            .map(|l| ListItem::new(Line::from(l.as_str())))
            .collect();

        let title = format!(
            " Logs ({}/{}) {} ",
            if total == 0 { 0 } else { start + 1 },
            total,
            if self.auto_scroll {
                "· auto"
            } else {
                "· manual"
            }
        );

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(if self.auto_scroll {
                        Style::default().fg(Color::Green)
                    } else {
                        Style::default().fg(Color::Yellow)
                    }),
            )
            .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

        f.render_stateful_widget(list, area, &mut self.list_state);
    }
}
