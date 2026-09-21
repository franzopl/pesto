//! Configuration regression tests, grouped by config domain.
//!
//! The production configuration module owns resolution, parsing, validation
//! and the TOML model. These suites follow the same boundaries so a change to
//! one domain only requires loading its focused tests.

use crate::config::*;

mod behavior;
mod cli_overrides;
mod defaults;
mod loading;
mod obfuscation;
mod parsing;
mod servers;
mod toml_sections;

fn base_overrides() -> Overrides {
    Overrides {
        groups: Some(vec!["alt.test".into()]),
        ..Default::default()
    }
}

fn minimal_file() -> FileConfig {
    let mut f = FileConfig::default();
    f.server.host = Some("h".into());
    f.posting.groups = Some(vec!["alt.test".into()]);
    f
}
