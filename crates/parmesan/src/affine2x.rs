//! Affine2x data layout for the AVX2+GFNI kernel (parity P1).
//!
//! ## Layout
//!
//! **Normal:** `[w0_lo, w0_hi, w1_lo, w1_hi, …]`.
//!
//! **Affine2x:** within each 16-byte lane, lo-bytes of the eight words occupy
//! the first 8 bytes and hi-bytes the second 8 — ParPar's
//! `separate_low_high` / `gf16_affine2x_prepare_block`, *without* the extra
//! `vpermq` that Shuffle2x applies:
//!
//! ```text
//! Normal  (32 B): w0lo w0hi w1lo w1hi … w15lo w15hi
//! Affine2x:       [w0lo…w7lo | w0hi…w7hi] [w8lo…w15lo | w8hi…w15hi]
//! ```
//!
//! The GFNI kernel then needs only two `gf2p8affine` plus a 64-bit lane swap
//! per source, and can keep recovery buffers in this layout until `finish`.

/// Byte size of one Affine2x buffer for `slice_words` u16 values.
///
/// # Panics
///
/// Panics if `slice_words` is not a multiple of 16.
pub fn affine2x_buffer_size(slice_words: usize) -> usize {
    assert!(
        slice_words.is_multiple_of(16),
        "affine2x_buffer_size: slice_words ({slice_words}) must be a multiple of 16"
    );
    slice_words * 2
}

/// Convert `src` (normal u16 bytes) into Affine2x layout.
///
/// # Panics
///
/// Panics if lengths differ or `src.len()` is not a multiple of 32.
pub fn to_affine2x(src: &[u8], dst: &mut [u8]) {
    assert_eq!(
        src.len(),
        dst.len(),
        "to_affine2x: src and dst must have equal length"
    );
    assert!(
        src.len().is_multiple_of(32),
        "to_affine2x: src length ({}) must be a multiple of 32",
        src.len()
    );

    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        unsafe { to_affine2x_avx2(src, dst) };
        return;
    }

    to_affine2x_scalar(src, dst);
}

/// Convert Affine2x bytes back to normal u16 layout.
///
/// # Panics
///
/// Panics if lengths differ or `src.len()` is not a multiple of 32.
pub fn from_affine2x(src: &[u8], dst: &mut [u8]) {
    assert_eq!(
        src.len(),
        dst.len(),
        "from_affine2x: src and dst must have equal length"
    );
    assert!(
        src.len().is_multiple_of(32),
        "from_affine2x: src length ({}) must be a multiple of 32",
        src.len()
    );

    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        unsafe { from_affine2x_avx2(src, dst) };
        return;
    }

    from_affine2x_scalar(src, dst);
}

fn to_affine2x_scalar(src: &[u8], dst: &mut [u8]) {
    for (chunk_in, chunk_out) in src.chunks_exact(32).zip(dst.chunks_exact_mut(32)) {
        for lane in 0..2 {
            let base = lane * 16;
            for i in 0..8 {
                chunk_out[base + i] = chunk_in[base + i * 2];
                chunk_out[base + 8 + i] = chunk_in[base + i * 2 + 1];
            }
        }
    }
}

fn from_affine2x_scalar(src: &[u8], dst: &mut [u8]) {
    for (chunk_in, chunk_out) in src.chunks_exact(32).zip(dst.chunks_exact_mut(32)) {
        for lane in 0..2 {
            let base = lane * 16;
            for i in 0..8 {
                chunk_out[base + i * 2] = chunk_in[base + i];
                chunk_out[base + i * 2 + 1] = chunk_in[base + 8 + i];
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn to_affine2x_avx2(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;
    let sep_mask = _mm256_set_epi8(
        15, 13, 11, 9, 7, 5, 3, 1, 14, 12, 10, 8, 6, 4, 2, 0, 15, 13, 11, 9, 7, 5, 3, 1, 14, 12,
        10, 8, 6, 4, 2, 0,
    );
    for (chunk_in, chunk_out) in src.chunks_exact(32).zip(dst.chunks_exact_mut(32)) {
        let v = _mm256_loadu_si256(chunk_in.as_ptr().cast());
        let separated = _mm256_shuffle_epi8(v, sep_mask);
        _mm256_storeu_si256(chunk_out.as_mut_ptr().cast(), separated);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn from_affine2x_avx2(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;
    let interleave_mask = _mm256_set_epi8(
        15, 7, 14, 6, 13, 5, 12, 4, 11, 3, 10, 2, 9, 1, 8, 0, 15, 7, 14, 6, 13, 5, 12, 4, 11, 3,
        10, 2, 9, 1, 8, 0,
    );
    for (chunk_in, chunk_out) in src.chunks_exact(32).zip(dst.chunks_exact_mut(32)) {
        let v = _mm256_loadu_si256(chunk_in.as_ptr().cast());
        let result = _mm256_shuffle_epi8(v, interleave_mask);
        _mm256_storeu_si256(chunk_out.as_mut_ptr().cast(), result);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(bytes: &[u8]) {
        assert!(bytes.len().is_multiple_of(32));
        let mut a2x = vec![0u8; bytes.len()];
        let mut recovered = vec![0u8; bytes.len()];
        to_affine2x(bytes, &mut a2x);
        from_affine2x(&a2x, &mut recovered);
        assert_eq!(recovered, bytes);
    }

    #[test]
    fn roundtrip_zeros() {
        roundtrip(&[0u8; 32]);
        roundtrip(&[0u8; 128]);
    }

    #[test]
    fn roundtrip_incrementing() {
        let bytes: Vec<u8> = (0..256u16).flat_map(|i| i.to_le_bytes()).collect();
        roundtrip(&bytes);
    }

    #[test]
    fn layout_correct_per_lane() {
        let bytes: Vec<u8> = (0u8..32).collect();
        let mut a2x = vec![0u8; 32];
        to_affine2x(&bytes, &mut a2x);
        for i in 0..8usize {
            assert_eq!(a2x[i], (i * 2) as u8);
            assert_eq!(a2x[8 + i], (i * 2 + 1) as u8);
            assert_eq!(a2x[16 + i], (16 + i * 2) as u8);
            assert_eq!(a2x[24 + i], (16 + i * 2 + 1) as u8);
        }
    }

    #[test]
    #[allow(unreachable_code)]
    fn scalar_and_simd_agree() {
        #[cfg(not(target_arch = "x86_64"))]
        return;
        let bytes: Vec<u8> = (0..256).map(|i| (i * 17) as u8).collect();
        let mut out_scalar = vec![0u8; 256];
        let mut out_simd = vec![0u8; 256];
        to_affine2x_scalar(&bytes, &mut out_scalar);
        to_affine2x(&bytes, &mut out_simd);
        assert_eq!(out_scalar, out_simd);
    }
}
