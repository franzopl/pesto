//! Shuffle2x data layout for the AVX2 nibble-shuffle kernel (Phase 28).
//!
//! ## Layout
//!
//! **Normal layout:** N consecutive `u16` words, each stored as 2 little-endian
//! bytes: `[w0_lo, w0_hi, w1_lo, w1_hi, ..., w(N-1)_lo, w(N-1)_hi]`.
//!
//! **Shuffle2x layout:** bytes are rearranged so that within each 32-byte
//! chunk (= one AVX2 register), all 16 lo-bytes occupy the first 128-bit lane
//! and all 16 hi-bytes occupy the second 128-bit lane:
//!
//! ```text
//! Normal  (32 bytes): w0_lo w0_hi  w1_lo w1_hi  … w15_lo w15_hi
//! Shuffle2x (32 B):  w0_lo w1_lo … w15_lo | w0_hi w1_hi … w15_hi
//! ```
//!
//! This is implemented as two AVX2 instructions per 32-byte chunk:
//!  1. `vpshufb` with mask `[0,2,4,6,8,10,12,14, 1,3,5,7,9,11,13,15]` repeated
//!     in each 128-bit lane → separates lo/hi bytes within each lane.
//!  2. `vpermq [3,1,2,0]` → moves all lo-bytes into lane 0, hi-bytes into lane 1.
//!
//! Total size is identical to normal layout: `N * 2` bytes.
//!
//! ## Why it speeds up the AVX2 kernel
//!
//! With lo-bytes in lane 0 and hi-bytes in lane 1, a single `vpshufb` can
//! look up nibble contributions for both bytes of a word simultaneously. The
//! `vperm2i128` instruction handles the cross-lane contribution (lo-byte nibbles
//! affecting the hi-byte output) without requiring separate pack/unpack operations,
//! reducing per-block instructions from ~21 to ~14 (33% fewer).
//!
//! ## Alignment requirement
//!
//! `N` must be a multiple of 16 (one 32-byte AVX2 processing group = 16 words).

/// Returns the byte size of one Shuffle2x buffer for `slice_words` u16 values.
/// Equal to `slice_words * 2` (same footprint as the normal layout).
///
/// # Panics
///
/// Panics if `slice_words` is not a multiple of 16.
pub fn shuffle2x_buffer_size(slice_words: usize) -> usize {
    assert!(
        slice_words.is_multiple_of(16),
        "shuffle2x_buffer_size: slice_words ({slice_words}) must be a multiple of 16"
    );
    slice_words * 2
}

/// Convert `src` (a raw slice of `slice_words * 2` bytes in normal u16 layout)
/// into Shuffle2x layout, writing into `dst`.
///
/// Uses the AVX2 path on x86_64 when available; falls back to scalar otherwise.
///
/// # Panics
///
/// Panics if `src.len()` is not a multiple of 32 or `dst.len() != src.len()`.
pub fn to_shuffle2x(src: &[u8], dst: &mut [u8]) {
    assert_eq!(
        src.len(),
        dst.len(),
        "to_shuffle2x: src and dst must have equal length"
    );
    assert!(
        src.len().is_multiple_of(32),
        "to_shuffle2x: src length ({}) must be a multiple of 32",
        src.len()
    );

    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        unsafe { to_shuffle2x_avx2(src, dst) };
        return;
    }

    to_shuffle2x_scalar(src, dst);
}

/// Convert `src` (Shuffle2x layout, `slice_words * 2` bytes) back to normal
/// u16 layout, writing into `dst`.
///
/// Uses the AVX2 path on x86_64 when available; falls back to scalar otherwise.
///
/// # Panics
///
/// Panics if `src.len()` is not a multiple of 32 or `dst.len() != src.len()`.
pub fn from_shuffle2x(src: &[u8], dst: &mut [u8]) {
    assert_eq!(
        src.len(),
        dst.len(),
        "from_shuffle2x: src and dst must have equal length"
    );
    assert!(
        src.len().is_multiple_of(32),
        "from_shuffle2x: src length ({}) must be a multiple of 32",
        src.len()
    );

    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        unsafe { from_shuffle2x_avx2(src, dst) };
        return;
    }

    from_shuffle2x_scalar(src, dst);
}

// ---------------------------------------------------------------------------
// Scalar implementations
// ---------------------------------------------------------------------------

fn to_shuffle2x_scalar(src: &[u8], dst: &mut [u8]) {
    // Each 32-byte chunk: reorder [lo0,hi0,lo1,hi1,...,lo15,hi15]
    //   → [lo0,lo1,...,lo15, hi0,hi1,...,hi15]
    for (chunk_in, chunk_out) in src.chunks_exact(32).zip(dst.chunks_exact_mut(32)) {
        for i in 0..16 {
            chunk_out[i] = chunk_in[i * 2]; // lo byte of word i
            chunk_out[16 + i] = chunk_in[i * 2 + 1]; // hi byte of word i
        }
    }
}

fn from_shuffle2x_scalar(src: &[u8], dst: &mut [u8]) {
    // Each 32-byte chunk: [lo0,...,lo15, hi0,...,hi15]
    //   → [lo0,hi0, lo1,hi1, ..., lo15,hi15]
    for (chunk_in, chunk_out) in src.chunks_exact(32).zip(dst.chunks_exact_mut(32)) {
        for i in 0..16 {
            chunk_out[i * 2] = chunk_in[i]; // lo byte of word i
            chunk_out[i * 2 + 1] = chunk_in[16 + i]; // hi byte of word i
        }
    }
}

// ---------------------------------------------------------------------------
// AVX2 implementations
// ---------------------------------------------------------------------------
//
// `to_shuffle2x_avx2`: for each 32-byte group (16 u16 words):
//   1. `vpshufb` with byte-interleave mask separates lo/hi bytes within each
//      128-bit lane:
//        lane0: [w0_lo,w1_lo,...,w7_lo, w0_hi,w1_hi,...,w7_hi]
//        lane1: [w8_lo,...,w15_lo, w8_hi,...,w15_hi]
//   2. `vpermq [3,1,2,0]` moves:
//        lane0 ← words 0-7 lo-bytes  (old lane0 low half)
//        lane1 ← words 0-7 hi-bytes  (old lane0 high half)
//      wait — that puts words 0-7 in lane0 and words 8-15 in ... no.
//      Actually: `vpermq [3,1,2,0]` on indices:
//        dst[0] = src[0]  (lane0 low  qword → keep: lo-bytes of words 0-7)
//        dst[1] = src[2]  (lane1 low  qword → lo-bytes of words 8-15)
//        dst[2] = src[1]  (lane0 high qword → hi-bytes of words 0-7)
//        dst[3] = src[3]  (lane1 high qword → hi-bytes of words 8-15)
//      Result: [lo0..lo7 | lo8..lo15 | hi0..hi7 | hi8..hi15]
//      Which equals: lane0 = [lo0..lo15], lane1 = [hi0..hi15]. ✓
//
// `from_shuffle2x_avx2`: inverse — `vpermq [2,0,3,1]` then `vpshufb` with
//   the interleave/unpack mask.

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn to_shuffle2x_avx2(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;

    // Byte-separation mask for each 128-bit lane:
    // picks bytes at positions [0,2,4,6,8,10,12,14, 1,3,5,7,9,11,13,15]
    // i.e. all even bytes (lo bytes) first, then all odd bytes (hi bytes).
    let sep_mask = _mm256_set_epi8(
        15, 13, 11, 9, 7, 5, 3, 1, 14, 12, 10, 8, 6, 4, 2, 0, // lane 1
        15, 13, 11, 9, 7, 5, 3, 1, 14, 12, 10, 8, 6, 4, 2, 0, // lane 0
    );

    for (chunk_in, chunk_out) in src.chunks_exact(32).zip(dst.chunks_exact_mut(32)) {
        let v = _mm256_loadu_si256(chunk_in.as_ptr() as *const __m256i);
        // Step 1: separate lo/hi bytes within each 128-bit lane
        let separated = _mm256_shuffle_epi8(v, sep_mask);
        // Step 2: move all lo-bytes to lane0, all hi-bytes to lane1
        // vpermq [3,1,2,0] = imm8 = 0b_11_01_10_00 = 0xD8... wait:
        // _MM_SHUFFLE(z,y,x,w) = (z<<6)|(y<<4)|(x<<2)|w
        // We want: out[0]=src[0], out[1]=src[2], out[2]=src[1], out[3]=src[3]
        // i.e. imm = (3<<6)|(1<<4)|(2<<2)|0 = 192+16+8+0 = 216 = 0xD8
        let result = _mm256_permute4x64_epi64(separated, 0xD8);
        _mm256_storeu_si256(chunk_out.as_mut_ptr() as *mut __m256i, result);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn from_shuffle2x_avx2(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;

    // Interleave mask: for each 128-bit lane takes the lo-byte group (bytes 0-7)
    // and hi-byte group (bytes 8-15) and interleaves them:
    // [b0,b8, b1,b9, b2,b10, b3,b11, b4,b12, b5,b13, b6,b14, b7,b15]
    let interleave_mask = _mm256_set_epi8(
        15, 7, 14, 6, 13, 5, 12, 4, 11, 3, 10, 2, 9, 1, 8, 0, // lane 1
        15, 7, 14, 6, 13, 5, 12, 4, 11, 3, 10, 2, 9, 1, 8, 0, // lane 0
    );

    for (chunk_in, chunk_out) in src.chunks_exact(32).zip(dst.chunks_exact_mut(32)) {
        let v = _mm256_loadu_si256(chunk_in.as_ptr() as *const __m256i);
        // Step 1: undo the permq — inverse of 0xD8 is 0x72
        // Original permq: out[0]=src[0], out[1]=src[2], out[2]=src[1], out[3]=src[3]
        // Inverse: out[0]=src[0], out[1]=src[2], out[2]=src[1], out[3]=src[3]
        // Wait — 0xD8 maps [0,2,1,3] → inverse maps [0,2,1,3] again (self-inverse)? No.
        // 0xD8 = (3<<6)|(1<<4)|(2<<2)|0: dst[0]←src[0], dst[1]←src[2], dst[2]←src[1], dst[3]←src[3]
        // Inverse: dst[0]←src[0], dst[1]←src[2], dst[2]←src[1], dst[3]←src[3] = same 0xD8
        // Because it swaps indices 1 and 2, which is self-inverse.
        let unpermed = _mm256_permute4x64_epi64(v, 0xD8);
        // Step 2: interleave lo/hi bytes back to normal u16 layout
        let result = _mm256_shuffle_epi8(unpermed, interleave_mask);
        _mm256_storeu_si256(chunk_out.as_mut_ptr() as *mut __m256i, result);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(bytes: &[u8]) {
        assert!(bytes.len().is_multiple_of(32));
        let mut s2x = vec![0u8; bytes.len()];
        let mut recovered = vec![0u8; bytes.len()];
        to_shuffle2x(bytes, &mut s2x);
        from_shuffle2x(&s2x, &mut recovered);
        assert_eq!(
            recovered,
            bytes,
            "round-trip failed for {} bytes",
            bytes.len()
        );
    }

    #[test]
    fn roundtrip_zeros() {
        roundtrip(&[0u8; 32]);
        roundtrip(&[0u8; 128]);
    }

    #[test]
    fn roundtrip_all_ones() {
        roundtrip(&[0xFFu8; 32]);
        roundtrip(&[0xFFu8; 256]);
    }

    #[test]
    fn roundtrip_incrementing() {
        let bytes: Vec<u8> = (0..256u16).flat_map(|i| i.to_le_bytes()).collect();
        roundtrip(&bytes);
    }

    #[test]
    fn roundtrip_random() {
        let mut lcg: u64 = 0xDEAD_BEEF_1234_5678;
        let bytes: Vec<u8> = (0..512)
            .map(|_| {
                lcg = lcg
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (lcg >> 56) as u8
            })
            .collect();
        roundtrip(&bytes);
    }

    #[test]
    fn layout_correct_lo_hi_separation() {
        // 32 bytes = 16 u16 words; word i = (i, i+16) as (lo, hi)
        let bytes: Vec<u8> = (0u8..32).collect();
        let mut s2x = vec![0u8; 32];
        to_shuffle2x(&bytes, &mut s2x);

        // In Shuffle2x: lane0 = lo bytes of words 0-15 = [0,2,4,6,8,10,12,14,16,18,20,22,24,26,28,30]
        //               lane1 = hi bytes of words 0-15 = [1,3,5,7,9,11,13,15,17,19,21,23,25,27,29,31]
        for i in 0..16usize {
            assert_eq!(
                s2x[i],
                (i * 2) as u8,
                "lane0[{i}] should be lo byte of word {i}"
            );
            assert_eq!(
                s2x[16 + i],
                (i * 2 + 1) as u8,
                "lane1[{i}] should be hi byte of word {i}"
            );
        }
    }

    #[test]
    #[allow(unreachable_code)]
    fn scalar_and_simd_agree() {
        #[cfg(not(target_arch = "x86_64"))]
        return;

        let mut lcg: u64 = 0xCAFE_BABE_1234;
        let bytes: Vec<u8> = (0..256)
            .map(|_| {
                lcg = lcg
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                (lcg >> 56) as u8
            })
            .collect();

        let mut out_scalar = vec![0u8; 256];
        let mut out_simd = vec![0u8; 256];
        to_shuffle2x_scalar(&bytes, &mut out_scalar);
        to_shuffle2x(&bytes, &mut out_simd);
        assert_eq!(out_scalar, out_simd, "to_shuffle2x: scalar vs SIMD differ");

        let mut inv_scalar = vec![0u8; 256];
        let mut inv_simd = vec![0u8; 256];
        from_shuffle2x_scalar(&out_scalar, &mut inv_scalar);
        from_shuffle2x(&out_simd, &mut inv_simd);
        assert_eq!(
            inv_scalar, inv_simd,
            "from_shuffle2x: scalar vs SIMD differ"
        );
    }
}
