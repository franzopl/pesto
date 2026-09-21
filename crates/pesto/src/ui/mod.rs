//! Upload progress presentation.
//!
//! Module map: `state` holds the render state, `reducer` applies progress
//! events to it, `metrics` and `format` compute and format derived values,
//! and `terminal`, `render` and `wizard` emit the concrete presentations.

mod format;
mod metrics;
mod reducer;
pub mod render;
mod state;
pub mod terminal;
pub mod wizard;
