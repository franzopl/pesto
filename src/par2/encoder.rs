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

use super::gf16::{input_logbases, xor_dep_matrix, Gf16, ORDER};
use super::packet::{md5, SliceChecksum};
use crate::yenc::crc32;

/// Bytes covered by the per-file 16k hash.
const HEAD_LEN: usize = 16 * 1024;

/// Pre-computed AVX-512/GFNI coefficient table for one (recovery_block, input_slice) pair.
/// Two 512-bit matrix registers (mat_lo, mat_hi) plus 256-entry scalar lookup tables.
#[cfg(target_arch = "x86_64")]
type Avx512GfniTable = (__m512i, __m512i, [u16; 256], [u16; 256]);

/// Pre-computed AVX2/GFNI coefficient table for one (recovery_block, input_slice) pair.
/// Two 256-bit matrix registers (mat_lo, mat_hi) plus 256-entry scalar lookup tables.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
type Avx2GfniTable = (__m256i, __m256i, [u16; 256], [u16; 256]);

/// Pre-computed SSSE3 coefficient table for one (recovery_block, input_slice) pair.
/// Eight 128-bit shuffle vectors plus 256-entry scalar lookup tables.
#[cfg(target_arch = "x86_64")]
type Ssse3Table = (
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    [u16; 256],
    [u16; 256],
);

/// Pre-computed AVX2/Shuffle2x coefficient table for one (recovery_block, input_slice) pair.
/// Four 256-bit shuffle vectors where each `__m256i` packs two 16-entry nibble tables
/// into its two 128-bit lanes, enabling the Shuffle2x kernel to use 4 PSHUFB instead of 8.
///
/// Layout (where loNk[n] = (gf.mul(n<<4k, c) & 0xFF), hiNk[n] = (gf.mul(n<<4k, c) >> 8)):
///   tNormA: lane0 = loN0, lane1 = hiN2
///   tNormB: lane0 = loN1, lane1 = hiN3
///   tSwapA: lane0 = loN2, lane1 = hiN0
///   tSwapB: lane0 = loN3, lane1 = hiN1
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
type Avx2Shuffle2xTable = (
    __m256i,    // tNormA
    __m256i,    // tNormB
    __m256i,    // tSwapA
    __m256i,    // tSwapB
    [u16; 256], // scalar table_low  (fallback / for the test harness)
    [u16; 256], // scalar table_high
);

/// Pre-computed AVX2 coefficient table for one (recovery_block, input_slice) pair.
/// Eight 256-bit shuffle vectors covering the four nibble × two byte-half combinations,
/// plus full 256-entry lookup tables for the scalar tail handler.
#[cfg(target_arch = "x86_64")]
type Avx2Table = (
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    [u16; 256],
    [u16; 256],
);

/// Storage for recovery accumulator buffers.
///
/// `Normal` holds one `Vec<u16>` per recovery block (the existing layout).
/// `Altmap` holds one `Vec<u8>` per recovery block in ALTMAP bit-plane format
/// (Phase 27d/27e); both variants occupy the same total memory.
/// `Shuffle2x` holds one `Vec<u8>` per recovery block in the Shuffle2x layout
/// (Phase 28a): lo-bytes in lane 0, hi-bytes in lane 1 of each 32-byte chunk.
pub(super) enum RecoveryBufferSet {
    Normal(Vec<Vec<u16>>),
    /// Each inner `Vec<u8>` has length `altmap_size(slice_words)` = `slice_words * 2`.
    Altmap(Vec<Vec<u8>>),
    /// Each inner `Vec<u8>` has length `shuffle2x_buffer_size(slice_words)` = `slice_words * 2`.
    Shuffle2x(Vec<Vec<u8>>),
}

impl RecoveryBufferSet {
    /// Borrow the normal (u16) buffers.  Panics when called on the Altmap/Shuffle2x variant.
    pub(super) fn as_normal_mut(&mut self) -> &mut Vec<Vec<u16>> {
        match self {
            Self::Normal(b) => b,
            Self::Altmap(_) | Self::Shuffle2x(_) => panic!("expected Normal recovery buffers"),
        }
    }

    /// Number of recovery blocks.
    #[allow(dead_code)]
    pub(super) fn len(&self) -> usize {
        match self {
            Self::Normal(b) => b.len(),
            Self::Altmap(b) | Self::Shuffle2x(b) => b.len(),
        }
    }
}

/// Returns the size in bytes of one ALTMAP recovery buffer for `slice_words`
/// GF(2^16) words.  Equal to `slice_words * 2` — same footprint as a
/// `Vec<u16>` of `slice_words` elements.
///
/// # Panics
///
/// Panics if `slice_words` is not a multiple of 16.
pub fn altmap_buffer_size(slice_words: usize) -> usize {
    super::altmap::altmap_size(slice_words)
}

/// Returns the size in bytes of one Shuffle2x recovery buffer for `slice_words`
/// GF(2^16) words.  Equal to `slice_words * 2` — same footprint as normal layout.
///
/// # Panics
///
/// Panics if `slice_words` is not a multiple of 16.
pub fn shuffle2x_buffer_size(slice_words: usize) -> usize {
    super::shuffle2x::shuffle2x_buffer_size(slice_words)
}

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
    /// Maximum bytes to queue before flushing.
    flush_limit_bytes: usize,
    /// When true each flush also computes per-slice MD5+CRC32 checksums in
    /// parallel with the Reed-Solomon work and accumulates them here.
    compute_checksums: bool,
    pending_checksums: Vec<SliceChecksum>,
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
}

impl RecoveryEncoder {
    /// Create an encoder for `total_input_slices` input slices of `slice_size`
    /// bytes each, producing `recovery_count` recovery blocks (exponents
    /// `exponent_start..exponent_start + recovery_count`).
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 4.
    pub fn new(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        assert!(
            slice_size > 0 && slice_size.is_multiple_of(4),
            "slice size must be a positive multiple of 4"
        );
        Self {
            gf: Gf16::new(),
            slice_words: slice_size / 2,
            logbases: input_logbases(total_input_slices),
            exponent_start,
            buffers: RecoveryBufferSet::Normal(vec![vec![0u16; slice_size / 2]; recovery_count]),
            next_index: 0,
            queued_slices: Vec::with_capacity(64),
            free_buffers: Vec::new(),
            flush_limit_bytes: 256 * 1024 * 1024,
            compute_checksums: false,
            pending_checksums: Vec::new(),
            #[cfg(feature = "bench-internals")]
            forced_path: None,
            #[cfg(target_arch = "x86_64")]
            dep_tables: Self::build_dep_tables(),
        }
    }

    /// Build the XOR dependency table for every GF(2^16) coefficient.
    ///
    /// Only allocated on AVX2-without-GFNI hardware, where the ALTMAP kernel
    /// (Phase 27e) will use it. Returns `None` on GFNI machines and when AVX2
    /// is unavailable.
    #[cfg(target_arch = "x86_64")]
    fn build_dep_tables() -> Option<Box<[[u16; 16]; 65536]>> {
        if !std::is_x86_feature_detected!("avx2") || std::is_x86_feature_detected!("gfni") {
            return None;
        }
        // Heap-allocate 2 MB without touching the stack. `alloc_zeroed` is
        // stable since Rust 1.28 and pre-zeros the memory (index 0 stays [0u16; 16]).
        let mut table: Box<[[u16; 16]; 65536]> = unsafe {
            let layout = std::alloc::Layout::new::<[[u16; 16]; 65536]>();
            Box::from_raw(std::alloc::alloc_zeroed(layout).cast())
        };
        for n in 1u16..=65535 {
            table[n as usize] = xor_dep_matrix(n);
        }
        Some(table)
    }

    /// Create an encoder that stores recovery buffers in ALTMAP bit-plane format.
    ///
    /// Identical to [`new`] in every respect except that the internal recovery
    /// buffers use the ALTMAP layout (Phase 27d/27e).  The `flush_avx2_altmap`
    /// path (27e) will use these directly; `finish()` converts them back to
    /// normal layout before returning `RecoverySlice`s.
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 32 bytes (= 16
    /// u16 words, the ALTMAP group size).
    pub fn new_altmap(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        assert!(
            slice_size > 0 && slice_size.is_multiple_of(32),
            "ALTMAP encoder requires slice_size to be a positive multiple of 32 bytes, got {slice_size}"
        );
        let slice_words = slice_size / 2;
        let buf_bytes = altmap_buffer_size(slice_words);
        Self {
            gf: Gf16::new(),
            slice_words,
            logbases: input_logbases(total_input_slices),
            exponent_start,
            buffers: RecoveryBufferSet::Altmap(vec![vec![0u8; buf_bytes]; recovery_count]),
            next_index: 0,
            queued_slices: Vec::with_capacity(64),
            free_buffers: Vec::new(),
            flush_limit_bytes: 256 * 1024 * 1024,
            compute_checksums: false,
            pending_checksums: Vec::new(),
            #[cfg(feature = "bench-internals")]
            forced_path: None,
            #[cfg(target_arch = "x86_64")]
            dep_tables: Self::build_dep_tables(),
        }
    }

    /// Create an encoder that stores recovery buffers in Shuffle2x layout.
    ///
    /// Identical to [`new`] in every respect except that the internal recovery
    /// buffers use the Shuffle2x layout (Phase 28a): lo-bytes in lane 0, hi-bytes
    /// in lane 1 of each 32-byte chunk.  The `flush_avx2_shuffle2x` path will
    /// use these directly; `finish()` converts them back to normal layout before
    /// returning `RecoverySlice`s.
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 32 bytes.
    pub fn new_shuffle2x(
        slice_size: usize,
        total_input_slices: usize,
        exponent_start: u32,
        recovery_count: usize,
    ) -> Self {
        assert!(
            slice_size > 0 && slice_size.is_multiple_of(32),
            "Shuffle2x encoder requires slice_size to be a positive multiple of 32 bytes, got {slice_size}"
        );
        let slice_words = slice_size / 2;
        let buf_bytes = shuffle2x_buffer_size(slice_words);
        Self {
            gf: Gf16::new(),
            slice_words,
            logbases: input_logbases(total_input_slices),
            exponent_start,
            buffers: RecoveryBufferSet::Shuffle2x(vec![vec![0u8; buf_bytes]; recovery_count]),
            next_index: 0,
            queued_slices: Vec::with_capacity(64),
            free_buffers: Vec::new(),
            flush_limit_bytes: 256 * 1024 * 1024,
            compute_checksums: false,
            pending_checksums: Vec::new(),
            #[cfg(feature = "bench-internals")]
            forced_path: None,
            // Shuffle2x never uses dep_tables (those are only for the ALTMAP path).
            #[cfg(target_arch = "x86_64")]
            dep_tables: None,
        }
    }

    /// Set the maximum bytes to queue before flushing.
    pub fn with_flush_limit(mut self, bytes: usize) -> Self {
        self.flush_limit_bytes = bytes;
        self
    }

    /// Force a specific SIMD flush path, bypassing runtime auto-detection.
    /// Only available with the `bench-internals` Cargo feature.
    #[cfg(feature = "bench-internals")]
    pub fn with_forced_path(mut self, path: BenchPath) -> Self {
        self.forced_path = Some(path);
        self
    }

    /// Enable parallel per-slice MD5+CRC32 checksum computation.
    /// Each flush will compute checksums alongside RS recovery using `rayon::join`.
    /// Call [`drain_checksums`] after [`finish`] to retrieve them in slice order.
    pub fn with_checksums(mut self) -> Self {
        self.compute_checksums = true;
        self
    }

    /// Return and clear all checksums accumulated so far (in input-slice order).
    pub fn drain_checksums(&mut self) -> Vec<SliceChecksum> {
        std::mem::take(&mut self.pending_checksums)
    }

    /// Hand the producer an empty, slice-sized `Vec<u8>` — either a buffer
    /// recycled from a previous flush or a fresh allocation. Returning the
    /// buffer to the encoder via [`add_slice`] keeps the pool refilled.
    pub fn take_buffer(&mut self) -> Vec<u8> {
        let slice_size = self.slice_words * 2;
        if let Some(mut buf) = self.free_buffers.pop() {
            buf.clear();
            if buf.capacity() < slice_size {
                buf.reserve_exact(slice_size - buf.capacity());
            }
            buf
        } else {
            Vec::with_capacity(slice_size)
        }
    }

    /// Move consumed queue buffers into the free-list (preserving their
    /// allocations) and restore the empty queue.
    fn recycle_queue(&mut self, mut queued: Vec<Vec<u8>>) {
        self.free_buffers.reserve(queued.len());
        for mut buf in queued.drain(..) {
            buf.clear();
            self.free_buffers.push(buf);
        }
        self.queued_slices = queued;
    }

    /// Remove all currently pooled free buffers and return them to the caller.
    ///
    /// Used by the background-worker path in `poster.rs` to ferry recycled
    /// slice allocations back to the producer without exposing `free_buffers`
    /// directly.
    pub fn drain_free_buffers(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.free_buffers)
    }

    /// Feed one input slice, already zero-padded to the slice size.
    ///
    /// Ownership of `slice` is taken so the encoder can queue it for batched
    /// processing without an extra copy on the read hot path.
    ///
    /// # Panics
    ///
    /// Panics if the slice length is wrong or more slices are fed than the
    /// `total_input_slices` declared at construction.
    pub fn add_slice(&mut self, slice: Vec<u8>) {
        assert_eq!(
            slice.len(),
            self.slice_words * 2,
            "slice length must equal the slice size"
        );
        self.queued_slices.push(slice);

        // Process if we hit the count limit (cache blocking) or a memory limit
        // (to keep the footprint lean). 256 MB is enough to amortize the flush
        // cost even for very few slices.
        let queued_bytes = self.queued_slices.len() * self.slice_words * 2;
        if self.queued_slices.len() >= 128 || queued_bytes >= self.flush_limit_bytes {
            self.flush();
        }
    }
    fn flush(&mut self) {
        if self.queued_slices.is_empty() {
            return;
        }

        // ALTMAP path: AVX2 XOR bit-dependency kernel (Phase 27e).
        #[cfg(target_arch = "x86_64")]
        if matches!(self.buffers, RecoveryBufferSet::Altmap(_)) {
            if std::is_x86_feature_detected!("avx2") {
                unsafe {
                    self.flush_avx2_altmap();
                }
                return;
            }
            // No AVX2: ALTMAP path is unsupported; drain without processing.
            let queued = std::mem::take(&mut self.queued_slices);
            self.next_index += queued.len();
            self.recycle_queue(queued);
            return;
        }

        // Shuffle2x path: AVX2 nibble-shuffle kernel with Shuffle2x buffer layout (Phase 28b).
        #[cfg(target_arch = "x86_64")]
        if matches!(self.buffers, RecoveryBufferSet::Shuffle2x(_)) {
            if std::is_x86_feature_detected!("avx2") {
                unsafe { self.flush_avx2_shuffle2x() };
                return;
            }
            // No AVX2: Shuffle2x path is unsupported; drain without processing.
            let queued = std::mem::take(&mut self.queued_slices);
            self.next_index += queued.len();
            self.recycle_queue(queued);
            return;
        }

        // When bench-internals is active a forced path overrides auto-detection.
        #[cfg(feature = "bench-internals")]
        if let Some(path) = self.forced_path {
            match path {
                BenchPath::Scalar => {
                    self.flush_scalar();
                    return;
                }
                #[cfg(target_arch = "x86_64")]
                BenchPath::Ssse3 => unsafe {
                    self.flush_ssse3();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx2 => unsafe {
                    self.flush_avx2();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx2Gfni => unsafe {
                    self.flush_avx2_gfni();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx512Gfni => unsafe {
                    self.flush_avx512_gfni();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx2Altmap => unsafe {
                    self.flush_avx2_altmap();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx2Shuffle2x => unsafe {
                    self.flush_avx2_shuffle2x();
                    return;
                },
            }
        }

        // AVX-512+GFNI path: verified correct on Intel Ice Lake Xeon (AWS m6i)
        // via gfni_recovery_matches_scalar (bench-internals feature).
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && std::is_x86_feature_detected!("gfni")
        {
            unsafe {
                self.flush_avx512_gfni();
            }
            return;
        }

        // AVX2+GFNI path: verified correct on i5-14400 (simd_recovery_matches_scalar).
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("gfni") {
            unsafe {
                self.flush_avx2_gfni();
            }
            return;
        }

        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") {
            unsafe {
                self.flush_avx2();
            }
            return;
        }

        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("ssse3") {
            unsafe {
                self.flush_ssse3();
            }
            return;
        }

        // NEON is mandatory on AArch64; no runtime check required.
        #[cfg(target_arch = "aarch64")]
        {
            unsafe {
                self.flush_neon();
            }
            return;
        }

        self.flush_scalar();
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn flush_avx2(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        unsafe {
            Self::flush_avx2_work(
                self.buffers.as_normal_mut(),
                &queued,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
        }

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn flush_avx2_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        let mask_f = _mm256_set1_epi8(0x0F);
        let mask_even = _mm256_set1_epi16(0x00FF);

        let n_rec = buffers.len();
        let n_queued = queued.len();

        // Pre-build all SIMD coefficient tables in a single parallel pass — one Vec
        // entry per (recovery_block × input_slice) pair, laid out as [rec * n_queued + q].
        //
        // Building tables outside the chunk loop means they are computed once per flush
        // rather than once per (flush × chunk). The chunk loop below can then reference
        // pre-built tables without any GF-table lookups in the hot path.
        //
        // __m256i is Send+Sync (it is [i64; 4] under the hood) so storing it in a Vec
        // that is shared across rayon tasks is safe.
        let all_tables: Vec<Avx2Table> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| unsafe {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut tl_l = [0u8; 16];
                let mut tl_h = [0u8; 16];
                let mut th_l = [0u8; 16];
                let mut th_h = [0u8; 16];
                let mut hl_l = [0u8; 16];
                let mut hl_h = [0u8; 16];
                let mut hh_l = [0u8; 16];
                let mut hh_h = [0u8; 16];

                for val in 0..16usize {
                    let r0 = gf.mul(val as u16, coeff);
                    tl_l[val] = (r0 & 0xFF) as u8;
                    th_l[val] = (r0 >> 8) as u8;
                    let r1 = gf.mul((val as u16) << 4, coeff);
                    tl_h[val] = (r1 & 0xFF) as u8;
                    th_h[val] = (r1 >> 8) as u8;
                    let r2 = gf.mul((val as u16) << 8, coeff);
                    hl_l[val] = (r2 & 0xFF) as u8;
                    hh_l[val] = (r2 >> 8) as u8;
                    let r3 = gf.mul((val as u16) << 12, coeff);
                    hl_h[val] = (r3 & 0xFF) as u8;
                    hh_h[val] = (r3 >> 8) as u8;
                }

                let v_tl_l =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(tl_l.as_ptr() as *const __m128i));
                let v_tl_h =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(tl_h.as_ptr() as *const __m128i));
                let v_th_l =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(th_l.as_ptr() as *const __m128i));
                let v_th_h =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(th_h.as_ptr() as *const __m128i));
                let v_hl_l =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(hl_l.as_ptr() as *const __m128i));
                let v_hl_h =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(hl_h.as_ptr() as *const __m128i));
                let v_hh_l =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(hh_l.as_ptr() as *const __m128i));
                let v_hh_h =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(hh_h.as_ptr() as *const __m128i));

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                (
                    v_tl_l, v_tl_h, v_th_l, v_th_h, v_hl_l, v_hl_h, v_hh_l, v_hh_h, table_low,
                    table_high,
                )
            })
            .collect();

        // 2D parallel loop: outer dimension = recovery block pairs (91 tasks for 183
        // blocks), inner dimension = 32 KiB chunks of each recovery buffer (960 chunks
        // for a 30 MiB slice). Total rayon tasks = 91 × 960 = ~87 K, saturating all
        // available cores instead of the previous 91-task ceiling.
        //
        // Each rayon task handles a group of consecutive recovery blocks (4× unrolling
        // over the recovery dimension). One input load + one nibble decomposition serves
        // all blocks in the group, halving the load and AND/SRL overhead per byte processed.
        let chunk_size = 16384usize; // 32 KiB recovery buffer chunk (see avx2_gfni A/B notes)

        buffers
            .par_chunks_mut(4)
            .enumerate()
            .for_each(|(group_idx, buf_group)| {
                let i = group_idx * 4;
                match buf_group {
                    [buf_a, buf_b, buf_c, buf_d] => {
                        // 4× unrolled: four recovery blocks share one input load.
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        let base_c = (i + 2) * n_queued;
                        let base_d = (i + 3) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .zip(buf_c.par_chunks_mut(chunk_size))
                            .zip(buf_d.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(
                                |(chunk_idx, (((chunk_a, chunk_b), chunk_c), chunk_d))| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk_a.len() * 2;
                                    let blocks_32 = byte_len / 32;
                                    let remainder = byte_len % 32;

                                    for q_idx in 0..n_queued {
                                        let (
                                            v_tl_l_a,
                                            v_tl_h_a,
                                            v_th_l_a,
                                            v_th_h_a,
                                            v_hl_l_a,
                                            v_hl_h_a,
                                            v_hh_l_a,
                                            v_hh_h_a,
                                            ref tlow_a,
                                            ref thigh_a,
                                        ) = all_tables[base_a + q_idx];
                                        let (
                                            v_tl_l_b,
                                            v_tl_h_b,
                                            v_th_l_b,
                                            v_th_h_b,
                                            v_hl_l_b,
                                            v_hl_h_b,
                                            v_hh_l_b,
                                            v_hh_h_b,
                                            ref tlow_b,
                                            ref thigh_b,
                                        ) = all_tables[base_b + q_idx];
                                        let (
                                            v_tl_l_c,
                                            v_tl_h_c,
                                            v_th_l_c,
                                            v_th_h_c,
                                            v_hl_l_c,
                                            v_hl_h_c,
                                            v_hh_l_c,
                                            v_hh_h_c,
                                            ref tlow_c,
                                            ref thigh_c,
                                        ) = all_tables[base_c + q_idx];
                                        let (
                                            v_tl_l_d,
                                            v_tl_h_d,
                                            v_th_l_d,
                                            v_th_h_d,
                                            v_hl_l_d,
                                            v_hl_h_d,
                                            v_hh_l_d,
                                            v_hh_h_d,
                                            ref tlow_d,
                                            ref thigh_d,
                                        ) = all_tables[base_d + q_idx];

                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_c = chunk_c.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_d = chunk_d.as_mut_ptr() as *mut __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let n0_2 = _mm256_and_si256(input, mask_f);
                                            let n1_3 = _mm256_and_si256(
                                                _mm256_srli_epi16(input, 4),
                                                mask_f,
                                            );

                                            // Block A
                                            let rle_a = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l_a, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h_a, n1_3),
                                            );
                                            let rhe_a = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l_a, n0_2),
                                                _mm256_shuffle_epi8(v_th_h_a, n1_3),
                                            );
                                            let rlo_a = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l_a, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h_a, n1_3),
                                            );
                                            let rho_a = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l_a, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h_a, n1_3),
                                            );
                                            let out_a = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        rle_a,
                                                        _mm256_srli_epi16(rlo_a, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        rhe_a,
                                                        _mm256_srli_epi16(rho_a, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_a,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_a), out_a),
                                            );

                                            // Block B
                                            let rle_b = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l_b, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h_b, n1_3),
                                            );
                                            let rhe_b = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l_b, n0_2),
                                                _mm256_shuffle_epi8(v_th_h_b, n1_3),
                                            );
                                            let rlo_b = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l_b, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h_b, n1_3),
                                            );
                                            let rho_b = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l_b, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h_b, n1_3),
                                            );
                                            let out_b = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        rle_b,
                                                        _mm256_srli_epi16(rlo_b, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        rhe_b,
                                                        _mm256_srli_epi16(rho_b, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_b,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_b), out_b),
                                            );

                                            // Block C
                                            let rle_c = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l_c, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h_c, n1_3),
                                            );
                                            let rhe_c = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l_c, n0_2),
                                                _mm256_shuffle_epi8(v_th_h_c, n1_3),
                                            );
                                            let rlo_c = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l_c, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h_c, n1_3),
                                            );
                                            let rho_c = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l_c, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h_c, n1_3),
                                            );
                                            let out_c = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        rle_c,
                                                        _mm256_srli_epi16(rlo_c, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        rhe_c,
                                                        _mm256_srli_epi16(rho_c, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_c,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_c), out_c),
                                            );

                                            // Block D
                                            let rle_d = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l_d, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h_d, n1_3),
                                            );
                                            let rhe_d = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l_d, n0_2),
                                                _mm256_shuffle_epi8(v_th_h_d, n1_3),
                                            );
                                            let rlo_d = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l_d, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h_d, n1_3),
                                            );
                                            let rho_d = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l_d, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h_d, n1_3),
                                            );
                                            let out_d = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        rle_d,
                                                        _mm256_srli_epi16(rlo_d, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        rhe_d,
                                                        _mm256_srli_epi16(rho_d, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_d,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_d), out_d),
                                            );

                                            ptr_in = ptr_in.add(1);
                                            ptr_a = ptr_a.add(1);
                                            ptr_b = ptr_b.add(1);
                                            ptr_c = ptr_c.add(1);
                                            ptr_d = ptr_d.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_32 * 16;
                                            let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                            let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                            let mut pw_c = chunk_c[ow..].as_mut_ptr();
                                            let mut pw_d = chunk_d[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
                                            let tail_end = p_in.add(remainder);
                                            while p_in < tail_end {
                                                let lo = *p_in as usize;
                                                let hi = *p_in.add(1) as usize;
                                                *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                                *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                                *pw_c ^= tlow_c[lo] ^ thigh_c[hi];
                                                *pw_d ^= tlow_d[lo] ^ thigh_d[hi];
                                                pw_a = pw_a.add(1);
                                                pw_b = pw_b.add(1);
                                                pw_c = pw_c.add(1);
                                                pw_d = pw_d.add(1);
                                                p_in = p_in.add(2);
                                            }
                                        }
                                    }
                                },
                            );
                    }
                    [buf_a, buf_b] => {
                        // Fallback for 2 blocks (remains 2× unrolled).
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(|(chunk_idx, (chunk_a, chunk_b))| unsafe {
                                let byte_offset = chunk_idx * chunk_size * 2;
                                let byte_len = chunk_a.len() * 2;
                                let blocks_32 = byte_len / 32;
                                let remainder = byte_len % 32;

                                for q_idx in 0..n_queued {
                                    let (
                                        v_tl_l_a,
                                        v_tl_h_a,
                                        v_th_l_a,
                                        v_th_h_a,
                                        v_hl_l_a,
                                        v_hl_h_a,
                                        v_hh_l_a,
                                        v_hh_h_a,
                                        ref tlow_a,
                                        ref thigh_a,
                                    ) = all_tables[base_a + q_idx];
                                    let (
                                        v_tl_l_b,
                                        v_tl_h_b,
                                        v_th_l_b,
                                        v_th_h_b,
                                        v_hl_l_b,
                                        v_hl_h_b,
                                        v_hh_l_b,
                                        v_hh_h_b,
                                        ref tlow_b,
                                        ref thigh_b,
                                    ) = all_tables[base_b + q_idx];
                                    let slice_chunk =
                                        &queued[q_idx][byte_offset..byte_offset + byte_len];

                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                    let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                    let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                    let end = ptr_in.add(blocks_32);

                                    while ptr_in < end {
                                        _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                        let input = _mm256_loadu_si256(ptr_in);
                                        let n0_2 = _mm256_and_si256(input, mask_f);
                                        let n1_3 =
                                            _mm256_and_si256(_mm256_srli_epi16(input, 4), mask_f);

                                        let rle_a = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_tl_l_a, n0_2),
                                            _mm256_shuffle_epi8(v_tl_h_a, n1_3),
                                        );
                                        let rhe_a = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_th_l_a, n0_2),
                                            _mm256_shuffle_epi8(v_th_h_a, n1_3),
                                        );
                                        let rlo_a = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_hl_l_a, n0_2),
                                            _mm256_shuffle_epi8(v_hl_h_a, n1_3),
                                        );
                                        let rho_a = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_hh_l_a, n0_2),
                                            _mm256_shuffle_epi8(v_hh_h_a, n1_3),
                                        );
                                        let out_a = _mm256_or_si256(
                                            _mm256_and_si256(
                                                _mm256_xor_si256(
                                                    rle_a,
                                                    _mm256_srli_epi16(rlo_a, 8),
                                                ),
                                                mask_even,
                                            ),
                                            _mm256_slli_epi16(
                                                _mm256_xor_si256(
                                                    rhe_a,
                                                    _mm256_srli_epi16(rho_a, 8),
                                                ),
                                                8,
                                            ),
                                        );
                                        _mm256_storeu_si256(
                                            ptr_a,
                                            _mm256_xor_si256(_mm256_loadu_si256(ptr_a), out_a),
                                        );

                                        let rle_b = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_tl_l_b, n0_2),
                                            _mm256_shuffle_epi8(v_tl_h_b, n1_3),
                                        );
                                        let rhe_b = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_th_l_b, n0_2),
                                            _mm256_shuffle_epi8(v_th_h_b, n1_3),
                                        );
                                        let rlo_b = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_hl_l_b, n0_2),
                                            _mm256_shuffle_epi8(v_hl_h_b, n1_3),
                                        );
                                        let rho_b = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_hh_l_b, n0_2),
                                            _mm256_shuffle_epi8(v_hh_h_b, n1_3),
                                        );
                                        let out_b = _mm256_or_si256(
                                            _mm256_and_si256(
                                                _mm256_xor_si256(
                                                    rle_b,
                                                    _mm256_srli_epi16(rlo_b, 8),
                                                ),
                                                mask_even,
                                            ),
                                            _mm256_slli_epi16(
                                                _mm256_xor_si256(
                                                    rhe_b,
                                                    _mm256_srli_epi16(rho_b, 8),
                                                ),
                                                8,
                                            ),
                                        );
                                        _mm256_storeu_si256(
                                            ptr_b,
                                            _mm256_xor_si256(_mm256_loadu_si256(ptr_b), out_b),
                                        );

                                        ptr_in = ptr_in.add(1);
                                        ptr_a = ptr_a.add(1);
                                        ptr_b = ptr_b.add(1);
                                    }

                                    if remainder > 0 {
                                        let ow = blocks_32 * 16;
                                        let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                        let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                        let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
                                        let tail_end = p_in.add(remainder);
                                        while p_in < tail_end {
                                            let lo = *p_in as usize;
                                            let hi = *p_in.add(1) as usize;
                                            *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                            *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                            pw_a = pw_a.add(1);
                                            pw_b = pw_b.add(1);
                                            p_in = p_in.add(2);
                                        }
                                    }
                                }
                            });
                    }
                    rest => {
                        // Fallback for remaining 1 or 3 blocks (scalar for SIMD simplicity here).
                        for (j, buf) in rest.iter_mut().enumerate() {
                            let base = (i + j) * n_queued;
                            buf.par_chunks_mut(chunk_size).enumerate().for_each(
                                |(chunk_idx, chunk)| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk.len() * 2;
                                    let blocks_32 = byte_len / 32;
                                    let remainder = byte_len % 32;

                                    for q_idx in 0..n_queued {
                                        let (
                                            v_tl_l,
                                            v_tl_h,
                                            v_th_l,
                                            v_th_h,
                                            v_hl_l,
                                            v_hl_h,
                                            v_hh_l,
                                            v_hh_h,
                                            ref table_low,
                                            ref table_high,
                                        ) = all_tables[base + q_idx];
                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_buf = chunk.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let n0_2 = _mm256_and_si256(input, mask_f);
                                            let n1_3 = _mm256_and_si256(
                                                _mm256_srli_epi16(input, 4),
                                                mask_f,
                                            );
                                            let res_lo_even = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h, n1_3),
                                            );
                                            let res_hi_even = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l, n0_2),
                                                _mm256_shuffle_epi8(v_th_h, n1_3),
                                            );
                                            let res_lo_odd = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h, n1_3),
                                            );
                                            let res_hi_odd = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h, n1_3),
                                            );
                                            let out = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        res_lo_even,
                                                        _mm256_srli_epi16(res_lo_odd, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        res_hi_even,
                                                        _mm256_srli_epi16(res_hi_odd, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_buf,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_buf), out),
                                            );
                                            ptr_in = ptr_in.add(1);
                                            ptr_buf = ptr_buf.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_32 * 16;
                                            let mut pw = chunk[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
                                            let tail_end = p_in.add(remainder);
                                            while p_in < tail_end {
                                                let lo = *p_in as usize;
                                                let hi = *p_in.add(1) as usize;
                                                *pw ^= table_low[lo] ^ table_high[hi];
                                                pw = pw.add(1);
                                                p_in = p_in.add(2);
                                            }
                                        }
                                    }
                                },
                            );
                        }
                    }
                }
            });
    }

    /// AVX2 Shuffle2x flush: accumulates queued slices into Shuffle2x recovery buffers.
    ///
    /// Input slices are in normal u16 layout. Recovery buffers are in Shuffle2x layout
    /// (lo-bytes in lane 0, hi-bytes in lane 1 of each 32-byte chunk). Uses 4 PSHUFB
    /// per recovery block per 32-byte input chunk instead of the 8 used by the plain
    /// AVX2 nibble-shuffle path, achieving ~33% fewer instructions per block.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn flush_avx2_shuffle2x(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        let RecoveryBufferSet::Shuffle2x(ref mut bufs) = self.buffers else {
            unreachable!("flush_avx2_shuffle2x called on non-Shuffle2x encoder");
        };

        unsafe {
            Self::flush_avx2_shuffle2x_work(
                bufs,
                &queued,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
        }

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn flush_avx2_shuffle2x_work(
        buffers: &mut [Vec<u8>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        // Byte-separation mask: within each 128-bit lane, move even-indexed bytes
        // (lo bytes of u16 words) to positions 0-7 and odd-indexed bytes (hi bytes)
        // to positions 8-15.  Combined with vpermq 0xD8 this is `separate_low_high`.
        let sep_mask = _mm256_set_epi8(
            15, 13, 11, 9, 7, 5, 3, 1, 14, 12, 10, 8, 6, 4, 2, 0, // lane 1
            15, 13, 11, 9, 7, 5, 3, 1, 14, 12, 10, 8, 6, 4, 2, 0, // lane 0
        );
        let mask_f = _mm256_set1_epi8(0x0F_u8 as i8);

        let n_rec = buffers.len();
        let n_queued = queued.len();

        // Pre-build 4-register Shuffle2x coefficient tables in parallel.
        // For coefficient c, 4-bit nibble index n (0..15):
        //   loNk[n] = (gf.mul(n << 4k, c) & 0xFF) as u8
        //   hiNk[n] = (gf.mul(n << 4k, c) >> 8) as u8
        // Table layout (each __m256i packs two 128-bit sub-tables into its two lanes):
        //   tNormA: lane0 = loN0, lane1 = hiN2
        //   tNormB: lane0 = loN1, lane1 = hiN3
        //   tSwapA: lane0 = loN2, lane1 = hiN0
        //   tSwapB: lane0 = loN3, lane1 = hiN1
        let all_tables: Vec<Avx2Shuffle2xTable> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| unsafe {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut lo_n0 = [0u8; 16];
                let mut lo_n1 = [0u8; 16];
                let mut lo_n2 = [0u8; 16];
                let mut lo_n3 = [0u8; 16];
                let mut hi_n0 = [0u8; 16];
                let mut hi_n1 = [0u8; 16];
                let mut hi_n2 = [0u8; 16];
                let mut hi_n3 = [0u8; 16];

                for n in 0..16usize {
                    let r0 = gf.mul(n as u16, coeff);
                    lo_n0[n] = (r0 & 0xFF) as u8;
                    hi_n0[n] = (r0 >> 8) as u8;
                    let r1 = gf.mul((n as u16) << 4, coeff);
                    lo_n1[n] = (r1 & 0xFF) as u8;
                    hi_n1[n] = (r1 >> 8) as u8;
                    let r2 = gf.mul((n as u16) << 8, coeff);
                    lo_n2[n] = (r2 & 0xFF) as u8;
                    hi_n2[n] = (r2 >> 8) as u8;
                    let r3 = gf.mul((n as u16) << 12, coeff);
                    lo_n3[n] = (r3 & 0xFF) as u8;
                    hi_n3[n] = (r3 >> 8) as u8;
                }

                let t_norm_a = _mm256_set_m128i(
                    _mm_loadu_si128(hi_n2.as_ptr() as *const __m128i),
                    _mm_loadu_si128(lo_n0.as_ptr() as *const __m128i),
                );
                let t_norm_b = _mm256_set_m128i(
                    _mm_loadu_si128(hi_n3.as_ptr() as *const __m128i),
                    _mm_loadu_si128(lo_n1.as_ptr() as *const __m128i),
                );
                let t_swap_a = _mm256_set_m128i(
                    _mm_loadu_si128(hi_n0.as_ptr() as *const __m128i),
                    _mm_loadu_si128(lo_n2.as_ptr() as *const __m128i),
                );
                let t_swap_b = _mm256_set_m128i(
                    _mm_loadu_si128(hi_n1.as_ptr() as *const __m128i),
                    _mm_loadu_si128(lo_n3.as_ptr() as *const __m128i),
                );

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                (
                    t_norm_a, t_norm_b, t_swap_a, t_swap_b, table_low, table_high,
                )
            })
            .collect();

        // 32 KiB byte chunks per recovery buffer = 16384 words, matching the
        // flush_avx2_work chunk granularity.
        let chunk_size_bytes = 32768usize;

        buffers
            .par_chunks_mut(4)
            .enumerate()
            .for_each(|(group_idx, buf_group)| {
                let i = group_idx * 4;
                match buf_group {
                    [buf_a, buf_b, buf_c, buf_d] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        let base_c = (i + 2) * n_queued;
                        let base_d = (i + 3) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size_bytes)
                            .zip(buf_b.par_chunks_mut(chunk_size_bytes))
                            .zip(buf_c.par_chunks_mut(chunk_size_bytes))
                            .zip(buf_d.par_chunks_mut(chunk_size_bytes))
                            .enumerate()
                            .for_each(
                                |(chunk_idx, (((chunk_a, chunk_b), chunk_c), chunk_d))| unsafe {
                                    let byte_offset = chunk_idx * chunk_size_bytes;
                                    let byte_len = chunk_a.len();
                                    let blocks_32 = byte_len / 32;

                                    for q_idx in 0..n_queued {
                                        let (tna_a, tnb_a, tsa_a, tsb_a, _, _) =
                                            all_tables[base_a + q_idx];
                                        let (tna_b, tnb_b, tsa_b, tsb_b, _, _) =
                                            all_tables[base_b + q_idx];
                                        let (tna_c, tnb_c, tsa_c, tsb_c, _, _) =
                                            all_tables[base_c + q_idx];
                                        let (tna_d, tnb_d, tsa_d, tsb_d, _, _) =
                                            all_tables[base_d + q_idx];

                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_in =
                                            slice_chunk.as_ptr() as *const __m256i;
                                        let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_c = chunk_c.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_d = chunk_d.as_mut_ptr() as *mut __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(
                                                ptr_in.add(4) as *const i8,
                                                _MM_HINT_T0,
                                            );
                                            let input = _mm256_loadu_si256(ptr_in);
                                            // separate_low_high: lane0 = lo bytes, lane1 = hi
                                            let s = _mm256_permute4x64_epi64(
                                                _mm256_shuffle_epi8(input, sep_mask),
                                                0xD8,
                                            );
                                            // swap lanes for cross-lane contributions
                                            let sw =
                                                _mm256_permute2x128_si256(s, s, 0x01);

                                            let lo_nib_s = _mm256_and_si256(s, mask_f);
                                            let hi_nib_s = _mm256_and_si256(
                                                _mm256_srli_epi16(s, 4),
                                                mask_f,
                                            );
                                            let lo_nib_sw = _mm256_and_si256(sw, mask_f);
                                            let hi_nib_sw = _mm256_and_si256(
                                                _mm256_srli_epi16(sw, 4),
                                                mask_f,
                                            );

                                            // s2x_block: 4 PSHUFB + 2 XOR = one GF(2^16) madd
                                            // into a Shuffle2x recovery buffer.
                                            // norm  = pshufb(tNormA, lo_nib_s)  ^ pshufb(tNormB, hi_nib_s)
                                            // swap  = pshufb(tSwapA, lo_nib_sw) ^ pshufb(tSwapB, hi_nib_sw)
                                            // result.lane0 = full lo-byte, result.lane1 = full hi-byte ✓
                                            macro_rules! s2x_block {
                                                ($ta:expr, $tb:expr, $tc:expr, $td:expr, $ptr:expr) => {{
                                                    let norm = _mm256_xor_si256(
                                                        _mm256_shuffle_epi8($ta, lo_nib_s),
                                                        _mm256_shuffle_epi8($tb, hi_nib_s),
                                                    );
                                                    let swap = _mm256_xor_si256(
                                                        _mm256_shuffle_epi8($tc, lo_nib_sw),
                                                        _mm256_shuffle_epi8($td, hi_nib_sw),
                                                    );
                                                    _mm256_storeu_si256(
                                                        $ptr,
                                                        _mm256_xor_si256(
                                                            _mm256_loadu_si256($ptr),
                                                            _mm256_xor_si256(norm, swap),
                                                        ),
                                                    );
                                                }};
                                            }

                                            s2x_block!(tna_a, tnb_a, tsa_a, tsb_a, ptr_a);
                                            s2x_block!(tna_b, tnb_b, tsa_b, tsb_b, ptr_b);
                                            s2x_block!(tna_c, tnb_c, tsa_c, tsb_c, ptr_c);
                                            s2x_block!(tna_d, tnb_d, tsa_d, tsb_d, ptr_d);

                                            ptr_in = ptr_in.add(1);
                                            ptr_a = ptr_a.add(1);
                                            ptr_b = ptr_b.add(1);
                                            ptr_c = ptr_c.add(1);
                                            ptr_d = ptr_d.add(1);
                                        }
                                    }
                                },
                            );
                    }
                    [buf_a, buf_b] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size_bytes)
                            .zip(buf_b.par_chunks_mut(chunk_size_bytes))
                            .enumerate()
                            .for_each(|(chunk_idx, (chunk_a, chunk_b))| unsafe {
                                let byte_offset = chunk_idx * chunk_size_bytes;
                                let byte_len = chunk_a.len();
                                let blocks_32 = byte_len / 32;

                                for q_idx in 0..n_queued {
                                    let (tna_a, tnb_a, tsa_a, tsb_a, _, _) =
                                        all_tables[base_a + q_idx];
                                    let (tna_b, tnb_b, tsa_b, tsb_b, _, _) =
                                        all_tables[base_b + q_idx];

                                    let slice_chunk =
                                        &queued[q_idx][byte_offset..byte_offset + byte_len];

                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                    let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                    let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                    let end = ptr_in.add(blocks_32);

                                    while ptr_in < end {
                                        _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                        let input = _mm256_loadu_si256(ptr_in);
                                        let s = _mm256_permute4x64_epi64(
                                            _mm256_shuffle_epi8(input, sep_mask),
                                            0xD8,
                                        );
                                        let sw = _mm256_permute2x128_si256(s, s, 0x01);

                                        let lo_nib_s = _mm256_and_si256(s, mask_f);
                                        let hi_nib_s = _mm256_and_si256(
                                            _mm256_srli_epi16(s, 4),
                                            mask_f,
                                        );
                                        let lo_nib_sw = _mm256_and_si256(sw, mask_f);
                                        let hi_nib_sw = _mm256_and_si256(
                                            _mm256_srli_epi16(sw, 4),
                                            mask_f,
                                        );

                                        macro_rules! s2x_block {
                                            ($ta:expr, $tb:expr, $tc:expr, $td:expr, $ptr:expr) => {{
                                                let norm = _mm256_xor_si256(
                                                    _mm256_shuffle_epi8($ta, lo_nib_s),
                                                    _mm256_shuffle_epi8($tb, hi_nib_s),
                                                );
                                                let swap = _mm256_xor_si256(
                                                    _mm256_shuffle_epi8($tc, lo_nib_sw),
                                                    _mm256_shuffle_epi8($td, hi_nib_sw),
                                                );
                                                _mm256_storeu_si256(
                                                    $ptr,
                                                    _mm256_xor_si256(
                                                        _mm256_loadu_si256($ptr),
                                                        _mm256_xor_si256(norm, swap),
                                                    ),
                                                );
                                            }};
                                        }

                                        s2x_block!(tna_a, tnb_a, tsa_a, tsb_a, ptr_a);
                                        s2x_block!(tna_b, tnb_b, tsa_b, tsb_b, ptr_b);

                                        ptr_in = ptr_in.add(1);
                                        ptr_a = ptr_a.add(1);
                                        ptr_b = ptr_b.add(1);
                                    }
                                }
                            });
                    }
                    rest => {
                        for (j, buf) in rest.iter_mut().enumerate() {
                            let base = (i + j) * n_queued;
                            buf.par_chunks_mut(chunk_size_bytes).enumerate().for_each(
                                |(chunk_idx, chunk)| unsafe {
                                    let byte_offset = chunk_idx * chunk_size_bytes;
                                    let byte_len = chunk.len();
                                    let blocks_32 = byte_len / 32;

                                    for q_idx in 0..n_queued {
                                        let (tna, tnb, tsa, tsb, _, _) =
                                            all_tables[base + q_idx];
                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_buf = chunk.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let s = _mm256_permute4x64_epi64(
                                                _mm256_shuffle_epi8(input, sep_mask),
                                                0xD8,
                                            );
                                            let sw = _mm256_permute2x128_si256(s, s, 0x01);
                                            let lo_nib_s = _mm256_and_si256(s, mask_f);
                                            let hi_nib_s = _mm256_and_si256(
                                                _mm256_srli_epi16(s, 4),
                                                mask_f,
                                            );
                                            let lo_nib_sw = _mm256_and_si256(sw, mask_f);
                                            let hi_nib_sw = _mm256_and_si256(
                                                _mm256_srli_epi16(sw, 4),
                                                mask_f,
                                            );
                                            let norm = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(tna, lo_nib_s),
                                                _mm256_shuffle_epi8(tnb, hi_nib_s),
                                            );
                                            let swap = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(tsa, lo_nib_sw),
                                                _mm256_shuffle_epi8(tsb, hi_nib_sw),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_buf,
                                                _mm256_xor_si256(
                                                    _mm256_loadu_si256(ptr_buf),
                                                    _mm256_xor_si256(norm, swap),
                                                ),
                                            );
                                            ptr_in = ptr_in.add(1);
                                            ptr_buf = ptr_buf.add(1);
                                        }
                                    }
                                },
                            );
                        }
                    }
                }
            });
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,gfni")]
    unsafe fn flush_avx2_gfni(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        unsafe {
            Self::flush_avx2_gfni_work(
                self.buffers.as_normal_mut(),
                &queued,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
        }

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,gfni")]
    #[allow(dead_code)]
    unsafe fn flush_avx2_gfni_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        use std::arch::x86_64::*;

        let deint_mask = _mm256_broadcastsi128_si256(_mm_setr_epi8(
            0, 2, 4, 6, 8, 10, 12, 14, // lo bytes of 8 words → positions 0..7
            1, 3, 5, 7, 9, 11, 13, 15, // hi bytes of 8 words → positions 8..15
        ));

        let n_rec = buffers.len();
        let n_queued = queued.len();

        let all_tables: Vec<Avx2GfniTable> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                // gf2p8affineqb uses byte (7-row) of the u64 matrix operand for
                // output bit `row` — so store each row at position (7-row)*8.
                let mut m_ll = 0u64; // lo input byte → lo output byte
                let mut m_lh = 0u64; // hi input byte → lo output byte
                let mut m_hl = 0u64; // lo input byte → hi output byte
                let mut m_hh = 0u64; // hi input byte → hi output byte
                for row in 0..8usize {
                    let mut row_ll = 0u8;
                    let mut row_lh = 0u8;
                    let mut row_hl = 0u8;
                    let mut row_hh = 0u8;
                    for j in 0..8usize {
                        let r_lo = gf.mul(1u16 << j, coeff);
                        if (r_lo >> row) & 1 == 1 {
                            row_ll |= 1 << j;
                        }
                        if (r_lo >> (row + 8)) & 1 == 1 {
                            row_hl |= 1 << j;
                        }
                        let r_hi = gf.mul(1u16 << (j + 8), coeff);
                        if (r_hi >> row) & 1 == 1 {
                            row_lh |= 1 << j;
                        }
                        if (r_hi >> (row + 8)) & 1 == 1 {
                            row_hh |= 1 << j;
                        }
                    }
                    let shift = (7 - row) * 8;
                    m_ll |= (row_ll as u64) << shift;
                    m_lh |= (row_lh as u64) << shift;
                    m_hl |= (row_hl as u64) << shift;
                    m_hh |= (row_hh as u64) << shift;
                }

                let mat_lo = _mm256_set_epi64x(m_lh as i64, m_ll as i64, m_lh as i64, m_ll as i64);
                let mat_hi = _mm256_set_epi64x(m_hh as i64, m_hl as i64, m_hh as i64, m_hl as i64);

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                (mat_lo, mat_hi, table_low, table_high)
            })
            .collect();

        // 32 KiB recovery buffer chunk: chunk × group fits L1/L2 and amortizes
        // the rayon task overhead. 8 × 32 KiB = 256 KiB stays in L2 on most
        // modern CPUs; if L2 is smaller (older Skylake clients) the hardware
        // prefetcher compensates adequately.
        let chunk_size = 16384usize;

        buffers
            .par_chunks_mut(8)
            .enumerate()
            .for_each(|(group_idx, buf_group)| {
                let i = group_idx * 8;
                match buf_group {
                    // 8-way unroll: one input load + one deinterleave feeds 8 recovery blocks,
                    // halving the loadu/shuffle overhead compared with the 4-way arm.
                    [buf_a, buf_b, buf_c, buf_d, buf_e, buf_f, buf_g, buf_h] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        let base_c = (i + 2) * n_queued;
                        let base_d = (i + 3) * n_queued;
                        let base_e = (i + 4) * n_queued;
                        let base_f = (i + 5) * n_queued;
                        let base_g = (i + 6) * n_queued;
                        let base_h = (i + 7) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .zip(buf_c.par_chunks_mut(chunk_size))
                            .zip(buf_d.par_chunks_mut(chunk_size))
                            .zip(buf_e.par_chunks_mut(chunk_size))
                            .zip(buf_f.par_chunks_mut(chunk_size))
                            .zip(buf_g.par_chunks_mut(chunk_size))
                            .zip(buf_h.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(
                                |(
                                    chunk_idx,
                                    (
                                        (
                                            (
                                                ((((chunk_a, chunk_b), chunk_c), chunk_d), chunk_e),
                                                chunk_f,
                                            ),
                                            chunk_g,
                                        ),
                                        chunk_h,
                                    ),
                                )| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk_a.len() * 2;
                                    let blocks_32 = byte_len / 32;
                                    let remainder = byte_len % 32;

                                    for q_idx in 0..n_queued {
                                        let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                            all_tables[base_a + q_idx];
                                        let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                            all_tables[base_b + q_idx];
                                        let (mat_lo_c, mat_hi_c, ref tlow_c, ref thigh_c) =
                                            all_tables[base_c + q_idx];
                                        let (mat_lo_d, mat_hi_d, ref tlow_d, ref thigh_d) =
                                            all_tables[base_d + q_idx];
                                        let (mat_lo_e, mat_hi_e, ref tlow_e, ref thigh_e) =
                                            all_tables[base_e + q_idx];
                                        let (mat_lo_f, mat_hi_f, ref tlow_f, ref thigh_f) =
                                            all_tables[base_f + q_idx];
                                        let (mat_lo_g, mat_hi_g, ref tlow_g, ref thigh_g) =
                                            all_tables[base_g + q_idx];
                                        let (mat_lo_h, mat_hi_h, ref tlow_h, ref thigh_h) =
                                            all_tables[base_h + q_idx];

                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_c = chunk_c.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_d = chunk_d.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_e = chunk_e.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_f = chunk_f.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_g = chunk_g.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_h = chunk_h.as_mut_ptr() as *mut __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let separated = _mm256_shuffle_epi8(input, deint_mask);

                                            macro_rules! gfni_block {
                                                ($mat_lo:expr, $mat_hi:expr, $ptr:expr) => {{
                                                    let vlo = _mm256_gf2p8affine_epi64_epi8(
                                                        separated, $mat_lo, 0,
                                                    );
                                                    let vhi = _mm256_gf2p8affine_epi64_epi8(
                                                        separated, $mat_hi, 0,
                                                    );
                                                    let out = _mm256_unpacklo_epi8(
                                                        _mm256_xor_si256(
                                                            vlo,
                                                            _mm256_bsrli_epi128::<8>(vlo),
                                                        ),
                                                        _mm256_xor_si256(
                                                            vhi,
                                                            _mm256_bsrli_epi128::<8>(vhi),
                                                        ),
                                                    );
                                                    _mm256_storeu_si256(
                                                        $ptr,
                                                        _mm256_xor_si256(
                                                            _mm256_loadu_si256($ptr),
                                                            out,
                                                        ),
                                                    );
                                                }};
                                            }

                                            gfni_block!(mat_lo_a, mat_hi_a, ptr_a);
                                            gfni_block!(mat_lo_b, mat_hi_b, ptr_b);
                                            gfni_block!(mat_lo_c, mat_hi_c, ptr_c);
                                            gfni_block!(mat_lo_d, mat_hi_d, ptr_d);
                                            gfni_block!(mat_lo_e, mat_hi_e, ptr_e);
                                            gfni_block!(mat_lo_f, mat_hi_f, ptr_f);
                                            gfni_block!(mat_lo_g, mat_hi_g, ptr_g);
                                            gfni_block!(mat_lo_h, mat_hi_h, ptr_h);

                                            ptr_in = ptr_in.add(1);
                                            ptr_a = ptr_a.add(1);
                                            ptr_b = ptr_b.add(1);
                                            ptr_c = ptr_c.add(1);
                                            ptr_d = ptr_d.add(1);
                                            ptr_e = ptr_e.add(1);
                                            ptr_f = ptr_f.add(1);
                                            ptr_g = ptr_g.add(1);
                                            ptr_h = ptr_h.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_32 * 16;
                                            let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                            let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                            let mut pw_c = chunk_c[ow..].as_mut_ptr();
                                            let mut pw_d = chunk_d[ow..].as_mut_ptr();
                                            let mut pw_e = chunk_e[ow..].as_mut_ptr();
                                            let mut pw_f = chunk_f[ow..].as_mut_ptr();
                                            let mut pw_g = chunk_g[ow..].as_mut_ptr();
                                            let mut pw_h = chunk_h[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
                                            let tail_end = p_in.add(remainder);
                                            while p_in < tail_end {
                                                let lo = *p_in as usize;
                                                let hi = *p_in.add(1) as usize;
                                                *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                                *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                                *pw_c ^= tlow_c[lo] ^ thigh_c[hi];
                                                *pw_d ^= tlow_d[lo] ^ thigh_d[hi];
                                                *pw_e ^= tlow_e[lo] ^ thigh_e[hi];
                                                *pw_f ^= tlow_f[lo] ^ thigh_f[hi];
                                                *pw_g ^= tlow_g[lo] ^ thigh_g[hi];
                                                *pw_h ^= tlow_h[lo] ^ thigh_h[hi];
                                                pw_a = pw_a.add(1);
                                                pw_b = pw_b.add(1);
                                                pw_c = pw_c.add(1);
                                                pw_d = pw_d.add(1);
                                                pw_e = pw_e.add(1);
                                                pw_f = pw_f.add(1);
                                                pw_g = pw_g.add(1);
                                                pw_h = pw_h.add(1);
                                                p_in = p_in.add(2);
                                            }
                                        }
                                    }
                                },
                            );
                    }
                    [buf_a, buf_b, buf_c, buf_d] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        let base_c = (i + 2) * n_queued;
                        let base_d = (i + 3) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .zip(buf_c.par_chunks_mut(chunk_size))
                            .zip(buf_d.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(
                                |(chunk_idx, (((chunk_a, chunk_b), chunk_c), chunk_d))| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk_a.len() * 2;
                                    let blocks_32 = byte_len / 32;
                                    let remainder = byte_len % 32;

                                    for q_idx in 0..n_queued {
                                        let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                            all_tables[base_a + q_idx];
                                        let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                            all_tables[base_b + q_idx];
                                        let (mat_lo_c, mat_hi_c, ref tlow_c, ref thigh_c) =
                                            all_tables[base_c + q_idx];
                                        let (mat_lo_d, mat_hi_d, ref tlow_d, ref thigh_d) =
                                            all_tables[base_d + q_idx];

                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_c = chunk_c.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_d = chunk_d.as_mut_ptr() as *mut __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let separated = _mm256_shuffle_epi8(input, deint_mask);

                                            // Block A
                                            let vlo_a = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_lo_a, 0,
                                            );
                                            let new_lo_a = _mm256_xor_si256(
                                                vlo_a,
                                                _mm256_bsrli_epi128::<8>(vlo_a),
                                            );
                                            let vhi_a = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_hi_a, 0,
                                            );
                                            let new_hi_a = _mm256_xor_si256(
                                                vhi_a,
                                                _mm256_bsrli_epi128::<8>(vhi_a),
                                            );
                                            let out_a = _mm256_unpacklo_epi8(new_lo_a, new_hi_a);
                                            _mm256_storeu_si256(
                                                ptr_a,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_a), out_a),
                                            );

                                            // Block B
                                            let vlo_b = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_lo_b, 0,
                                            );
                                            let new_lo_b = _mm256_xor_si256(
                                                vlo_b,
                                                _mm256_bsrli_epi128::<8>(vlo_b),
                                            );
                                            let vhi_b = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_hi_b, 0,
                                            );
                                            let new_hi_b = _mm256_xor_si256(
                                                vhi_b,
                                                _mm256_bsrli_epi128::<8>(vhi_b),
                                            );
                                            let out_b = _mm256_unpacklo_epi8(new_lo_b, new_hi_b);
                                            _mm256_storeu_si256(
                                                ptr_b,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_b), out_b),
                                            );

                                            // Block C
                                            let vlo_c = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_lo_c, 0,
                                            );
                                            let new_lo_c = _mm256_xor_si256(
                                                vlo_c,
                                                _mm256_bsrli_epi128::<8>(vlo_c),
                                            );
                                            let vhi_c = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_hi_c, 0,
                                            );
                                            let new_hi_c = _mm256_xor_si256(
                                                vhi_c,
                                                _mm256_bsrli_epi128::<8>(vhi_c),
                                            );
                                            let out_c = _mm256_unpacklo_epi8(new_lo_c, new_hi_c);
                                            _mm256_storeu_si256(
                                                ptr_c,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_c), out_c),
                                            );

                                            // Block D
                                            let vlo_d = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_lo_d, 0,
                                            );
                                            let new_lo_d = _mm256_xor_si256(
                                                vlo_d,
                                                _mm256_bsrli_epi128::<8>(vlo_d),
                                            );
                                            let vhi_d = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_hi_d, 0,
                                            );
                                            let new_hi_d = _mm256_xor_si256(
                                                vhi_d,
                                                _mm256_bsrli_epi128::<8>(vhi_d),
                                            );
                                            let out_d = _mm256_unpacklo_epi8(new_lo_d, new_hi_d);
                                            _mm256_storeu_si256(
                                                ptr_d,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_d), out_d),
                                            );

                                            ptr_in = ptr_in.add(1);
                                            ptr_a = ptr_a.add(1);
                                            ptr_b = ptr_b.add(1);
                                            ptr_c = ptr_c.add(1);
                                            ptr_d = ptr_d.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_32 * 16;
                                            let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                            let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                            let mut pw_c = chunk_c[ow..].as_mut_ptr();
                                            let mut pw_d = chunk_d[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
                                            let tail_end = p_in.add(remainder);
                                            while p_in < tail_end {
                                                let lo = *p_in as usize;
                                                let hi = *p_in.add(1) as usize;
                                                *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                                *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                                *pw_c ^= tlow_c[lo] ^ thigh_c[hi];
                                                *pw_d ^= tlow_d[lo] ^ thigh_d[hi];
                                                pw_a = pw_a.add(1);
                                                pw_b = pw_b.add(1);
                                                pw_c = pw_c.add(1);
                                                pw_d = pw_d.add(1);
                                                p_in = p_in.add(2);
                                            }
                                        }
                                    }
                                },
                            );
                    }
                    [buf_a, buf_b] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(|(chunk_idx, (chunk_a, chunk_b))| unsafe {
                                let byte_offset = chunk_idx * chunk_size * 2;
                                let byte_len = chunk_a.len() * 2;
                                let blocks_32 = byte_len / 32;
                                let remainder = byte_len % 32;

                                for q_idx in 0..n_queued {
                                    let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                        all_tables[base_a + q_idx];
                                    let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                        all_tables[base_b + q_idx];
                                    let slice_chunk =
                                        &queued[q_idx][byte_offset..byte_offset + byte_len];

                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                    let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                    let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                    let end = ptr_in.add(blocks_32);

                                    while ptr_in < end {
                                        _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                        let input = _mm256_loadu_si256(ptr_in);
                                        let separated = _mm256_shuffle_epi8(input, deint_mask);

                                        let vlo_a =
                                            _mm256_gf2p8affine_epi64_epi8(separated, mat_lo_a, 0);
                                        let new_lo_a = _mm256_xor_si256(
                                            vlo_a,
                                            _mm256_bsrli_epi128::<8>(vlo_a),
                                        );
                                        let vhi_a =
                                            _mm256_gf2p8affine_epi64_epi8(separated, mat_hi_a, 0);
                                        let new_hi_a = _mm256_xor_si256(
                                            vhi_a,
                                            _mm256_bsrli_epi128::<8>(vhi_a),
                                        );
                                        let out_a = _mm256_unpacklo_epi8(new_lo_a, new_hi_a);
                                        _mm256_storeu_si256(
                                            ptr_a,
                                            _mm256_xor_si256(_mm256_loadu_si256(ptr_a), out_a),
                                        );

                                        let vlo_b =
                                            _mm256_gf2p8affine_epi64_epi8(separated, mat_lo_b, 0);
                                        let new_lo_b = _mm256_xor_si256(
                                            vlo_b,
                                            _mm256_bsrli_epi128::<8>(vlo_b),
                                        );
                                        let vhi_b =
                                            _mm256_gf2p8affine_epi64_epi8(separated, mat_hi_b, 0);
                                        let new_hi_b = _mm256_xor_si256(
                                            vhi_b,
                                            _mm256_bsrli_epi128::<8>(vhi_b),
                                        );
                                        let out_b = _mm256_unpacklo_epi8(new_lo_b, new_hi_b);
                                        _mm256_storeu_si256(
                                            ptr_b,
                                            _mm256_xor_si256(_mm256_loadu_si256(ptr_b), out_b),
                                        );

                                        ptr_in = ptr_in.add(1);
                                        ptr_a = ptr_a.add(1);
                                        ptr_b = ptr_b.add(1);
                                    }

                                    if remainder > 0 {
                                        let ow = blocks_32 * 16;
                                        let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                        let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                        let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
                                        let tail_end = p_in.add(remainder);
                                        while p_in < tail_end {
                                            let lo = *p_in as usize;
                                            let hi = *p_in.add(1) as usize;
                                            *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                            *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                            pw_a = pw_a.add(1);
                                            pw_b = pw_b.add(1);
                                            p_in = p_in.add(2);
                                        }
                                    }
                                }
                            });
                    }
                    rest => {
                        for (j, buf) in rest.iter_mut().enumerate() {
                            let base = (i + j) * n_queued;
                            buf.par_chunks_mut(chunk_size).enumerate().for_each(
                                |(chunk_idx, chunk)| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk.len() * 2;
                                    let blocks_32 = byte_len / 32;
                                    let remainder = byte_len % 32;

                                    for q_idx in 0..n_queued {
                                        let (mat_lo, mat_hi, ref tlow, ref thigh) =
                                            all_tables[base + q_idx];
                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_buf = chunk.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let separated = _mm256_shuffle_epi8(input, deint_mask);
                                            let vlo =
                                                _mm256_gf2p8affine_epi64_epi8(separated, mat_lo, 0);
                                            let vhi =
                                                _mm256_gf2p8affine_epi64_epi8(separated, mat_hi, 0);
                                            let out = _mm256_unpacklo_epi8(
                                                _mm256_xor_si256(
                                                    vlo,
                                                    _mm256_bsrli_epi128::<8>(vlo),
                                                ),
                                                _mm256_xor_si256(
                                                    vhi,
                                                    _mm256_bsrli_epi128::<8>(vhi),
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_buf,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_buf), out),
                                            );
                                            ptr_in = ptr_in.add(1);
                                            ptr_buf = ptr_buf.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_32 * 16;
                                            let mut pw = chunk[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
                                            let tail_end = p_in.add(remainder);
                                            while p_in < tail_end {
                                                let lo = *p_in as usize;
                                                let hi = *p_in.add(1) as usize;
                                                *pw ^= tlow[lo] ^ thigh[hi];
                                                pw = pw.add(1);
                                                p_in = p_in.add(2);
                                            }
                                        }
                                    }
                                },
                            );
                        }
                    }
                }
            });
    }

    /// GF(2^16) multiply-by-coefficient using AVX-512BW + GFNI instructions.
    ///
    /// The `vgf2p8affineqb` instruction applies an 8×8 GF(2) matrix to each input
    /// byte in a single cycle. Any GF(2^16) multiply-by-constant is a GF(2)-linear
    /// map on 16 bits, which decomposes into four 8×8 matrices (one per pair of
    /// input/output byte halves). Processing 512-bit vectors yields 32 GF(2^16)
    /// words per loop iteration — roughly twice the AVX2 throughput.
    ///
    /// Inner-loop layout (per 512-bit iteration):
    ///   1. De-interleave bytes within each 128-bit lane so lo bytes occupy the
    ///      low qword and hi bytes the high qword.
    ///   2. Apply two GFNI affine transforms (mat_lo, mat_hi) — each call covers
    ///      both the M_ll/M_lh or M_hl/M_hh matrices simultaneously by placing
    ///      different matrices in the two qwords of each lane.
    ///   3. Fold the two qword results within each lane (bsrli + xor) to produce
    ///      the combined lo and hi result bytes.
    ///   4. Re-interleave with `vunpcklbw` and XOR into the recovery buffer.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512bw,gfni")]
    unsafe fn flush_avx512_gfni(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            let buffers = self.buffers.as_normal_mut();
            let logbases = &self.logbases;
            let exponent_start = self.exponent_start;
            let gf = &self.gf;
            let ((), cs) = rayon::join(
                || unsafe {
                    Self::flush_avx512_gfni_work(
                        buffers,
                        &queued,
                        start_index,
                        logbases,
                        exponent_start,
                        gf,
                    )
                },
                || queued.par_iter().map(|s| slice_checksum(s)).collect(),
            );
            cs
        } else {
            unsafe {
                Self::flush_avx512_gfni_work(
                    self.buffers.as_normal_mut(),
                    &queued,
                    start_index,
                    &self.logbases,
                    self.exponent_start,
                    &self.gf,
                );
            }
            Vec::new()
        };

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512bw,gfni")]
    unsafe fn flush_avx512_gfni_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        use std::arch::x86_64::*;

        // Broadcast the de-interleave shuffle to all four 128-bit lanes:
        // within each lane, move lo bytes (even positions 0,2,…,14) to the low
        // qword (positions 0..7) and hi bytes (odd positions 1,3,…,15) to the
        // high qword (positions 8..15). This lets us apply different GFNI
        // matrices to lo vs hi bytes in a single vgf2p8affineqb call.
        let deint_mask = _mm512_broadcast_i32x4(_mm_setr_epi8(
            0, 2, 4, 6, 8, 10, 12, 14, // lo bytes of 8 words → positions 0..7
            1, 3, 5, 7, 9, 11, 13, 15, // hi bytes of 8 words → positions 8..15
        ));

        let n_rec = buffers.len();
        let n_queued = queued.len();

        // Pre-build all coefficient tables in a single parallel pass.
        // Layout: all_tables[rec * n_queued + q_idx].
        let all_tables: Vec<Avx512GfniTable> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                // Decompose GF(2^16) multiply-by-coeff into four 8×8 GF(2) matrices.
                // For each 16-bit word w = (lo_byte, hi_byte):
                //   result_lo = M_ll * lo  ^  M_lh * hi
                //   result_hi = M_hl * lo  ^  M_hh * hi
                //
                // gf2p8affineqb uses byte (7-row) of the u64 matrix operand for
                // output bit `row` — so store each row at position (7-row)*8.
                let mut m_ll = 0u64; // lo input byte → lo output byte
                let mut m_lh = 0u64; // hi input byte → lo output byte
                let mut m_hl = 0u64; // lo input byte → hi output byte
                let mut m_hh = 0u64; // hi input byte → hi output byte
                for row in 0..8usize {
                    let mut row_ll = 0u8;
                    let mut row_lh = 0u8;
                    let mut row_hl = 0u8;
                    let mut row_hh = 0u8;
                    for j in 0..8usize {
                        let r_lo = gf.mul(1u16 << j, coeff);
                        if (r_lo >> row) & 1 == 1 {
                            row_ll |= 1 << j;
                        }
                        if (r_lo >> (row + 8)) & 1 == 1 {
                            row_hl |= 1 << j;
                        }
                        let r_hi = gf.mul(1u16 << (j + 8), coeff);
                        if (r_hi >> row) & 1 == 1 {
                            row_lh |= 1 << j;
                        }
                        if (r_hi >> (row + 8)) & 1 == 1 {
                            row_hh |= 1 << j;
                        }
                    }
                    let shift = (7 - row) * 8;
                    m_ll |= (row_ll as u64) << shift;
                    m_lh |= (row_lh as u64) << shift;
                    m_hl |= (row_hl as u64) << shift;
                    m_hh |= (row_hh as u64) << shift;
                }

                // Each 128-bit lane has two qwords: the low qword handles lo bytes
                // (positions 0..7 after de-interleave) and the high qword handles hi
                // bytes (positions 8..15). Alternating the two matrices in adjacent
                // qwords lets one vgf2p8affineqb cover both contributions at once.
                // _mm512_set_epi64 takes arguments from high (e7) to low (e0).
                let mat_lo = _mm512_set_epi64(
                    m_lh as i64,
                    m_ll as i64, // lane 3: hi→lo, lo→lo
                    m_lh as i64,
                    m_ll as i64, // lane 2
                    m_lh as i64,
                    m_ll as i64, // lane 1
                    m_lh as i64,
                    m_ll as i64, // lane 0
                );
                let mat_hi = _mm512_set_epi64(
                    m_hh as i64,
                    m_hl as i64, // lane 3: hi→hi, lo→hi
                    m_hh as i64,
                    m_hl as i64, // lane 2
                    m_hh as i64,
                    m_hl as i64, // lane 1
                    m_hh as i64,
                    m_hl as i64, // lane 0
                );

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                (mat_lo, mat_hi, table_low, table_high)
            })
            .collect();

        // 2D parallel loop: outer = recovery block groups, inner = 32 KiB chunks of
        // each recovery buffer. 4 × 32 KiB = 128 KiB fits comfortably in L2 on all
        // current AVX-512 CPUs (512 KB L2 on Ice Lake/Sapphire Rapids).
        let chunk_size = 16384usize;

        buffers
            .par_chunks_mut(4)
            .enumerate()
            .for_each(|(group_idx, buf_group)| {
                let i = group_idx * 4;
                match buf_group {
                    // 4-way: one input load + deinterleave feeds 4 recovery blocks.
                    [buf_a, buf_b, buf_c, buf_d] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        let base_c = (i + 2) * n_queued;
                        let base_d = (i + 3) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .zip(buf_c.par_chunks_mut(chunk_size))
                            .zip(buf_d.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(
                                |(chunk_idx, (((chunk_a, chunk_b), chunk_c), chunk_d))| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk_a.len() * 2;
                                    let blocks_64 = byte_len / 64;
                                    let remainder = byte_len % 64;

                                    for q_idx in 0..n_queued {
                                        let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                            all_tables[base_a + q_idx];
                                        let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                            all_tables[base_b + q_idx];
                                        let (mat_lo_c, mat_hi_c, ref tlow_c, ref thigh_c) =
                                            all_tables[base_c + q_idx];
                                        let (mat_lo_d, mat_hi_d, ref tlow_d, ref thigh_d) =
                                            all_tables[base_d + q_idx];
                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m512i;
                                        let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m512i;
                                        let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m512i;
                                        let mut ptr_c = chunk_c.as_mut_ptr() as *mut __m512i;
                                        let mut ptr_d = chunk_d.as_mut_ptr() as *mut __m512i;
                                        let end = ptr_in.add(blocks_64);

                                        while ptr_in < end {
                                            let input = _mm512_loadu_si512(ptr_in.cast());
                                            let separated = _mm512_shuffle_epi8(input, deint_mask);

                                            macro_rules! gfni512_block {
                                                ($mat_lo:expr, $mat_hi:expr, $ptr:expr) => {{
                                                    let vlo = _mm512_gf2p8affine_epi64_epi8(
                                                        separated, $mat_lo, 0,
                                                    );
                                                    let vhi = _mm512_gf2p8affine_epi64_epi8(
                                                        separated, $mat_hi, 0,
                                                    );
                                                    let out = _mm512_unpacklo_epi8(
                                                        _mm512_xor_si512(
                                                            vlo,
                                                            _mm512_bsrli_epi128::<8>(vlo),
                                                        ),
                                                        _mm512_xor_si512(
                                                            vhi,
                                                            _mm512_bsrli_epi128::<8>(vhi),
                                                        ),
                                                    );
                                                    _mm512_storeu_si512(
                                                        ($ptr as *mut __m512i).cast(),
                                                        _mm512_xor_si512(
                                                            _mm512_loadu_si512(
                                                                ($ptr as *const __m512i).cast(),
                                                            ),
                                                            out,
                                                        ),
                                                    );
                                                }};
                                            }

                                            gfni512_block!(mat_lo_a, mat_hi_a, ptr_a);
                                            gfni512_block!(mat_lo_b, mat_hi_b, ptr_b);
                                            gfni512_block!(mat_lo_c, mat_hi_c, ptr_c);
                                            gfni512_block!(mat_lo_d, mat_hi_d, ptr_d);

                                            ptr_in = ptr_in.add(1);
                                            ptr_a = ptr_a.add(1);
                                            ptr_b = ptr_b.add(1);
                                            ptr_c = ptr_c.add(1);
                                            ptr_d = ptr_d.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_64 * 32;
                                            let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                            let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                            let mut pw_c = chunk_c[ow..].as_mut_ptr();
                                            let mut pw_d = chunk_d[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_64 * 64..].as_ptr();
                                            let tail_end = p_in.add(remainder);
                                            while p_in < tail_end {
                                                let lo = *p_in as usize;
                                                let hi = *p_in.add(1) as usize;
                                                *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                                *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                                *pw_c ^= tlow_c[lo] ^ thigh_c[hi];
                                                *pw_d ^= tlow_d[lo] ^ thigh_d[hi];
                                                pw_a = pw_a.add(1);
                                                pw_b = pw_b.add(1);
                                                pw_c = pw_c.add(1);
                                                pw_d = pw_d.add(1);
                                                p_in = p_in.add(2);
                                            }
                                        }
                                    }
                                },
                            );
                    }
                    [buf_a, buf_b] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(|(chunk_idx, (chunk_a, chunk_b))| unsafe {
                                let byte_offset = chunk_idx * chunk_size * 2;
                                let byte_len = chunk_a.len() * 2;
                                let blocks_64 = byte_len / 64;
                                let remainder = byte_len % 64;

                                for q_idx in 0..n_queued {
                                    let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                        all_tables[base_a + q_idx];
                                    let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                        all_tables[base_b + q_idx];
                                    let slice_chunk =
                                        &queued[q_idx][byte_offset..byte_offset + byte_len];

                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m512i;
                                    let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m512i;
                                    let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m512i;
                                    let end = ptr_in.add(blocks_64);

                                    while ptr_in < end {
                                        let input = _mm512_loadu_si512(ptr_in.cast());
                                        let separated = _mm512_shuffle_epi8(input, deint_mask);

                                        // Block A
                                        let vlo_a =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_lo_a, 0);
                                        let new_lo_a = _mm512_xor_si512(
                                            vlo_a,
                                            _mm512_bsrli_epi128::<8>(vlo_a),
                                        );
                                        let vhi_a =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_hi_a, 0);
                                        let new_hi_a = _mm512_xor_si512(
                                            vhi_a,
                                            _mm512_bsrli_epi128::<8>(vhi_a),
                                        );
                                        let out_a = _mm512_unpacklo_epi8(new_lo_a, new_hi_a);
                                        let prev_a = _mm512_loadu_si512(ptr_a.cast());
                                        _mm512_storeu_si512(
                                            ptr_a.cast(),
                                            _mm512_xor_si512(prev_a, out_a),
                                        );

                                        // Block B — reuses `separated`
                                        let vlo_b =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_lo_b, 0);
                                        let new_lo_b = _mm512_xor_si512(
                                            vlo_b,
                                            _mm512_bsrli_epi128::<8>(vlo_b),
                                        );
                                        let vhi_b =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_hi_b, 0);
                                        let new_hi_b = _mm512_xor_si512(
                                            vhi_b,
                                            _mm512_bsrli_epi128::<8>(vhi_b),
                                        );
                                        let out_b = _mm512_unpacklo_epi8(new_lo_b, new_hi_b);
                                        let prev_b = _mm512_loadu_si512(ptr_b.cast());
                                        _mm512_storeu_si512(
                                            ptr_b.cast(),
                                            _mm512_xor_si512(prev_b, out_b),
                                        );

                                        ptr_in = ptr_in.add(1);
                                        ptr_a = ptr_a.add(1);
                                        ptr_b = ptr_b.add(1);
                                    }

                                    if remainder > 0 {
                                        let ow = blocks_64 * 32;
                                        let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                        let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                        let mut p_in = slice_chunk[blocks_64 * 64..].as_ptr();
                                        let tail_end = p_in.add(remainder);
                                        while p_in < tail_end {
                                            let lo = *p_in as usize;
                                            let hi = *p_in.add(1) as usize;
                                            *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                            *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                            pw_a = pw_a.add(1);
                                            pw_b = pw_b.add(1);
                                            p_in = p_in.add(2);
                                        }
                                    }
                                }
                            });
                    }
                    [buf_a] => {
                        let base = i * n_queued;
                        buf_a.par_chunks_mut(chunk_size).enumerate().for_each(
                            |(chunk_idx, chunk_a)| unsafe {
                                let byte_offset = chunk_idx * chunk_size * 2;
                                let byte_len = chunk_a.len() * 2;
                                let blocks_64 = byte_len / 64;
                                let remainder = byte_len % 64;

                                for q_idx in 0..n_queued {
                                    let (mat_lo, mat_hi, ref table_low, ref table_high) =
                                        all_tables[base + q_idx];
                                    let slice_chunk =
                                        &queued[q_idx][byte_offset..byte_offset + byte_len];

                                    let mut ptr_buf = chunk_a.as_mut_ptr() as *mut __m512i;
                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m512i;
                                    let end = ptr_in.add(blocks_64);

                                    while ptr_in < end {
                                        let input = _mm512_loadu_si512(ptr_in.cast());
                                        let separated = _mm512_shuffle_epi8(input, deint_mask);

                                        let v_lo =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_lo, 0);
                                        let new_lo =
                                            _mm512_xor_si512(v_lo, _mm512_bsrli_epi128::<8>(v_lo));
                                        let v_hi =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_hi, 0);
                                        let new_hi =
                                            _mm512_xor_si512(v_hi, _mm512_bsrli_epi128::<8>(v_hi));
                                        let out = _mm512_unpacklo_epi8(new_lo, new_hi);
                                        let prev = _mm512_loadu_si512(ptr_buf.cast());
                                        _mm512_storeu_si512(
                                            ptr_buf.cast(),
                                            _mm512_xor_si512(prev, out),
                                        );

                                        ptr_in = ptr_in.add(1);
                                        ptr_buf = ptr_buf.add(1);
                                    }

                                    if remainder > 0 {
                                        let ow = blocks_64 * 32;
                                        let mut pw = chunk_a[ow..].as_mut_ptr();
                                        let mut p_in = slice_chunk[blocks_64 * 64..].as_ptr();
                                        let tail_end = p_in.add(remainder);
                                        while p_in < tail_end {
                                            let lo = *p_in as usize;
                                            let hi = *p_in.add(1) as usize;
                                            *pw ^= table_low[lo] ^ table_high[hi];
                                            pw = pw.add(1);
                                            p_in = p_in.add(2);
                                        }
                                    }
                                }
                            },
                        );
                    }
                    _ => {}
                }
            });
    }

    /// ALTMAP XOR bit-dependency kernel (Phase 27e).
    ///
    /// Transposes each queued raw slice into ALTMAP layout, then applies the
    /// pre-built dep-matrix table via `vpxor` — one 256-bit vector per
    /// (output-plane, vec-index) pair.  4-way unroll over recovery blocks.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    unsafe fn flush_avx2_altmap(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        let slice_words = self.slice_words;
        let altmap_slices: Vec<Vec<u8>> = queued
            .par_iter()
            .map(|s| {
                let mut am = vec![0u8; super::altmap::altmap_size(slice_words)];
                // SAFETY: slice_size is always even; s is exactly slice_size bytes.
                let words =
                    unsafe { std::slice::from_raw_parts(s.as_ptr().cast::<u16>(), slice_words) };
                super::altmap::to_altmap(words, &mut am);
                am
            })
            .collect();

        let dep_tables = self
            .dep_tables
            .as_deref()
            .expect("dep_tables must be built for ALTMAP path");

        let buffers = match &mut self.buffers {
            RecoveryBufferSet::Altmap(b) => b.as_mut_slice(),
            _ => panic!("flush_avx2_altmap called on non-ALTMAP encoder"),
        };

        unsafe {
            Self::flush_avx2_altmap_work(
                buffers,
                &altmap_slices,
                start_index,
                &self.logbases,
                self.exponent_start,
                dep_tables,
                &self.gf,
            );
        }

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    /// Static worker for [`flush_avx2_altmap`].
    ///
    /// `buffers`: one `Vec<u8>` per recovery block (ALTMAP layout).
    /// `queued`:  one `Vec<u8>` per input slice (already in ALTMAP layout).
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    #[allow(clippy::needless_range_loop)]
    unsafe fn flush_avx2_altmap_work(
        buffers: &mut [Vec<u8>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        dep_tables: &[[u16; 16]; 65536],
        gf: &Gf16,
    ) {
        use std::arch::x86_64::*;

        let n_rec = buffers.len();
        if n_rec == 0 || queued.is_empty() {
            return;
        }

        // plane_bytes = slice_words / 8.  ALTMAP invariant: buf.len() == plane_bytes * 16.
        let plane_bytes = buffers[0].len() / 16;
        let n_vec = plane_bytes / 32; // full 256-bit vectors per plane section

        // Process 4 recovery blocks at a time.
        buffers
            .par_chunks_mut(4)
            .enumerate()
            .for_each(|(chunk_idx, rec_chunk)| {
                let rec_base = chunk_idx * 4;
                let chunk_len = rec_chunk.len(); // 1..=4

                for (q, slice_am) in queued.iter().enumerate() {
                    let slice_index = start_index + q;
                    let logbase = logbases[slice_index] as u64;

                    // Coefficient = antilog[(logbase * exponent) % ORDER].
                    let mut coeffs = [0u16; 4];
                    for r in 0..chunk_len {
                        let exponent = exponent_start + (rec_base + r) as u32;
                        let log_coeff =
                            ((logbase * exponent as u64) % super::gf16::ORDER as u64) as u32;
                        coeffs[r] = gf.exp(log_coeff);
                    }

                    // AVX2 path: one 256-bit vector per plane per vec-index.
                    for vi in 0..n_vec {
                        // Load 16 input planes at this vector position.
                        let mut in_planes = [_mm256_setzero_si256(); 16];
                        for p in 0..16usize {
                            let off = p * plane_bytes + vi * 32;
                            // SAFETY: bounds guaranteed by ALTMAP layout invariant.
                            in_planes[p] =
                                unsafe { _mm256_loadu_si256(slice_am.as_ptr().add(off).cast()) };
                        }

                        for r in 0..chunk_len {
                            let coeff = coeffs[r];
                            if coeff == 0 {
                                continue;
                            }
                            let deps = &dep_tables[coeff as usize];
                            for plane_out in 0..16usize {
                                let mask = deps[plane_out];
                                if mask == 0 {
                                    continue;
                                }
                                let mut acc = _mm256_setzero_si256();
                                for plane_in in 0..16usize {
                                    if (mask >> plane_in) & 1 == 1 {
                                        acc = _mm256_xor_si256(acc, in_planes[plane_in]);
                                    }
                                }
                                let off = plane_out * plane_bytes + vi * 32;
                                // SAFETY: off + 32 <= plane_bytes * 16 == buf.len().
                                let ptr = rec_chunk[r].as_mut_ptr().add(off).cast::<__m256i>();
                                let prev = unsafe { _mm256_loadu_si256(ptr) };
                                unsafe {
                                    _mm256_storeu_si256(ptr, _mm256_xor_si256(prev, acc));
                                }
                            }
                        }
                    }

                    // Scalar tail for remainder bytes within each plane.
                    let tail_start = n_vec * 32;
                    if tail_start < plane_bytes {
                        for r in 0..chunk_len {
                            let coeff = coeffs[r];
                            if coeff == 0 {
                                continue;
                            }
                            let deps = &dep_tables[coeff as usize];
                            for plane_out in 0..16usize {
                                let mask = deps[plane_out];
                                if mask == 0 {
                                    continue;
                                }
                                for byte_off in tail_start..plane_bytes {
                                    let mut acc: u8 = 0;
                                    for plane_in in 0..16usize {
                                        if (mask >> plane_in) & 1 == 1 {
                                            acc ^= slice_am[plane_in * plane_bytes + byte_off];
                                        }
                                    }
                                    rec_chunk[r][plane_out * plane_bytes + byte_off] ^= acc;
                                }
                            }
                        }
                    }
                }
            });
    }

    /// Same 4-nibble shuffle algorithm as `flush_avx2` but operating on 128-bit
    /// `__m128i` registers. Covers all x86-64 CPUs with SSSE3 (2007+) that do
    /// not have AVX2.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "ssse3")]
    unsafe fn flush_ssse3(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            let buffers = self.buffers.as_normal_mut();
            let logbases = &self.logbases;
            let exponent_start = self.exponent_start;
            let gf = &self.gf;
            let ((), cs) = rayon::join(
                || unsafe {
                    Self::flush_ssse3_work(
                        buffers,
                        &queued,
                        start_index,
                        logbases,
                        exponent_start,
                        gf,
                    )
                },
                || queued.par_iter().map(|s| slice_checksum(s)).collect(),
            );
            cs
        } else {
            unsafe {
                Self::flush_ssse3_work(
                    self.buffers.as_normal_mut(),
                    &queued,
                    start_index,
                    &self.logbases,
                    self.exponent_start,
                    &self.gf,
                );
            }
            Vec::new()
        };

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "ssse3")]
    unsafe fn flush_ssse3_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        let mask_f = _mm_set1_epi8(0x0F_u8 as i8);
        let mask_even = _mm_set1_epi16(0x00FF_u16 as i16);

        let n_rec = buffers.len();
        let n_queued = queued.len();

        // Pre-build all SIMD coefficient tables in a single parallel pass — one Vec
        // entry per (recovery_block × input_slice) pair, laid out as [rec * n_queued + q].
        let all_tables: Vec<Ssse3Table> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| unsafe {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut tl_l = [0u8; 16];
                let mut tl_h = [0u8; 16];
                let mut th_l = [0u8; 16];
                let mut th_h = [0u8; 16];
                let mut hl_l = [0u8; 16];
                let mut hl_h = [0u8; 16];
                let mut hh_l = [0u8; 16];
                let mut hh_h = [0u8; 16];

                for val in 0..16usize {
                    let r0 = gf.mul(val as u16, coeff);
                    tl_l[val] = (r0 & 0xFF) as u8;
                    th_l[val] = (r0 >> 8) as u8;
                    let r1 = gf.mul((val as u16) << 4, coeff);
                    tl_h[val] = (r1 & 0xFF) as u8;
                    th_h[val] = (r1 >> 8) as u8;
                    let r2 = gf.mul((val as u16) << 8, coeff);
                    hl_l[val] = (r2 & 0xFF) as u8;
                    hh_l[val] = (r2 >> 8) as u8;
                    let r3 = gf.mul((val as u16) << 12, coeff);
                    hl_h[val] = (r3 & 0xFF) as u8;
                    hh_h[val] = (r3 >> 8) as u8;
                }

                let v_tl_l = _mm_loadu_si128(tl_l.as_ptr() as *const __m128i);
                let v_tl_h = _mm_loadu_si128(tl_h.as_ptr() as *const __m128i);
                let v_th_l = _mm_loadu_si128(th_l.as_ptr() as *const __m128i);
                let v_th_h = _mm_loadu_si128(th_h.as_ptr() as *const __m128i);
                let v_hl_l = _mm_loadu_si128(hl_l.as_ptr() as *const __m128i);
                let v_hl_h = _mm_loadu_si128(hl_h.as_ptr() as *const __m128i);
                let v_hh_l = _mm_loadu_si128(hh_l.as_ptr() as *const __m128i);
                let v_hh_h = _mm_loadu_si128(hh_h.as_ptr() as *const __m128i);

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                (
                    v_tl_l, v_tl_h, v_th_l, v_th_h, v_hl_l, v_hl_h, v_hh_l, v_hh_h, table_low,
                    table_high,
                )
            })
            .collect();

        // Chunk-outer loop: all rayon tasks rendezvous at each chunk boundary so
        // all threads read the same 4 MiB input window → L3 hits (same strategy as AVX2).
        let slice_words = queued[0].len() / 2;
        let chunk_size = 16384usize; // 32 KiB recovery buffer chunk stays in L1/L2
        let n_chunks = slice_words.div_ceil(chunk_size);

        for chunk_idx in 0..n_chunks {
            let word_start = chunk_idx * chunk_size;
            let word_end = (word_start + chunk_size).min(slice_words);
            let byte_offset = word_start * 2;
            let byte_len = (word_end - word_start) * 2;
            let blocks_16 = byte_len / 16;
            let remainder = byte_len % 16;

            buffers
                .par_chunks_mut(2)
                .enumerate()
                .for_each(|(pair_idx, buf_pair)| unsafe {
                    let i = pair_idx * 2;
                    match buf_pair {
                        [buf_a, buf_b] => {
                            let base_a = i * n_queued;
                            let base_b = (i + 1) * n_queued;
                            let chunk_a = &mut buf_a[word_start..word_end];
                            let chunk_b = &mut buf_b[word_start..word_end];

                            for q_idx in 0..n_queued {
                                let (
                                    v_tl_l_a,
                                    v_tl_h_a,
                                    v_th_l_a,
                                    v_th_h_a,
                                    v_hl_l_a,
                                    v_hl_h_a,
                                    v_hh_l_a,
                                    v_hh_h_a,
                                    ref tlow_a,
                                    ref thigh_a,
                                ) = all_tables[base_a + q_idx];
                                let (
                                    v_tl_l_b,
                                    v_tl_h_b,
                                    v_th_l_b,
                                    v_th_h_b,
                                    v_hl_l_b,
                                    v_hl_h_b,
                                    v_hh_l_b,
                                    v_hh_h_b,
                                    ref tlow_b,
                                    ref thigh_b,
                                ) = all_tables[base_b + q_idx];
                                let slice_chunk =
                                    &queued[q_idx][byte_offset..byte_offset + byte_len];

                                let mut ptr_in = slice_chunk.as_ptr() as *const __m128i;
                                let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m128i;
                                let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m128i;
                                let end = ptr_in.add(blocks_16);

                                while ptr_in < end {
                                    let input = _mm_loadu_si128(ptr_in);
                                    let n0_2 = _mm_and_si128(input, mask_f);
                                    let n1_3 = _mm_and_si128(_mm_srli_epi16(input, 4), mask_f);

                                    // Block A
                                    let rle_a = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_tl_l_a, n0_2),
                                        _mm_shuffle_epi8(v_tl_h_a, n1_3),
                                    );
                                    let rhe_a = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_th_l_a, n0_2),
                                        _mm_shuffle_epi8(v_th_h_a, n1_3),
                                    );
                                    let rlo_a = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hl_l_a, n0_2),
                                        _mm_shuffle_epi8(v_hl_h_a, n1_3),
                                    );
                                    let rho_a = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hh_l_a, n0_2),
                                        _mm_shuffle_epi8(v_hh_h_a, n1_3),
                                    );
                                    let sle_a = _mm_xor_si128(rle_a, _mm_srli_epi16(rlo_a, 8));
                                    let she_a = _mm_xor_si128(rhe_a, _mm_srli_epi16(rho_a, 8));
                                    let out_a = _mm_or_si128(
                                        _mm_and_si128(sle_a, mask_even),
                                        _mm_slli_epi16(she_a, 8),
                                    );
                                    let prev_a = _mm_loadu_si128(ptr_a);
                                    _mm_storeu_si128(ptr_a, _mm_xor_si128(prev_a, out_a));

                                    // Block B — reuses n0_2 and n1_3
                                    let rle_b = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_tl_l_b, n0_2),
                                        _mm_shuffle_epi8(v_tl_h_b, n1_3),
                                    );
                                    let rhe_b = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_th_l_b, n0_2),
                                        _mm_shuffle_epi8(v_th_h_b, n1_3),
                                    );
                                    let rlo_b = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hl_l_b, n0_2),
                                        _mm_shuffle_epi8(v_hl_h_b, n1_3),
                                    );
                                    let rho_b = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hh_l_b, n0_2),
                                        _mm_shuffle_epi8(v_hh_h_b, n1_3),
                                    );
                                    let sle_b = _mm_xor_si128(rle_b, _mm_srli_epi16(rlo_b, 8));
                                    let she_b = _mm_xor_si128(rhe_b, _mm_srli_epi16(rho_b, 8));
                                    let out_b = _mm_or_si128(
                                        _mm_and_si128(sle_b, mask_even),
                                        _mm_slli_epi16(she_b, 8),
                                    );
                                    let prev_b = _mm_loadu_si128(ptr_b);
                                    _mm_storeu_si128(ptr_b, _mm_xor_si128(prev_b, out_b));

                                    ptr_in = ptr_in.add(1);
                                    ptr_a = ptr_a.add(1);
                                    ptr_b = ptr_b.add(1);
                                }

                                if remainder > 0 {
                                    let ow = blocks_16 * 8;
                                    let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                    let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                    let mut p_in = slice_chunk[blocks_16 * 16..].as_ptr();
                                    let tail_end = p_in.add(remainder);
                                    while p_in < tail_end {
                                        let lo = *p_in as usize;
                                        let hi = *p_in.add(1) as usize;
                                        *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                        *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                        pw_a = pw_a.add(1);
                                        pw_b = pw_b.add(1);
                                        p_in = p_in.add(2);
                                    }
                                }
                            }
                        }
                        [buf_a] => {
                            let base = i * n_queued;
                            let chunk_a = &mut buf_a[word_start..word_end];

                            for q_idx in 0..n_queued {
                                let (
                                    v_tl_l,
                                    v_tl_h,
                                    v_th_l,
                                    v_th_h,
                                    v_hl_l,
                                    v_hl_h,
                                    v_hh_l,
                                    v_hh_h,
                                    ref table_low,
                                    ref table_high,
                                ) = all_tables[base + q_idx];
                                let slice_chunk =
                                    &queued[q_idx][byte_offset..byte_offset + byte_len];

                                let mut ptr_buf = chunk_a.as_mut_ptr() as *mut __m128i;
                                let mut ptr_in = slice_chunk.as_ptr() as *const __m128i;
                                let end = ptr_in.add(blocks_16);

                                while ptr_in < end {
                                    let input = _mm_loadu_si128(ptr_in);
                                    let n0_2 = _mm_and_si128(input, mask_f);
                                    let n1_3 = _mm_and_si128(_mm_srli_epi16(input, 4), mask_f);
                                    let res_lo_even = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_tl_l, n0_2),
                                        _mm_shuffle_epi8(v_tl_h, n1_3),
                                    );
                                    let res_hi_even = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_th_l, n0_2),
                                        _mm_shuffle_epi8(v_th_h, n1_3),
                                    );
                                    let res_lo_odd = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hl_l, n0_2),
                                        _mm_shuffle_epi8(v_hl_h, n1_3),
                                    );
                                    let res_hi_odd = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hh_l, n0_2),
                                        _mm_shuffle_epi8(v_hh_h, n1_3),
                                    );
                                    let sum_lo =
                                        _mm_xor_si128(res_lo_even, _mm_srli_epi16(res_lo_odd, 8));
                                    let sum_hi =
                                        _mm_xor_si128(res_hi_even, _mm_srli_epi16(res_hi_odd, 8));
                                    let out = _mm_or_si128(
                                        _mm_and_si128(sum_lo, mask_even),
                                        _mm_slli_epi16(sum_hi, 8),
                                    );
                                    let prev = _mm_loadu_si128(ptr_buf);
                                    _mm_storeu_si128(ptr_buf, _mm_xor_si128(prev, out));
                                    ptr_in = ptr_in.add(1);
                                    ptr_buf = ptr_buf.add(1);
                                }

                                if remainder > 0 {
                                    let ow = blocks_16 * 8;
                                    let mut pw = chunk_a[ow..].as_mut_ptr();
                                    let mut p_in = slice_chunk[blocks_16 * 16..].as_ptr();
                                    let tail_end = p_in.add(remainder);
                                    while p_in < tail_end {
                                        let lo = *p_in as usize;
                                        let hi = *p_in.add(1) as usize;
                                        *pw ^= table_low[lo] ^ table_high[hi];
                                        pw = pw.add(1);
                                        p_in = p_in.add(2);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                });
        }
    }

    /// Same 4-nibble shuffle algorithm as `flush_ssse3` for AArch64 targets.
    /// `vqtbl1q_u8` is the NEON equivalent of `_mm_shuffle_epi8`; NEON is
    /// mandatory on all AArch64 hardware so no runtime detection is needed.
    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    unsafe fn flush_neon(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            let buffers = self.buffers.as_normal_mut();
            let logbases = &self.logbases;
            let exponent_start = self.exponent_start;
            let gf = &self.gf;
            let ((), cs) = rayon::join(
                || unsafe {
                    Self::flush_neon_work(
                        buffers,
                        &queued,
                        start_index,
                        logbases,
                        exponent_start,
                        gf,
                    )
                },
                || queued.par_iter().map(|s| slice_checksum(s)).collect(),
            );
            cs
        } else {
            unsafe {
                Self::flush_neon_work(
                    self.buffers.as_normal_mut(),
                    &queued,
                    start_index,
                    &self.logbases,
                    self.exponent_start,
                    &self.gf,
                );
            }
            Vec::new()
        };

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    unsafe fn flush_neon_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        use std::arch::aarch64::*;

        let mask_f = vdupq_n_u8(0x0F);
        // 0x00FF per 16-bit lane: bytes [0xFF, 0x00, 0xFF, 0x00, ...].
        let mask_even = vreinterpretq_u8_u16(vdupq_n_u16(0x00FF));

        buffers.par_iter_mut().enumerate().for_each(|(i, buffer)| {
            let exponent = exponent_start + i as u32;

            let mut tables = Vec::with_capacity(queued.len());
            for (q_idx, _) in queued.iter().enumerate() {
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut tl_l = [0u8; 16];
                let mut tl_h = [0u8; 16];
                let mut th_l = [0u8; 16];
                let mut th_h = [0u8; 16];
                let mut hl_l = [0u8; 16];
                let mut hl_h = [0u8; 16];
                let mut hh_l = [0u8; 16];
                let mut hh_h = [0u8; 16];

                for val in 0..16usize {
                    let r0 = gf.mul(val as u16, coeff);
                    tl_l[val] = (r0 & 0xFF) as u8;
                    th_l[val] = (r0 >> 8) as u8;

                    let r1 = gf.mul((val as u16) << 4, coeff);
                    tl_h[val] = (r1 & 0xFF) as u8;
                    th_h[val] = (r1 >> 8) as u8;

                    let r2 = gf.mul((val as u16) << 8, coeff);
                    hl_l[val] = (r2 & 0xFF) as u8;
                    hh_l[val] = (r2 >> 8) as u8;

                    let r3 = gf.mul((val as u16) << 12, coeff);
                    hl_h[val] = (r3 & 0xFF) as u8;
                    hh_h[val] = (r3 >> 8) as u8;
                }

                let v_tl_l = vld1q_u8(tl_l.as_ptr());
                let v_tl_h = vld1q_u8(tl_h.as_ptr());
                let v_th_l = vld1q_u8(th_l.as_ptr());
                let v_th_h = vld1q_u8(th_h.as_ptr());
                let v_hl_l = vld1q_u8(hl_l.as_ptr());
                let v_hl_h = vld1q_u8(hl_h.as_ptr());
                let v_hh_l = vld1q_u8(hh_l.as_ptr());
                let v_hh_h = vld1q_u8(hh_h.as_ptr());

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                tables.push((
                    v_tl_l, v_tl_h, v_th_l, v_th_h, v_hl_l, v_hl_h, v_hh_l, v_hh_h, table_low,
                    table_high,
                ));
            }

            // 16384 words == 32 KiB: keeps the recovery chunk L1-resident
            // across all queued input slices.
            let chunk_size = 16384;
            for (chunk_idx, buffer_chunk) in buffer.chunks_mut(chunk_size).enumerate() {
                let byte_offset = chunk_idx * chunk_size * 2;
                let byte_len = buffer_chunk.len() * 2;
                let blocks_16 = byte_len / 16;
                let remainder = byte_len % 16;

                for (q_idx, slice) in queued.iter().enumerate() {
                    let slice_chunk = &slice[byte_offset..byte_offset + byte_len];
                    let (
                        v_tl_l,
                        v_tl_h,
                        v_th_l,
                        v_th_h,
                        v_hl_l,
                        v_hl_h,
                        v_hh_l,
                        v_hh_h,
                        ref table_low,
                        ref table_high,
                    ) = tables[q_idx];

                    let mut ptr_buf = buffer_chunk.as_mut_ptr() as *mut u8;
                    let mut ptr_in = slice_chunk.as_ptr();
                    let end = ptr_in.add(blocks_16 * 16);

                    while ptr_in.add(64) <= end {
                        let input0 = vld1q_u8(ptr_in);
                        let input1 = vld1q_u8(ptr_in.add(16));
                        let input2 = vld1q_u8(ptr_in.add(32));
                        let input3 = vld1q_u8(ptr_in.add(48));

                        // Nibble extraction
                        let n0_2_0 = vandq_u8(input0, mask_f);
                        let n1_3_0 = vandq_u8(
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(input0), 4)),
                            mask_f,
                        );
                        let n0_2_1 = vandq_u8(input1, mask_f);
                        let n1_3_1 = vandq_u8(
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(input1), 4)),
                            mask_f,
                        );
                        let n0_2_2 = vandq_u8(input2, mask_f);
                        let n1_3_2 = vandq_u8(
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(input2), 4)),
                            mask_f,
                        );
                        let n0_2_3 = vandq_u8(input3, mask_f);
                        let n1_3_3 = vandq_u8(
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(input3), 4)),
                            mask_f,
                        );

                        // Lookup lo-bytes even
                        let rle0 = veorq_u8(vqtbl1q_u8(v_tl_l, n0_2_0), vqtbl1q_u8(v_tl_h, n1_3_0));
                        let rle1 = veorq_u8(vqtbl1q_u8(v_tl_l, n0_2_1), vqtbl1q_u8(v_tl_h, n1_3_1));
                        let rle2 = veorq_u8(vqtbl1q_u8(v_tl_l, n0_2_2), vqtbl1q_u8(v_tl_h, n1_3_2));
                        let rle3 = veorq_u8(vqtbl1q_u8(v_tl_l, n0_2_3), vqtbl1q_u8(v_tl_h, n1_3_3));

                        // Lookup hi-bytes even
                        let rhe0 = veorq_u8(vqtbl1q_u8(v_th_l, n0_2_0), vqtbl1q_u8(v_th_h, n1_3_0));
                        let rhe1 = veorq_u8(vqtbl1q_u8(v_th_l, n0_2_1), vqtbl1q_u8(v_th_h, n1_3_1));
                        let rhe2 = veorq_u8(vqtbl1q_u8(v_th_l, n0_2_2), vqtbl1q_u8(v_th_h, n1_3_2));
                        let rhe3 = veorq_u8(vqtbl1q_u8(v_th_l, n0_2_3), vqtbl1q_u8(v_th_h, n1_3_3));

                        // Lookup lo-bytes odd
                        let rlo0 = veorq_u8(vqtbl1q_u8(v_hl_l, n0_2_0), vqtbl1q_u8(v_hl_h, n1_3_0));
                        let rlo1 = veorq_u8(vqtbl1q_u8(v_hl_l, n0_2_1), vqtbl1q_u8(v_hl_h, n1_3_1));
                        let rlo2 = veorq_u8(vqtbl1q_u8(v_hl_l, n0_2_2), vqtbl1q_u8(v_hl_h, n1_3_2));
                        let rlo3 = veorq_u8(vqtbl1q_u8(v_hl_l, n0_2_3), vqtbl1q_u8(v_hl_h, n1_3_3));

                        // Lookup hi-bytes odd
                        let rho0 = veorq_u8(vqtbl1q_u8(v_hh_l, n0_2_0), vqtbl1q_u8(v_hh_h, n1_3_0));
                        let rho1 = veorq_u8(vqtbl1q_u8(v_hh_l, n0_2_1), vqtbl1q_u8(v_hh_h, n1_3_1));
                        let rho2 = veorq_u8(vqtbl1q_u8(v_hh_l, n0_2_2), vqtbl1q_u8(v_hh_h, n1_3_2));
                        let rho3 = veorq_u8(vqtbl1q_u8(v_hh_l, n0_2_3), vqtbl1q_u8(v_hh_h, n1_3_3));

                        // Final Summing & Packing 0
                        let sum_lo0 = veorq_u8(
                            rle0,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(rlo0), 8)),
                        );
                        let sum_hi0 = veorq_u8(
                            rhe0,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(rho0), 8)),
                        );
                        let out0 = vorrq_u8(
                            vandq_u8(sum_lo0, mask_even),
                            vreinterpretq_u8_u16(vshlq_n_u16(vreinterpretq_u16_u8(sum_hi0), 8)),
                        );
                        vst1q_u8(ptr_buf, veorq_u8(vld1q_u8(ptr_buf), out0));

                        // Final Summing & Packing 1
                        let sum_lo1 = veorq_u8(
                            rle1,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(rlo1), 8)),
                        );
                        let sum_hi1 = veorq_u8(
                            rhe1,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(rho1), 8)),
                        );
                        let out1 = vorrq_u8(
                            vandq_u8(sum_lo1, mask_even),
                            vreinterpretq_u8_u16(vshlq_n_u16(vreinterpretq_u16_u8(sum_hi1), 8)),
                        );
                        vst1q_u8(ptr_buf.add(16), veorq_u8(vld1q_u8(ptr_buf.add(16)), out1));

                        // Final Summing & Packing 2
                        let sum_lo2 = veorq_u8(
                            rle2,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(rlo2), 8)),
                        );
                        let sum_hi2 = veorq_u8(
                            rhe2,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(rho2), 8)),
                        );
                        let out2 = vorrq_u8(
                            vandq_u8(sum_lo2, mask_even),
                            vreinterpretq_u8_u16(vshlq_n_u16(vreinterpretq_u16_u8(sum_hi2), 8)),
                        );
                        vst1q_u8(ptr_buf.add(32), veorq_u8(vld1q_u8(ptr_buf.add(32)), out2));

                        // Final Summing & Packing 3
                        let sum_lo3 = veorq_u8(
                            rle3,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(rlo3), 8)),
                        );
                        let sum_hi3 = veorq_u8(
                            rhe3,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(rho3), 8)),
                        );
                        let out3 = vorrq_u8(
                            vandq_u8(sum_lo3, mask_even),
                            vreinterpretq_u8_u16(vshlq_n_u16(vreinterpretq_u16_u8(sum_hi3), 8)),
                        );
                        vst1q_u8(ptr_buf.add(48), veorq_u8(vld1q_u8(ptr_buf.add(48)), out3));

                        ptr_in = ptr_in.add(64);
                        ptr_buf = ptr_buf.add(64);
                    }

                    while ptr_in < end {
                        let input = vld1q_u8(ptr_in);

                        // n0_2: low nibble of each byte (n0 for lo-bytes, n2 for hi-bytes).
                        let n0_2 = vandq_u8(input, mask_f);
                        // n1_3: high nibble of each byte via 16-bit logical shift right by 4.
                        let n1_3 = vandq_u8(
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(input), 4)),
                            mask_f,
                        );

                        let res_lo_even =
                            veorq_u8(vqtbl1q_u8(v_tl_l, n0_2), vqtbl1q_u8(v_tl_h, n1_3));
                        let res_hi_even =
                            veorq_u8(vqtbl1q_u8(v_th_l, n0_2), vqtbl1q_u8(v_th_h, n1_3));
                        let res_lo_odd =
                            veorq_u8(vqtbl1q_u8(v_hl_l, n0_2), vqtbl1q_u8(v_hl_h, n1_3));
                        let res_hi_odd =
                            veorq_u8(vqtbl1q_u8(v_hh_l, n0_2), vqtbl1q_u8(v_hh_h, n1_3));

                        // Combine even-byte and odd-byte contributions.
                        // srli_epi16(x, 8) == vshrq_n_u16 reinterpreted as u8.
                        let sum_lo_even = veorq_u8(
                            res_lo_even,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(res_lo_odd), 8)),
                        );
                        let sum_hi_even = veorq_u8(
                            res_hi_even,
                            vreinterpretq_u8_u16(vshrq_n_u16(vreinterpretq_u16_u8(res_hi_odd), 8)),
                        );

                        // Pack: low byte of each output word from sum_lo_even,
                        // high byte from sum_hi_even (shifted left 8 within each u16 lane).
                        let r_l_masked = vandq_u8(sum_lo_even, mask_even);
                        let r_h_shifted =
                            vreinterpretq_u8_u16(vshlq_n_u16(vreinterpretq_u16_u8(sum_hi_even), 8));
                        let out = vorrq_u8(r_l_masked, r_h_shifted);

                        let prev = vld1q_u8(ptr_buf);
                        vst1q_u8(ptr_buf, veorq_u8(prev, out));

                        ptr_in = ptr_in.add(16);
                        ptr_buf = ptr_buf.add(16);
                    }

                    if remainder > 0 {
                        let offset_words = blocks_16 * 8;
                        let mut ptr_word = buffer_chunk[offset_words..].as_mut_ptr();
                        let mut p_in = slice_chunk[blocks_16 * 16..].as_ptr();
                        let tail_end = p_in.add(remainder);

                        while p_in < tail_end {
                            let lo = *p_in as usize;
                            let hi = *p_in.add(1) as usize;
                            *ptr_word ^= table_low[lo] ^ table_high[hi];
                            ptr_word = ptr_word.add(1);
                            p_in = p_in.add(2);
                        }
                    }
                }
            }
        });
    }

    fn flush_scalar(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            let buffers = self.buffers.as_normal_mut();
            let logbases = &self.logbases;
            let exponent_start = self.exponent_start;
            let gf = &self.gf;
            let ((), cs) = rayon::join(
                || {
                    Self::flush_scalar_work(
                        buffers,
                        &queued,
                        start_index,
                        logbases,
                        exponent_start,
                        gf,
                    )
                },
                || queued.par_iter().map(|s| slice_checksum(s)).collect(),
            );
            cs
        } else {
            Self::flush_scalar_work(
                self.buffers.as_normal_mut(),
                &queued,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
            Vec::new()
        };

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    pub(super) fn flush_scalar_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        buffers.par_iter_mut().enumerate().for_each(|(i, buffer)| {
            let exponent = exponent_start + i as u32;

            let mut tables = Vec::with_capacity(queued.len());
            for (q_idx, _) in queued.iter().enumerate() {
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255 {
                    table_low[b as usize] = gf.mul(b as u16, coeff);
                    table_high[b as usize] = gf.mul((b as u16) << 8, coeff);
                }
                tables.push((table_low, table_high));
            }

            let chunk_size = 16384;
            for (chunk_idx, buffer_chunk) in buffer.chunks_mut(chunk_size).enumerate() {
                let byte_offset = chunk_idx * chunk_size * 2;
                let byte_len = buffer_chunk.len() * 2;

                for (q_idx, slice) in queued.iter().enumerate() {
                    let slice_chunk = &slice[byte_offset..byte_offset + byte_len];
                    let (ref table_low, ref table_high) = tables[q_idx];

                    for (word, chunk) in buffer_chunk.iter_mut().zip(slice_chunk.chunks_exact(2)) {
                        *word ^= table_low[chunk[0] as usize] ^ table_high[chunk[1] as usize];
                    }
                }
            }
        });
    }

    /// Consume the encoder and return the finished recovery slices together
    /// with all accumulated per-slice checksums (empty when checksums were
    /// not enabled via [`with_checksums`]).
    pub fn finish(mut self) -> (Vec<RecoverySlice>, Vec<SliceChecksum>) {
        self.flush();
        let checksums = self.pending_checksums;
        let exponent_start = self.exponent_start;
        let slice_words = self.slice_words;
        let slices: Vec<RecoverySlice> = match self.buffers {
            RecoveryBufferSet::Normal(bufs) => bufs
                .into_par_iter()
                .enumerate()
                .map(|(i, buffer)| {
                    let mut data = Vec::with_capacity(buffer.len() * 2);
                    for word in buffer {
                        data.extend_from_slice(&word.to_le_bytes());
                    }
                    RecoverySlice {
                        exponent: exponent_start + i as u32,
                        data,
                    }
                })
                .collect(),
            RecoveryBufferSet::Altmap(bufs) => bufs
                .into_par_iter()
                .enumerate()
                .map(|(i, altmap_buf)| {
                    let mut words = vec![0u16; slice_words];
                    super::altmap::from_altmap(&altmap_buf, &mut words);
                    let mut data = Vec::with_capacity(slice_words * 2);
                    for word in words {
                        data.extend_from_slice(&word.to_le_bytes());
                    }
                    RecoverySlice {
                        exponent: exponent_start + i as u32,
                        data,
                    }
                })
                .collect(),
            RecoveryBufferSet::Shuffle2x(bufs) => bufs
                .into_par_iter()
                .enumerate()
                .map(|(i, s2x_buf)| {
                    let mut normal = vec![0u8; s2x_buf.len()];
                    super::shuffle2x::from_shuffle2x(&s2x_buf, &mut normal);
                    RecoverySlice {
                        exponent: exponent_start + i as u32,
                        data: normal,
                    }
                })
                .collect(),
        };
        (slices, checksums)
    }
}

/// MD5 and length of a whole file, plus the MD5 of its first 16 KiB.
#[derive(Debug, Clone)]
pub struct FileHashes {
    pub md5_full: [u8; 16],
    pub md5_16k: [u8; 16],
    pub length: u64,
}

/// Computes [`FileHashes`] from a file's real bytes, fed incrementally.
pub struct FileHasher {
    full: Md5,
    head: Md5,
    head_consumed: usize,
    length: u64,
}

impl FileHasher {
    /// Start hashing a new file.
    pub fn new() -> Self {
        Self {
            full: Md5::new(),
            head: Md5::new(),
            head_consumed: 0,
            length: 0,
        }
    }

    /// Feed more of the file's real (unpadded) bytes.
    pub fn update(&mut self, data: &[u8]) {
        self.full.update(data);
        self.length += data.len() as u64;
        let room = HEAD_LEN - self.head_consumed;
        if room > 0 {
            let take = room.min(data.len());
            self.head.update(&data[..take]);
            self.head_consumed += take;
        }
    }

    /// Finish and return the hashes.
    pub fn finish(self) -> FileHashes {
        let mut md5_full = [0u8; 16];
        md5_full.copy_from_slice(&self.full.finalize());
        let mut md5_16k = [0u8; 16];
        md5_16k.copy_from_slice(&self.head.finalize());
        FileHashes {
            md5_full,
            md5_16k,
            length: self.length,
        }
    }
}

impl Default for FileHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// MD5 + CRC32 checksum of one zero-padded input slice (for the IFSC packet).
pub fn slice_checksum(padded_slice: &[u8]) -> SliceChecksum {
    SliceChecksum {
        md5: md5(padded_slice),
        crc32: crc32(padded_slice),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_exponent_zero_is_the_xor_of_all_inputs() {
        let a = [0x10u8, 0x20, 0x30, 0x40];
        let b = [0x01u8, 0x02, 0x03, 0x04];
        let mut encoder = RecoveryEncoder::new(4, 2, 0, 1);
        encoder.add_slice(a.to_vec());
        encoder.add_slice(b.to_vec());
        let (recovery, _) = encoder.finish();

        let expected: Vec<u8> = a.iter().zip(&b).map(|(x, y)| x ^ y).collect();
        assert_eq!(recovery[0].exponent, 0);
        assert_eq!(recovery[0].data, expected);
    }

    #[test]
    fn recovery_exponent_one_scales_a_single_input_by_its_base() {
        let gf = Gf16::new();
        let slice = [0x34u8, 0x12, 0x78, 0x56]; // words 0x1234, 0x5678
        let mut encoder = RecoveryEncoder::new(4, 1, 0, 2);
        encoder.add_slice(slice.to_vec());
        let (recovery, _) = encoder.finish();

        // base of input block 0 is 2; exponent 1 -> each word multiplied by 2.
        let w0 = gf.mul(0x1234, 2);
        let w1 = gf.mul(0x5678, 2);
        let mut expected = Vec::new();
        expected.extend_from_slice(&w0.to_le_bytes());
        expected.extend_from_slice(&w1.to_le_bytes());
        assert_eq!(recovery[1].data, expected);
    }

    // Slices of ≥ 16 bytes trigger the SIMD path (AVX2/SSSE3 on x86, NEON on
    // aarch64). This test compares SIMD output against the scalar reference to
    // ensure both produce bit-identical recovery data.
    #[test]
    fn simd_recovery_matches_scalar_for_larger_slices() {
        // 32-byte slices: blocks_16 = 2 (NEON), blocks_32 = 1 (AVX2) — exercises SIMD.
        let slice_size = 32;
        let total_slices = 3;
        let recovery_count = 4;

        // Build a deterministic non-trivial input.
        let slices: Vec<Vec<u8>> = (0..total_slices)
            .map(|s| {
                (0..slice_size)
                    .map(|i| ((s * 37 + i * 13 + 7) & 0xFF) as u8)
                    .collect()
            })
            .collect();

        // Run through the SIMD encoder.
        let mut enc = RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count);
        for s in &slices {
            enc.add_slice(s.clone());
        }
        let (simd_recovery, _) = enc.finish();

        // Build a scalar reference: temporarily patch out SIMD by calling
        // flush_scalar_work directly.
        let gf = Gf16::new();
        let logbases = input_logbases(total_slices);
        let mut scalar_buffers = vec![vec![0u16; slice_size / 2]; recovery_count];
        RecoveryEncoder::flush_scalar_work(&mut scalar_buffers, &slices, 0, &logbases, 0, &gf);
        let scalar_recovery: Vec<Vec<u8>> = scalar_buffers
            .into_iter()
            .map(|buf| buf.into_iter().flat_map(|w| w.to_le_bytes()).collect())
            .collect();

        for (i, (simd, scalar)) in simd_recovery.iter().zip(&scalar_recovery).enumerate() {
            assert_eq!(
                simd.data, *scalar,
                "SIMD and scalar disagree on recovery block {i}"
            );
        }
    }

    // Validates that flush_avx512_gfni produces bit-identical output to the
    // scalar reference.  Requires the `bench-internals` feature to force the
    // path; skips cleanly on CPUs without AVX-512/GFNI.
    //
    // Run with:
    //   cargo test --features bench-internals -- gfni_recovery_matches_scalar
    #[cfg(feature = "bench-internals")]
    #[test]
    fn gfni_recovery_matches_scalar() {
        if !std::is_x86_feature_detected!("avx512f")
            || !std::is_x86_feature_detected!("avx512bw")
            || !std::is_x86_feature_detected!("gfni")
        {
            eprintln!("gfni_recovery_matches_scalar: skipped (no GFNI on this CPU)");
            return;
        }

        // Use a slice size that exercises both the 64-byte SIMD blocks and the
        // scalar remainder path (not a multiple of 64).
        let slice_size = 96; // 64 + 32 — one full block + a remainder
        let total_slices = 5;
        let recovery_count = 6;

        let slices: Vec<Vec<u8>> = (0..total_slices)
            .map(|s| {
                (0..slice_size)
                    .map(|i| ((s * 53 + i * 17 + 3) & 0xFF) as u8)
                    .collect()
            })
            .collect();

        // GFNI path via forced dispatch.
        let mut enc = RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count)
            .with_forced_path(BenchPath::Avx512Gfni);
        for s in &slices {
            enc.add_slice(s.clone());
        }
        let (gfni_recovery, _) = enc.finish();

        // Scalar reference.
        let gf = Gf16::new();
        let logbases = input_logbases(total_slices);
        let mut scalar_buffers = vec![vec![0u16; slice_size / 2]; recovery_count];
        RecoveryEncoder::flush_scalar_work(&mut scalar_buffers, &slices, 0, &logbases, 0, &gf);
        let scalar_recovery: Vec<Vec<u8>> = scalar_buffers
            .into_iter()
            .map(|buf| buf.into_iter().flat_map(|w| w.to_le_bytes()).collect())
            .collect();

        for (i, (gfni, scalar)) in gfni_recovery.iter().zip(&scalar_recovery).enumerate() {
            assert_eq!(
                gfni.data, *scalar,
                "GFNI and scalar disagree on recovery block {i}"
            );
        }
    }

    #[test]
    fn file_hasher_16k_equals_full_for_small_files() {
        let mut hasher = FileHasher::new();
        hasher.update(b"hello ");
        hasher.update(b"world");
        let hashes = hasher.finish();
        assert_eq!(hashes.length, 11);
        assert_eq!(hashes.md5_full, md5(b"hello world"));
        assert_eq!(hashes.md5_16k, md5(b"hello world"));
    }

    #[test]
    fn file_hasher_16k_covers_only_the_first_16k() {
        let data = vec![0x5Au8; HEAD_LEN + 5000];
        let mut hasher = FileHasher::new();
        hasher.update(&data[..10_000]);
        hasher.update(&data[10_000..]);
        let hashes = hasher.finish();
        assert_eq!(hashes.length as usize, data.len());
        assert_eq!(hashes.md5_full, md5(&data));
        assert_eq!(hashes.md5_16k, md5(&data[..HEAD_LEN]));
    }

    #[test]
    fn slice_checksum_matches_md5_and_crc32() {
        let slice = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let checksum = slice_checksum(&slice);
        assert_eq!(checksum.md5, md5(&slice));
        assert_eq!(checksum.crc32, crc32(&slice));
    }

    #[test]
    #[cfg(target_arch = "x86_64")]
    fn dep_tables_correctness_and_timing() {
        use std::time::Instant;

        let t0 = Instant::now();
        let enc = RecoveryEncoder::new(4, 1, 0, 1);
        let elapsed = t0.elapsed();

        let Some(ref tables) = enc.dep_tables else {
            // GFNI hardware or non-AVX2: table is not allocated; skip.
            return;
        };

        // index 0 must be all-zero (multiply by 0 always yields 0).
        assert_eq!(tables[0], [0u16; 16]);

        // index 1 must be the identity (multiply by 1 is a no-op).
        let identity: [u16; 16] = std::array::from_fn(|k| 1 << k);
        assert_eq!(tables[1], identity);

        // Spot-check: table[n] must equal xor_dep_matrix(n) for representative n.
        for &n in &[2u16, 3, 7, 256, 1000, 0x1234, 0xABCD, 65534] {
            assert_eq!(
                tables[n as usize],
                xor_dep_matrix(n),
                "dep_tables mismatch at n={n}"
            );
        }

        // Release target: < 5 ms on i5-10400. Debug builds are much slower due
        // to the absence of optimizations; allow up to 5 s there.
        let limit_ms = if cfg!(debug_assertions) { 5_000 } else { 50 };
        assert!(
            elapsed.as_millis() < limit_ms,
            "dep_tables construction took {}ms, expected < {limit_ms}ms",
            elapsed.as_millis()
        );
    }

    #[test]
    fn new_altmap_produces_correct_recovery_data() {
        // Verify that new_altmap() produces byte-identical recovery data to new().
        // Only meaningful on AVX2 hardware; skip otherwise.
        #[cfg(target_arch = "x86_64")]
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }

        // slice_size must be a multiple of 32 bytes (16 u16 words) for ALTMAP.
        let slice_size = 64usize; // 32 u16 words
        let total_slices = 4;
        let recovery_count = 3;

        let slices: Vec<Vec<u8>> = (0..total_slices)
            .map(|s| {
                (0..slice_size)
                    .map(|i| ((s * 17 + i * 5 + 3) & 0xFF) as u8)
                    .collect()
            })
            .collect();

        // Normal encoder.
        let mut enc_normal = RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count);
        for s in &slices {
            enc_normal.add_slice(s.clone());
        }
        let (normal_recovery, _) = enc_normal.finish();

        // ALTMAP encoder (uses flush_avx2_altmap after Phase 27e).
        let mut enc_altmap =
            RecoveryEncoder::new_altmap(slice_size, total_slices, 0, recovery_count);
        for s in &slices {
            enc_altmap.add_slice(s.clone());
        }
        let (altmap_recovery, _) = enc_altmap.finish();

        assert_eq!(
            altmap_recovery.len(),
            normal_recovery.len(),
            "slice count mismatch"
        );
        for (i, (a, n)) in altmap_recovery
            .iter()
            .zip(normal_recovery.iter())
            .enumerate()
        {
            assert_eq!(
                a.data, n.data,
                "ALTMAP recovery slice {i} differs from normal encoder output"
            );
        }
    }

    #[test]
    fn altmap_buffer_size_matches_normal() {
        // ALTMAP buffers must have the same byte footprint as normal Vec<u16> buffers.
        for slice_words in [16, 32, 256, 1024, 384_000] {
            let normal_bytes = slice_words * 2;
            let altmap_bytes = altmap_buffer_size(slice_words);
            assert_eq!(
                altmap_bytes, normal_bytes,
                "size mismatch at slice_words={slice_words}"
            );
        }
    }

    #[test]
    fn new_shuffle2x_produces_correct_recovery_data() {
        // Verify that new_shuffle2x() produces byte-identical recovery data to new().
        // Only meaningful on AVX2 hardware; skip otherwise.
        #[cfg(target_arch = "x86_64")]
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }

        // slice_size must be a multiple of 32 bytes (16 u16 words) for Shuffle2x.
        let slice_size = 64usize;
        let total_slices = 5;
        let recovery_count = 4;

        let slices: Vec<Vec<u8>> = (0..total_slices)
            .map(|s| {
                (0..slice_size)
                    .map(|i| ((s * 19 + i * 7 + 11) & 0xFF) as u8)
                    .collect()
            })
            .collect();

        // Normal encoder.
        let mut enc_normal = RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count);
        for s in &slices {
            enc_normal.add_slice(s.clone());
        }
        let (normal_recovery, _) = enc_normal.finish();

        // Shuffle2x encoder (uses flush_avx2_shuffle2x after Phase 28b).
        let mut enc_s2x =
            RecoveryEncoder::new_shuffle2x(slice_size, total_slices, 0, recovery_count);
        for s in &slices {
            enc_s2x.add_slice(s.clone());
        }
        let (s2x_recovery, _) = enc_s2x.finish();

        assert_eq!(
            s2x_recovery.len(),
            normal_recovery.len(),
            "slice count mismatch"
        );
        for (i, (s2x, normal)) in s2x_recovery.iter().zip(normal_recovery.iter()).enumerate() {
            assert_eq!(
                s2x.data, normal.data,
                "Shuffle2x recovery slice {i} differs from normal encoder output"
            );
        }
    }

    #[test]
    fn new_shuffle2x_exponent_start_offset() {
        // Verify that exponent_start != 0 works correctly with Shuffle2x.
        #[cfg(target_arch = "x86_64")]
        if !std::is_x86_feature_detected!("avx2") {
            return;
        }

        let slice_size = 32usize;
        let total_slices = 3;
        let recovery_count = 2;
        let exponent_start = 5u32;

        let slices: Vec<Vec<u8>> = (0..total_slices)
            .map(|s| {
                (0..slice_size)
                    .map(|i| ((s * 11 + i * 3) & 0xFF) as u8)
                    .collect()
            })
            .collect();

        let mut enc_normal =
            RecoveryEncoder::new(slice_size, total_slices, exponent_start, recovery_count);
        for s in &slices {
            enc_normal.add_slice(s.clone());
        }
        let (normal_recovery, _) = enc_normal.finish();

        let mut enc_s2x = RecoveryEncoder::new_shuffle2x(
            slice_size,
            total_slices,
            exponent_start,
            recovery_count,
        );
        for s in &slices {
            enc_s2x.add_slice(s.clone());
        }
        let (s2x_recovery, _) = enc_s2x.finish();

        for (i, (s2x, normal)) in s2x_recovery.iter().zip(normal_recovery.iter()).enumerate() {
            assert_eq!(
                s2x.exponent, normal.exponent,
                "exponent mismatch at block {i}"
            );
            assert_eq!(
                s2x.data, normal.data,
                "Shuffle2x recovery slice {i} differs from normal encoder output (exponent_start={exponent_start})"
            );
        }
    }
}
