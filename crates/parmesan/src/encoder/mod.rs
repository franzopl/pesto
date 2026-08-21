//! Streaming Reed-Solomon recovery encoder and file hashers.
//!
//! [`RecoveryEncoder`] accepts input slices one at a time and accumulates each
//! one into every recovery buffer, so a file can be read a single time. After
//! the last slice the buffers hold the finished recovery data.
//!
//! Each slice is interpreted as a sequence of little-endian 16-bit GF(2^16)
//! words (matching `par2cmdline`). Recovery word `k` of the block with
//! exponent `e` is `XOR over inputs j of coeff(j, e) * input_j[k]`, where
//! `coeff(j, e) = 2^(logbase_j * e)`.

use md5::{Digest, Md5};
use rayon::prelude::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
use std::collections::TryReserveError;

#[cfg(target_arch = "aarch64")]
use std::arch::aarch64::poly16x8_t;

#[cfg(target_arch = "x86_64")]
use super::gf16::xor_dep_matrix;
use super::gf16::{input_logbases, Gf16, ORDER};
use super::packet::SliceChecksum;
use crate::SimdPath;

/// Bytes covered by the per-file 16k hash.
const HEAD_LEN: usize = 16 * 1024;

mod affine512;
mod affine_kernels;
mod api;
mod buffers;
mod fallback;
mod flush;
mod hash;
mod shuffle_kernels;
mod tables;

pub use buffers::{
    affine2x_buffer_size, affine512_kernel_available, affine_buffer_size, affine_kernel_available,
    altmap_buffer_size, altmap_kernel_available, shuffle2x_buffer_size, shuffle2x_kernel_available,
    shuffle512_kernel_available,
};
pub use hash::{slice_checksum, FileHasher, FileHashes};

use crate::{affine, affine2x, altmap, gf16, shuffle2x};
use affine512::*;
use buffers::*;
use tables::*;

/// One finished recovery slice.
#[derive(Debug, Clone)]
pub struct RecoverySlice {
    /// Reed-Solomon exponent of this recovery block.
    pub exponent: u32,
    /// Recovery slice bytes (length equal to the slice size).
    pub data: Vec<u8>,
}

/// Accumulates input slices into Reed-Solomon recovery buffers.
pub struct RecoveryEncoder {
    gf: Gf16,
    /// Number of 16-bit words per slice.
    slice_words: usize,
    /// `logbase` exponent of each input slice, by global slice index.
    logbases: Vec<u32>,
    /// The starting exponent for the first buffer.
    exponent_start: u32,
    /// One accumulator buffer per recovery block; index = recovery exponent - exponent_start.
    buffers: RecoveryBufferSet,
    /// Number of input slices fed so far.
    next_index: usize,
    /// Queue of input slices waiting to be processed (cache blocking).
    queued_slices: Vec<Vec<u8>>,
    /// Reusable buffer pool — slices that were consumed in the last flush keep
    /// their allocation here so the producer can pick them back up via
    /// [`take_buffer`] instead of asking the allocator for a fresh page.
    free_buffers: Vec<Vec<u8>>,
    /// Reused Affine shuffle-prepare outputs (parpar keeps a prepare scratch
    /// instead of allocating a full extra copy of every queued slice).
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    affine_prepare: Vec<Vec<u8>>,
    /// Maximum bytes to queue before flushing.
    flush_limit_bytes: usize,
    /// When true each flush also computes per-slice MD5+CRC32 checksums in
    /// parallel with the Reed-Solomon work and accumulates them here.
    compute_checksums: bool,
    pending_checksums: Vec<SliceChecksum>,
    /// Manual override for the SIMD multiplication backend.
    pub(super) simd_path: SimdPath,
    /// Force a specific SIMD path instead of auto-detecting; only available
    /// when built with the `bench-internals` Cargo feature.
    #[cfg(feature = "bench-internals")]
    forced_path: Option<BenchPath>,
    /// XOR bit-dependency matrices for all 65536 GF(2^16) coefficients.
    /// `dep_tables[n][k]` is the bitmask of input bits that XOR into output bit `k`
    /// when multiplying by coefficient `n`. Allocated at construction time on
    /// AVX2-without-GFNI hardware, where it drives the ALTMAP kernel (27e).
    /// `None` on GFNI hardware (which uses `GF2P8AFFINEQB` instead) and on
    /// non-x86 targets.
    #[cfg(target_arch = "x86_64")]
    dep_tables: Option<Box<[[u16; 16]; 65536]>>,
}

/// Selects which SIMD flush path to use when `bench-internals` is enabled.
/// Lets benchmarks bypass runtime dispatch and compare paths directly.
#[cfg(feature = "bench-internals")]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum BenchPath {
    Scalar,
    #[cfg(target_arch = "x86_64")]
    Ssse3,
    #[cfg(target_arch = "x86_64")]
    Avx2,
    #[cfg(target_arch = "x86_64")]
    Avx2Gfni,
    #[cfg(target_arch = "x86_64")]
    Avx512Gfni,
    #[cfg(target_arch = "x86_64")]
    Avx2Altmap,
    #[cfg(target_arch = "x86_64")]
    Avx2Shuffle2x,
    #[cfg(target_arch = "aarch64")]
    NeonClmul,
}

#[cfg(test)]
mod tests;
