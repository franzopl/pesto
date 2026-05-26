use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use ratatui::{
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState},
    Frame,
};

use crate::catalog::NzbStatusEntry;

/// How a file appears in the browser based on its catalog/queue state.
#[derive(Debug, Clone)]
pub enum NzbBadge {
    /// Not in catalog, not marked.
    None,
    /// Marked for queuing (Space key).
    Marked,
    /// Currently being uploaded.
    Uploading,
    /// In catalog — carries the status entry.
    Uploaded(NzbStatusEntry),
}

#[derive(Debug)]
pub struct FileTree {
    pub items: Vec<PathBuf>,
    pub current_dir: PathBuf,
    pub selected: usize,
    pub show_hidden: bool,
    /// Absolute paths that have been marked for queuing (Space key).
    pub marked: HashSet<PathBuf>,
    /// NZB status from the catalog, keyed by original_name (filename or full path).
    pub nzb_status: HashMap<String, NzbStatusEntry>,
    /// Names of files currently being uploaded (basename).
    pub uploading: HashSet<String>,
    /// First visible item index — managed manually to get correct scroll behaviour.
    scroll_offset: usize,
    /// Number of items that fit in the last rendered area; updated at render time.
    visible_height: usize,
}

impl FileTree {
    pub fn new() -> Self {
        let mut tree = Self {
            items: vec![],
            current_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
            selected: 0,
            show_hidden: false,
            marked: HashSet::new(),
            nzb_status: HashMap::new(),
            uploading: HashSet::new(),
            scroll_offset: 0,
            visible_height: 20,
        };
        tree.refresh();
        tree
    }

    /// Replace the NZB status map (called after catalog refresh).
    pub fn set_nzb_status(&mut self, status: HashMap<String, NzbStatusEntry>) {
        self.nzb_status = status;
    }

    /// Mark names that are currently being uploaded.
    pub fn set_uploading(&mut self, names: HashSet<String>) {
        self.uploading = names;
    }

    pub fn select_next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if self.selected + 1 >= self.items.len() {
            // Wrap to top.
            self.selected = 0;
            self.scroll_offset = 0;
        } else {
            self.selected += 1;
            // Scroll only when cursor leaves the visible area.
            let bottom = self.scroll_offset + self.visible_height;
            if self.selected >= bottom {
                self.scroll_offset = self.selected + 1 - self.visible_height;
            }
        }
    }

    pub fn select_previous(&mut self) {
        if self.items.is_empty() {
            return;
        }
        if self.selected == 0 {
            // Wrap to bottom.
            self.selected = self.items.len() - 1;
            self.scroll_offset = self.items.len().saturating_sub(self.visible_height);
        } else {
            self.selected -= 1;
            // Scroll only when cursor leaves the visible area.
            if self.selected < self.scroll_offset {
                self.scroll_offset = self.selected;
            }
        }
    }

    pub fn get_selected(&self) -> Option<&PathBuf> {
        self.items.get(self.selected)
    }

    /// Return the NZB badge for the currently selected item.
    pub fn selected_badge(&self) -> Option<NzbBadge> {
        let path = self.items.get(self.selected)?;
        Some(self.badge_for(path))
    }

    /// Return the NZB badge for a given path.
    pub fn badge_for(&self, path: &PathBuf) -> NzbBadge {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default();
        let full = path.to_string_lossy();

        if self.marked.contains(path) {
            return NzbBadge::Marked;
        }
        if self.uploading.contains(name) {
            return NzbBadge::Uploading;
        }
        if let Some(entry) = self
            .nzb_status
            .get(full.as_ref())
            .or_else(|| self.nzb_status.get(name))
        {
            return NzbBadge::Uploaded(entry.clone());
        }
        NzbBadge::None
    }

    /// Toggle the mark on the currently selected item and advance cursor.
    pub fn toggle_mark(&mut self) {
        if let Some(path) = self.items.get(self.selected).cloned() {
            if self.marked.contains(&path) {
                self.marked.remove(&path);
            } else {
                self.marked.insert(path);
            }
        }
        self.select_next();
    }

    /// Return all marked paths (files and directories). Clears the mark set.
    pub fn take_marked(&mut self) -> Vec<PathBuf> {
        let mut paths: Vec<PathBuf> = self.marked.drain().collect();
        paths.sort();
        paths
    }

    pub fn marked_count(&self) -> usize {
        self.marked.len()
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
                self.scroll_offset = 0;
            }
        }
    }

    pub fn go_to_parent(&mut self) {
        if let Some(parent) = self.current_dir.parent() {
            self.current_dir = parent.to_path_buf();
            self.refresh();
            self.selected = 0;
            self.scroll_offset = 0;
        }
    }

    pub fn toggle_hidden(&mut self) {
        self.show_hidden = !self.show_hidden;
        self.refresh();
    }

    /// Render this FileTree into the given area.
    pub fn render(&mut self, f: &mut Frame, area: Rect, focused: bool) {
        // Keep visible_height in sync so navigation knows how many rows fit.
        self.visible_height = (area.height as usize).saturating_sub(2).max(1);
        let items: Vec<ListItem> = self
            .items
            .iter()
            .enumerate()
            .map(|(i, path)| {
                let badge = self.badge_for(path);
                let icon = if path.is_dir() { "📁" } else { "📄" };
                let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("?");
                let is_selected = i == self.selected;

                let (check, check_style, name_style) = match &badge {
                    NzbBadge::Marked => (
                        "[x] ",
                        Style::default()
                            .fg(Color::Green)
                            .add_modifier(Modifier::BOLD),
                        if is_selected {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::Green)
                        },
                    ),
                    NzbBadge::Uploading => (
                        "[▶] ",
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                        Style::default().fg(Color::Cyan),
                    ),
                    NzbBadge::Uploaded(entry) => {
                        let (sym, color) = badge_symbol(entry);
                        (
                            sym,
                            Style::default().fg(color).add_modifier(Modifier::DIM),
                            if is_selected {
                                Style::default()
                                    .fg(Color::Yellow)
                                    .add_modifier(Modifier::BOLD)
                            } else {
                                Style::default().fg(color).add_modifier(Modifier::DIM)
                            },
                        )
                    }
                    NzbBadge::None => (
                        "[ ] ",
                        Style::default().fg(Color::DarkGray),
                        if is_selected {
                            Style::default()
                                .fg(Color::Yellow)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ),
                };

                ListItem::new(Line::from(vec![
                    Span::styled(check, check_style),
                    Span::styled(format!("{} {}", icon, name), name_style),
                ]))
            })
            .collect();

        let n_marked = self.marked.len();
        let marked_hint = if n_marked > 0 {
            format!(" — {} marked", n_marked)
        } else {
            String::new()
        };

        let title = format!(
            " Browser — {} ({} items{}{}) ",
            self.current_dir.display(),
            self.items.len(),
            if self.show_hidden { " • hidden" } else { "" },
            marked_hint,
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
        *state.offset_mut() = self.scroll_offset;

        f.render_stateful_widget(list, area, &mut state);
    }
}

/// Returns `(badge_string, color)` for a catalog entry.
/// Badge is always 4 chars wide so the list stays aligned.
fn badge_symbol(entry: &NzbStatusEntry) -> (&'static str, Color) {
    match (entry.obfuscated, entry.has_password) {
        (false, false) => ("[✓] ", Color::Green),
        (true, false) => ("[~] ", Color::Yellow),
        (false, true) => ("[P] ", Color::Magenta),
        (true, true) => ("[*] ", Color::Cyan),
    }
}
