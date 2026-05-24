use std::path::PathBuf;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::Span,
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

#[derive(Debug)]
pub struct FileTree {
    pub items: Vec<PathBuf>,
    pub current_dir: PathBuf,
    pub selected: usize,
    pub show_hidden: bool,
}

impl FileTree {
    pub fn new() -> Self {
        let mut tree = Self {
            items: vec![],
            current_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            selected: 0,
            show_hidden: false,
        };
        tree.refresh();
        tree
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

    pub fn get_selected(&self) -> Option<&PathBuf> {
        self.items.get(self.selected)
    }

    pub fn refresh(&mut self) {
        if let Ok(entries) = std::fs::read_dir(&self.current_dir) {
            let mut items: Vec<PathBuf> = entries
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    if self.show_hidden {
                        true
                    } else {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|s| !s.starts_with('.'))
                            .unwrap_or(false)
                    }
                })
                .collect();

            items.sort_by(|a, b| {
                let a_is_dir = a.is_dir();
                let b_is_dir = b.is_dir();
                if a_is_dir != b_is_dir {
                    b_is_dir.cmp(&a_is_dir)
                } else {
                    a.file_name().cmp(&b.file_name())
                }
            });

            self.items = items;
            if self.selected >= self.items.len() {
                self.selected = 0;
            }
        }
    }

    pub fn go_to_parent(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.refresh();
            self.selected = 0;
        }
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.refresh();
    }

    /// Render this FileTree into the given area.
    pub fn render(&self, f: &mut Frame, area: Rect, focused: bool) {
        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let icon = if path.is_dir() { "📁" } else { "📄" };
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let style = if i == self.selected {
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                ListItem::new(Span::styled(format!("{} {}", icon, name), style))
            })
            .collect();

        let title = format!(
            " Browser — {} ({} items{}) ",
            self.current_dir.display(),
            self.items.len(),
            if self.show_hidden { " • hidden" } else { "" }
        );

        let border_style = if focused {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };

        let list = List::new(items)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .border_style(border_style),
            )
            .highlight_style(
                Style::default()
                    .fg(Color::Black)
                    .bg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            );

        let mut state = ListState::default();
        state.select(Some(self.selected));

        f.render_stateful_widget(list, area, &mut state);
    }
}
