//! PAR2 recovery-set planning: geometry, memory budget and (later) ingestion
//! and season sets. Kept separate from the posting orchestrator so the
//! numbers that size the encoder do not require reading the hot path.

pub(super) mod geometry;
pub(super) mod memory;

pub(crate) use geometry::{par2_geometry, par2_geometry_from_sizes};
pub(crate) use memory::{address_space_limit, connection_overhead_reserve, par2_memory_plan};
