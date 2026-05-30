use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

#[derive(Debug, Default)]
pub struct UploadQueue {
    pub items: Vec<String>,
    pub active: usize,
    pub selected: usize,
}

impl UploadQueue {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the path if absent, remove it if present. Returns `true` when the
    /// item is now queued, `false` when it was removed. This is the single
    /// mutation used by the Browser's `Space` key, keeping the queue the one
    /// source of truth for what will be uploaded.
    pub fn toggle(&mut self, path: String) -> bool {
        if let Some(pos) = self.items.iter().position(|p| p == &path) {
            self.items.remove(pos);
            if self.selected >= self.items.len() && !self.items.is_empty() {
                self.selected = self.items.len() - 1;
            }
            false
        } else {
            self.items.push(path);
            true
        }
    }

    pub fn remove_selected(&mut self) -> Option<String> {
        if self.items.is_empty() {
            return None;
        }
        let idx = self.selected.min(self.items.len() - 1);
        let removed = self.items.remove(idx);
        if self.selected >= self.items.len() && !self.items.is_empty() {
            self.selected = self.items.len() - 1;
        }
        if self.active > self.items.len() {
            self.active = self.items.len();
        }
        Some(removed)
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.active = 0;
        self.selected = 0;
    }

    pub fn select_next(&mut self) {
        if !self.items.is_empty() {
            self.selected = (self.selected + 1) % self.items.len();
        }
    }

    pub fn select_previous(&mut self) {
        if !self.items.is_empty() {
            self.selected = if self.selected == 0 {
                self.items.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    /// Move the selected item one position up in the queue.
    pub fn move_selected_up(&mut self) {
        if self.items.len() < 2 || self.selected == 0 {
            return;
        }
        self.items.swap(self.selected, self.selected - 1);
        self.selected -= 1;
    }

    /// Move the selected item one position down in the queue.
    pub fn move_selected_down(&mut self) {
        if self.items.len() < 2 || self.selected + 1 >= self.items.len() {
            return;
        }
        self.items.swap(self.selected, self.selected + 1);
        self.selected += 1;
    }

    #[allow(dead_code)]
    pub fn render(&mut self, f: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, item)| {
                let prefix = if i < self.active { "▶ " } else { "  " };
                let style = if i == self.selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Line::from(vec![
                    Span::styled(prefix, Style::default().fg(Color::Green)),
                    Span::styled(item, style),
                ]))
            })
            .collect();

        let title = format!(
            " Upload Queue ({}/{}) ",
            self.items.len(),
            if self.active > 0 { "uploading" } else { "idle" }
        );

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(Style::default().fg(Color::Cyan)),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );

        let mut state = ListState::default();
        if !self.items.is_empty() {
            state.select(Some(self.selected));
        }

        f.render_stateful_widget(list, area, &mut state);
    }
}
