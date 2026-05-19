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

use super::gf16::{input_logbases, Gf16, ORDER};
use super::packet::{md5, SliceChecksum};
use crate::yenc::crc32;

/// Bytes covered by the per-file 16k hash.
const HEAD_LEN: usize = 16 * 1024;

/// Pre-computed AVX-512/GFNI coefficient table for one (recovery_block, input_slice) pair.
/// Two 512-bit matrix registers (mat_lo, mat_hi) plus 256-entry scalar lookup tables.
#[cfg(target_arch = "x86_64")]
type Avx512GfniTable = (__m512i, __m512i, [u16; 256], [u16; 256]);

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
    buffers: Vec<Vec<u16>>,
    /// Number of input slices fed so far.
    next_index: usize,
    /// Queue of input slices waiting to be processed (cache blocking).
    queued_slices: Vec<Vec<u8>>,
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
    Avx512Gfni,
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
            buffers: vec![vec![0u16; slice_size / 2]; recovery_count],
            next_index: 0,
            queued_slices: Vec::with_capacity(64),
            flush_limit_bytes: 256 * 1024 * 1024,
            compute_checksums: false,
            pending_checksums: Vec::new(),
            #[cfg(feature = "bench-internals")]
            forced_path: None,
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
                BenchPath::Avx512Gfni => unsafe {
                    self.flush_avx512_gfni();
                    return;
                },
            }
        }

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
            // Split borrows so both rayon::join arms are Send without capturing &mut self.
            let buffers = &mut self.buffers;
            let logbases = &self.logbases;
            let exponent_start = self.exponent_start;
            let gf = &self.gf;
            let ((), cs) = rayon::join(
                || unsafe {
                    Self::flush_avx2_work(
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
                Self::flush_avx2_work(
                    &mut self.buffers,
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
        self.queued_slices = queued;
        self.queued_slices.clear();
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
        // Each rayon task handles a PAIR of consecutive recovery blocks (2× unrolling
        // over the recovery dimension). One input load + one nibble decomposition serves
        // both blocks, halving the load and AND/SRL overhead per byte processed.
        let chunk_size = 16384usize; // 32 KiB recovery buffer chunk stays in L1

        buffers
            .par_chunks_mut(2)
            .enumerate()
            .for_each(|(pair_idx, buf_pair)| {
                let i = pair_idx * 2;
                match buf_pair {
                    [buf_a, buf_b] => {
                        // 2× unrolled: two recovery blocks share one input load.
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
                                        // One load; nibble decomposition amortised over both blocks.
                                        let input = _mm256_loadu_si256(ptr_in);
                                        let n0_2 = _mm256_and_si256(input, mask_f);
                                        let n1_3 =
                                            _mm256_and_si256(_mm256_srli_epi16(input, 4), mask_f);

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
                                        let sle_a =
                                            _mm256_xor_si256(rle_a, _mm256_srli_epi16(rlo_a, 8));
                                        let she_a =
                                            _mm256_xor_si256(rhe_a, _mm256_srli_epi16(rho_a, 8));
                                        let out_a = _mm256_or_si256(
                                            _mm256_and_si256(sle_a, mask_even),
                                            _mm256_slli_epi16(she_a, 8),
                                        );
                                        let prev_a = _mm256_loadu_si256(ptr_a);
                                        _mm256_storeu_si256(ptr_a, _mm256_xor_si256(prev_a, out_a));

                                        // Block B — reuses n0_2 and n1_3
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
                                        let sle_b =
                                            _mm256_xor_si256(rle_b, _mm256_srli_epi16(rlo_b, 8));
                                        let she_b =
                                            _mm256_xor_si256(rhe_b, _mm256_srli_epi16(rho_b, 8));
                                        let out_b = _mm256_or_si256(
                                            _mm256_and_si256(sle_b, mask_even),
                                            _mm256_slli_epi16(she_b, 8),
                                        );
                                        let prev_b = _mm256_loadu_si256(ptr_b);
                                        _mm256_storeu_si256(ptr_b, _mm256_xor_si256(prev_b, out_b));

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
                    [buf_a] => {
                        // Scalar-fallback for the last odd recovery block.
                        let base = i * n_queued;
                        buf_a.par_chunks_mut(chunk_size).enumerate().for_each(
                            |(chunk_idx, chunk_a)| unsafe {
                                let byte_offset = chunk_idx * chunk_size * 2;
                                let byte_len = chunk_a.len() * 2;
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

                                    let mut ptr_buf = chunk_a.as_mut_ptr() as *mut __m256i;
                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                    let end = ptr_in.add(blocks_32);

                                    while ptr_in < end {
                                        let input = _mm256_loadu_si256(ptr_in);
                                        let n0_2 = _mm256_and_si256(input, mask_f);
                                        let n1_3 =
                                            _mm256_and_si256(_mm256_srli_epi16(input, 4), mask_f);
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
                                        let sum_lo = _mm256_xor_si256(
                                            res_lo_even,
                                            _mm256_srli_epi16(res_lo_odd, 8),
                                        );
                                        let sum_hi = _mm256_xor_si256(
                                            res_hi_even,
                                            _mm256_srli_epi16(res_hi_odd, 8),
                                        );
                                        let out = _mm256_or_si256(
                                            _mm256_and_si256(sum_lo, mask_even),
                                            _mm256_slli_epi16(sum_hi, 8),
                                        );
                                        let prev = _mm256_loadu_si256(ptr_buf);
                                        _mm256_storeu_si256(ptr_buf, _mm256_xor_si256(prev, out));
                                        ptr_in = ptr_in.add(1);
                                        ptr_buf = ptr_buf.add(1);
                                    }

                                    if remainder > 0 {
                                        let ow = blocks_32 * 16;
                                        let mut pw = chunk_a[ow..].as_mut_ptr();
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
                    _ => {}
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
            let buffers = &mut self.buffers;
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
                    &mut self.buffers,
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
        self.queued_slices = queued;
        self.queued_slices.clear();
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
                // GFNI matrix encoding: row i lives at bits [i*8 + 7 : i*8]
                // (row 0 in the LSB byte, row 7 in the MSB byte).
                // M[i][j] = 1 iff input bit j affects output bit i.
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
                    m_ll |= (row_ll as u64) << (row * 8);
                    m_lh |= (row_lh as u64) << (row * 8);
                    m_hl |= (row_hl as u64) << (row * 8);
                    m_hh |= (row_hh as u64) << (row * 8);
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

        // 2D parallel loop: outer = recovery block pairs, inner = 32 KiB chunks of
        // each recovery buffer. Total rayon tasks = (n_rec/2) × (slice_words/16384),
        // saturating all available cores instead of the previous n_rec/2 ceiling.
        let chunk_size = 16384usize; // 32 KiB recovery buffer chunk stays in L1

        buffers
            .par_chunks_mut(2)
            .enumerate()
            .for_each(|(pair_idx, buf_pair)| {
                let i = pair_idx * 2;
                match buf_pair {
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
                                // 512-bit (64-byte) iterations; remainder handled by scalar path.
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
            let buffers = &mut self.buffers;
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
                    &mut self.buffers,
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
        self.queued_slices = queued;
        self.queued_slices.clear();
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
        let chunk_size = 16384usize; // 32 KiB recovery buffer chunk stays in L1
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
            let buffers = &mut self.buffers;
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
                    &mut self.buffers,
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
        self.queued_slices = queued;
        self.queued_slices.clear();
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
            let buffers = &mut self.buffers;
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
                &mut self.buffers,
                &queued,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
            Vec::new()
        };

        self.pending_checksums.extend(new_cs);
        self.queued_slices = queued;
        self.queued_slices.clear();
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
        let slices = self
            .buffers
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
            .collect();
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
}
