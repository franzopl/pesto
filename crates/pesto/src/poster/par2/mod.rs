//! PAR2 recovery-set planning: geometry, memory budget, ingestion and season
//! sets. Kept separate from the posting orchestrator so the numbers that size
//! the encoder do not require reading the hot path.

pub(super) mod geometry;
pub(super) mod memory;
mod season;

pub(crate) use geometry::par2_geometry;
pub(crate) use memory::{address_space_limit, connection_overhead_reserve, par2_memory_plan};
pub use season::{generate_and_write_season_par2, generate_and_write_season_par2_with_progress};
