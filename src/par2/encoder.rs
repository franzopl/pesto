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
}

impl RecoveryEncoder {
    /// Create an encoder for `total_input_slices` input slices of `slice_size`
    /// bytes each, producing `recovery_count` recovery blocks (exponents
    /// `exponent_start..exponent_start + recovery_count`).
    ///
    /// # Panics
    ///
    /// Panics if `slice_size` is not a positive multiple of 4.
    pub fn new(slice_size: usize, total_input_slices: usize, exponent_start: u32, recovery_count: usize) -> Self {
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
        }
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
        if self.queued_slices.len() >= 64 {
            self.flush();
        }
    }

    /// Process queued slices into the recovery buffers.
    fn flush(&mut self) {
        if self.queued_slices.is_empty() {
            return;
        }

        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") {
            unsafe {
                self.flush_avx2();
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

        let mask_f = _mm256_set1_epi8(0x0F);
        let mask_even = _mm256_set1_epi16(0x00FF);

        self.buffers.par_iter_mut().enumerate().for_each(|(i, buffer)| {
            let exponent = self.exponent_start + i as u32;

            // Precompute AVX2 shuffle tables + scalar tables for tail
            let mut tables = Vec::with_capacity(queued.len());
            for (q_idx, _) in queued.iter().enumerate() {
                let logbase = self.logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = self.gf.exp(log_coeff);

                let mut tl_l = [0u8; 16];
                let mut tl_h = [0u8; 16];
                let mut th_l = [0u8; 16];
                let mut th_h = [0u8; 16];
                let mut hl_l = [0u8; 16];
                let mut hl_h = [0u8; 16];
                let mut hh_l = [0u8; 16];
                let mut hh_h = [0u8; 16];

                for val in 0..16 {
                    let r0 = self.gf.mul(val as u16, coeff);
                    tl_l[val as usize] = (r0 & 0xFF) as u8;
                    th_l[val as usize] = (r0 >> 8) as u8;

                    let r1 = self.gf.mul((val as u16) << 4, coeff);
                    tl_h[val as usize] = (r1 & 0xFF) as u8;
                    th_h[val as usize] = (r1 >> 8) as u8;

                    let r2 = self.gf.mul((val as u16) << 8, coeff);
                    hl_l[val as usize] = (r2 & 0xFF) as u8;
                    hh_l[val as usize] = (r2 >> 8) as u8;

                    let r3 = self.gf.mul((val as u16) << 12, coeff);
                    hl_h[val as usize] = (r3 & 0xFF) as u8;
                    hh_h[val as usize] = (r3 >> 8) as u8;
                }

                let v_tl_l = _mm256_broadcastsi128_si256(_mm_loadu_si128(tl_l.as_ptr() as *const __m128i));
                let v_tl_h = _mm256_broadcastsi128_si256(_mm_loadu_si128(tl_h.as_ptr() as *const __m128i));
                let v_th_l = _mm256_broadcastsi128_si256(_mm_loadu_si128(th_l.as_ptr() as *const __m128i));
                let v_th_h = _mm256_broadcastsi128_si256(_mm_loadu_si128(th_h.as_ptr() as *const __m128i));
                let v_hl_l = _mm256_broadcastsi128_si256(_mm_loadu_si128(hl_l.as_ptr() as *const __m128i));
                let v_hl_h = _mm256_broadcastsi128_si256(_mm_loadu_si128(hl_h.as_ptr() as *const __m128i));
                let v_hh_l = _mm256_broadcastsi128_si256(_mm_loadu_si128(hh_l.as_ptr() as *const __m128i));
                let v_hh_h = _mm256_broadcastsi128_si256(_mm_loadu_si128(hh_h.as_ptr() as *const __m128i));

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255 {
                    table_low[b as usize] = self.gf.mul(b as u16, coeff);
                    table_high[b as usize] = self.gf.mul((b as u16) << 8, coeff);
                }

                tables.push((v_tl_l, v_tl_h, v_th_l, v_th_h, v_hl_l, v_hl_h, v_hh_l, v_hh_h, table_low, table_high));
            }

            let chunk_size = 32768; // 64KB L1 blocking
            for (chunk_idx, buffer_chunk) in buffer.chunks_mut(chunk_size).enumerate() {
                let byte_offset = chunk_idx * chunk_size * 2;
                let byte_len = buffer_chunk.len() * 2;
                let blocks_32 = byte_len / 32;
                let remainder = byte_len % 32;

                for (q_idx, slice) in queued.iter().enumerate() {
                    let slice_chunk = &slice[byte_offset..byte_offset + byte_len];
                    let (v_tl_l, v_tl_h, v_th_l, v_th_h, v_hl_l, v_hl_h, v_hh_l, v_hh_h, ref table_low, ref table_high) = tables[q_idx];

                    let mut ptr_buf = buffer_chunk.as_mut_ptr() as *mut __m256i;
                    let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                    let end = ptr_in.add(blocks_32);

                    while ptr_in < end {
                        let input = _mm256_loadu_si256(ptr_in);

                        let n0_2 = _mm256_and_si256(input, mask_f);
                        let n1_3 = _mm256_and_si256(_mm256_srli_epi16(input, 4), mask_f);

                        let res_lo_even = _mm256_xor_si256(_mm256_shuffle_epi8(v_tl_l, n0_2), _mm256_shuffle_epi8(v_tl_h, n1_3));
                        let res_hi_even = _mm256_xor_si256(_mm256_shuffle_epi8(v_th_l, n0_2), _mm256_shuffle_epi8(v_th_h, n1_3));

                        let res_lo_odd = _mm256_xor_si256(_mm256_shuffle_epi8(v_hl_l, n0_2), _mm256_shuffle_epi8(v_hl_h, n1_3));
                        let res_hi_odd = _mm256_xor_si256(_mm256_shuffle_epi8(v_hh_l, n0_2), _mm256_shuffle_epi8(v_hh_h, n1_3));

                        let sum_lo_even = _mm256_xor_si256(res_lo_even, _mm256_srli_epi16(res_lo_odd, 8));
                        let sum_hi_even = _mm256_xor_si256(res_hi_even, _mm256_srli_epi16(res_hi_odd, 8));

                        let r_l_masked = _mm256_and_si256(sum_lo_even, mask_even);
                        let r_h_shifted = _mm256_slli_epi16(sum_hi_even, 8);

                        let out = _mm256_or_si256(r_l_masked, r_h_shifted);

                        let prev = _mm256_loadu_si256(ptr_buf);
                        _mm256_storeu_si256(ptr_buf, _mm256_xor_si256(prev, out));

                        ptr_in = ptr_in.add(1);
                        ptr_buf = ptr_buf.add(1);
                    }

                    if remainder > 0 {
                        let offset_words = blocks_32 * 16;
                        let mut ptr_word = buffer_chunk[offset_words..].as_mut_ptr();
                        let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
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

        self.queued_slices = queued;
        self.queued_slices.clear();
    }

    fn flush_scalar(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        self.buffers.par_iter_mut().enumerate().for_each(|(i, buffer)| {
            let exponent = self.exponent_start + i as u32;

            let mut tables = Vec::with_capacity(queued.len());
            for (q_idx, _) in queued.iter().enumerate() {
                let logbase = self.logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = self.gf.exp(log_coeff);

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255 {
                    table_low[b as usize] = self.gf.mul(b as u16, coeff);
                    table_high[b as usize] = self.gf.mul((b as u16) << 8, coeff);
                }
                tables.push((table_low, table_high));
            }

            let chunk_size = 32768;
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

        self.queued_slices = queued;
        self.queued_slices.clear();
    }

    /// Consume the encoder and return the finished recovery slices.
    pub fn finish(mut self) -> Vec<RecoverySlice> {
        self.flush();
        let exponent_start = self.exponent_start;
        self.buffers
            .into_iter()
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
            .collect()
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
        let recovery = encoder.finish();

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
        let recovery = encoder.finish();

        // base of input block 0 is 2; exponent 1 -> each word multiplied by 2.
        let w0 = gf.mul(0x1234, 2);
        let w1 = gf.mul(0x5678, 2);
        let mut expected = Vec::new();
        expected.extend_from_slice(&w0.to_le_bytes());
        expected.extend_from_slice(&w1.to_le_bytes());
        assert_eq!(recovery[1].data, expected);
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
