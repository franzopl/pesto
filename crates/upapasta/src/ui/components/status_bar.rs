use ratatui::{
    layout::Rect,
    style::{Color, Style},
    widgets::{Block, Borders, Paragraph},
    Frame,
};

#[derive(Debug, Default)]
pub struct StatusBar {
    pub message: String,
}

impl StatusBar {
    pub fn new(message: String) -> Self {
        Self { message }
    }

    pub fn set(&mut self, msg: impl Into<String>) {
        self.message = msg.into();
    }

    pub fn render(&self, f: &mut Frame, area: Rect) {
        let help = format!("{}  |  Tab: switch  •  q: quit", self.message);

        let status = Paragraph::new(help)
            .style(Style::default().fg(Color::DarkGray))
            .block(Block::default().borders(Borders::TOP).title(" Status "));

        f.render_widget(status, area);
    }
}
