/// Holds the transient status message shown in the bottom bar. Rendering lives
/// in `ui::draw_status_bar`, which combines this message with context-sensitive
/// key hints.
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
}
