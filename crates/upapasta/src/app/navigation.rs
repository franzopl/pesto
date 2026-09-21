//! Top-level screen navigation state.

use super::App;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum AppState {
    #[default]
    Dashboard,
    /// Dedicated upload-queue screen: review, reorder, remove and launch the
    /// queue built in the Browser. The single home for queue management.
    Queue,
    Browser,
    History,
    NzbVault,
    Config,
    /// Watch-mode setup and live status (monitored directory, stabilizing /
    /// queued items, on/off toggle).
    Watch,
}

impl App {
    pub fn next_tab(&mut self) {
        self.state = match self.state {
            AppState::Dashboard => AppState::Queue,
            AppState::Queue => AppState::Browser,
            AppState::Browser => AppState::History,
            AppState::History => AppState::NzbVault,
            AppState::NzbVault => AppState::Config,
            AppState::Config => AppState::Watch,
            AppState::Watch => AppState::Dashboard,
        };
        if self.state == AppState::NzbVault {
            self.load_vault();
        }
        self.log_panel.push(format!("Switched to {:?}", self.state));
    }

    pub fn prev_tab(&mut self) {
        self.state = match self.state {
            AppState::Dashboard => AppState::Watch,
            AppState::Queue => AppState::Dashboard,
            AppState::Browser => AppState::Queue,
            AppState::History => AppState::Browser,
            AppState::NzbVault => AppState::History,
            AppState::Config => AppState::NzbVault,
            AppState::Watch => AppState::Config,
        };
        if self.state == AppState::NzbVault {
            self.load_vault();
        }
    }
}
