//! PAR2 generation.
//!
//! A pure-Rust PAR2 (Parity Volume Set 2.0) creator. Parity is produced in the
//! same single read pass used for posting: each slice, as it is read and
//! yEnc-encoded, is also accumulated into the Reed-Solomon recovery buffers.
//!
//! See ROADMAP.md phase 7 for the development plan.
//!
//! - [`gf16`] — the GF(2^16) field and Reed-Solomon matrix (phase 7a)
//! - [`packet`] — PAR2 packet serialization (phase 7b)
//! - [`layout`] — volume-split file layout (phase 7b)
//! - [`encoder`] — streaming Reed-Solomon encoder (phase 7c)

pub mod altmap;
pub mod encoder;
pub mod gf16;
pub mod layout;
pub mod packet;
