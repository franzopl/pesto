//! Affine (not Affine2x) layout: parpar `gf16_shuffle_prepare_block` / finish.
//!
//! Each 64-byte group (two AVX2 registers) is rearranged so lo-bytes and
//! hi-bytes live in separate vectors. The GFNI Affine kernel then applies
//! four 8×8 matrices (`ll`, `lh`, `hl`, `hh`) with `gf2p8affine`.

/// Byte size of one Affine buffer. Equal to `slice_words * 2`.
///
/// # Panics
///
/// Panics if `slice_words` is not a multiple of 32 (64 data bytes).
pub fn affine_buffer_size(slice_words: usize) -> usize {
    assert!(
        slice_words.is_multiple_of(32),
        "affine_buffer_size: slice_words ({slice_words}) must be a multiple of 32"
    );
    slice_words * 2
}

/// Normal u16 bytes → Affine shuffle-prepare layout.
///
/// # Panics
///
/// Panics if lengths differ or `src.len()` is not a multiple of 64.
pub fn to_affine(src: &[u8], dst: &mut [u8]) {
    assert_eq!(src.len(), dst.len());
    assert!(
        src.len().is_multiple_of(64),
        "to_affine: length {} must be a multiple of 64",
        src.len()
    );
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        unsafe { to_affine_avx2(src, dst) };
        return;
    }
    to_affine_scalar(src, dst);
}

/// Affine layout → normal u16 bytes.
pub fn from_affine(src: &[u8], dst: &mut [u8]) {
    assert_eq!(src.len(), dst.len());
    assert!(
        src.len().is_multiple_of(64),
        "from_affine: length {} must be a multiple of 64",
        src.len()
    );
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx2") {
        unsafe { from_affine_avx2(src, dst) };
        return;
    }
    from_affine_scalar(src, dst);
}

/// `vpshufb` separate_low_high on each 16-byte lane of a 32-byte chunk.
fn separate_low_high_avx2_lanes(chunk: &[u8]) -> [u8; 32] {
    let mut o = [0u8; 32];
    for lane in 0..2 {
        let base = lane * 16;
        for i in 0..8 {
            o[base + i] = chunk[base + i * 2];
            o[base + 8 + i] = chunk[base + i * 2 + 1];
        }
    }
    o
}

fn to_affine_scalar(src: &[u8], dst: &mut [u8]) {
    for (cin, cout) in src.chunks_exact(64).zip(dst.chunks_exact_mut(64)) {
        let a = separate_low_high_avx2_lanes(&cin[..32]);
        let b = separate_low_high_avx2_lanes(&cin[32..]);
        cout[..8].copy_from_slice(&a[8..16]);
        cout[8..16].copy_from_slice(&b[8..16]);
        cout[16..24].copy_from_slice(&a[24..32]);
        cout[24..32].copy_from_slice(&b[24..32]);
        cout[32..40].copy_from_slice(&a[0..8]);
        cout[40..48].copy_from_slice(&b[0..8]);
        cout[48..56].copy_from_slice(&a[16..24]);
        cout[56..64].copy_from_slice(&b[16..24]);
    }
}

fn from_affine_scalar(src: &[u8], dst: &mut [u8]) {
    for (cin, cout) in src.chunks_exact(64).zip(dst.chunks_exact_mut(64)) {
        let ta = &cin[..32];
        let tb = &cin[32..];
        // unpacklo_epi8(tb, ta) then unpackhi_epi8(tb, ta), per 16-byte lane.
        for lane in 0..2 {
            let base = lane * 16;
            for i in 0..8 {
                cout[lane * 32 + i * 2] = tb[base + i];
                cout[lane * 32 + i * 2 + 1] = ta[base + i];
                cout[lane * 32 + 16 + i * 2] = tb[base + 8 + i];
                cout[lane * 32 + 16 + i * 2 + 1] = ta[base + 8 + i];
            }
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn to_affine_avx2(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;
    let sep = _mm256_set_epi8(
        15, 13, 11, 9, 7, 5, 3, 1, 14, 12, 10, 8, 6, 4, 2, 0, 15, 13, 11, 9, 7, 5, 3, 1, 14, 12,
        10, 8, 6, 4, 2, 0,
    );
    for (cin, cout) in src.chunks_exact(64).zip(dst.chunks_exact_mut(64)) {
        let ta = _mm256_shuffle_epi8(_mm256_loadu_si256(cin.as_ptr().cast()), sep);
        let tb = _mm256_shuffle_epi8(_mm256_loadu_si256(cin.as_ptr().add(32).cast()), sep);
        _mm256_storeu_si256(cout.as_mut_ptr().cast(), _mm256_unpackhi_epi64(ta, tb));
        _mm256_storeu_si256(
            cout.as_mut_ptr().add(32).cast(),
            _mm256_unpacklo_epi64(ta, tb),
        );
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn from_affine_avx2(src: &[u8], dst: &mut [u8]) {
    use std::arch::x86_64::*;
    for (cin, cout) in src.chunks_exact(64).zip(dst.chunks_exact_mut(64)) {
        let ta = _mm256_loadu_si256(cin.as_ptr().cast());
        let tb = _mm256_loadu_si256(cin.as_ptr().add(32).cast());
        _mm256_storeu_si256(cout.as_mut_ptr().cast(), _mm256_unpacklo_epi8(tb, ta));
        _mm256_storeu_si256(
            cout.as_mut_ptr().add(32).cast(),
            _mm256_unpackhi_epi8(tb, ta),
        );
    }
}

/// Normal → Affine shuffle-prepare for AVX-512 (128-byte groups).
pub fn to_affine512(src: &[u8], dst: &mut [u8]) {
    assert_eq!(src.len(), dst.len());
    assert!(
        src.len().is_multiple_of(128),
        "to_affine512: length {} must be a multiple of 128",
        src.len()
    );
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512bw") {
        unsafe { to_affine512_avx512(src, dst) };
        return;
    }
    // Two AVX2 64-byte Affine groups is NOT the 512 layout; keep bytes as
    // sequential AVX2 prepares so a 256-bit kernel can still consume them.
    to_affine(src, dst);
}

/// Affine-512 layout → normal u16 bytes.
pub fn from_affine512(src: &[u8], dst: &mut [u8]) {
    assert_eq!(src.len(), dst.len());
    assert!(src.len().is_multiple_of(128));
    #[cfg(target_arch = "x86_64")]
    if std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512bw") {
        unsafe { from_affine512_avx512(src, dst) };
        return;
    }
    from_affine(src, dst);
}

/// Shuffle-prepare one 128-byte Affine512 block (parpar `gf16_shuffle_prepare_block`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw")]
pub(crate) unsafe fn affine512_prepare_block(src: *const u8, dst: *mut u8) {
    use std::arch::x86_64::*;
    let sep = _mm512_set_epi8(
        15, 13, 11, 9, 7, 5, 3, 1, 14, 12, 10, 8, 6, 4, 2, 0, 15, 13, 11, 9, 7, 5, 3, 1, 14, 12,
        10, 8, 6, 4, 2, 0, 15, 13, 11, 9, 7, 5, 3, 1, 14, 12, 10, 8, 6, 4, 2, 0, 15, 13, 11, 9, 7,
        5, 3, 1, 14, 12, 10, 8, 6, 4, 2, 0,
    );
    let ta = _mm512_shuffle_epi8(_mm512_loadu_si512(src.cast()), sep);
    let tb = _mm512_shuffle_epi8(_mm512_loadu_si512(src.add(64).cast()), sep);
    _mm512_storeu_si512(dst.cast(), _mm512_unpackhi_epi64(ta, tb));
    _mm512_storeu_si512(dst.add(64).cast(), _mm512_unpacklo_epi64(ta, tb));
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw")]
unsafe fn to_affine512_avx512(src: &[u8], dst: &mut [u8]) {
    for (cin, cout) in src.chunks_exact(128).zip(dst.chunks_exact_mut(128)) {
        affine512_prepare_block(cin.as_ptr(), cout.as_mut_ptr());
    }
}

/// Inverse of [`affine512_prepare_block`] (parpar `gf16_shuffle_finish_block`).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw")]
pub(crate) unsafe fn affine512_finish_block(src: *const u8, dst: *mut u8) {
    use std::arch::x86_64::*;
    let ta = _mm512_loadu_si512(src.cast());
    let tb = _mm512_loadu_si512(src.add(64).cast());
    _mm512_storeu_si512(dst.cast(), _mm512_unpacklo_epi8(tb, ta));
    _mm512_storeu_si512(dst.add(64).cast(), _mm512_unpackhi_epi8(tb, ta));
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw")]
unsafe fn from_affine512_avx512(src: &[u8], dst: &mut [u8]) {
    for (cin, cout) in src.chunks_exact(128).zip(dst.chunks_exact_mut(128)) {
        affine512_finish_block(cin.as_ptr(), cout.as_mut_ptr());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn roundtrip(bytes: &[u8]) {
        let mut a = vec![0u8; bytes.len()];
        let mut b = vec![0u8; bytes.len()];
        to_affine(bytes, &mut a);
        from_affine(&a, &mut b);
        assert_eq!(&b[..], bytes);
    }

    #[test]
    fn affine_roundtrip_incrementing() {
        let bytes: Vec<u8> = (0..256).map(|i| i as u8).collect();
        roundtrip(&bytes);
    }

    #[test]
    fn affine_scalar_matches_dispatch() {
        let bytes: Vec<u8> = (0..128).map(|i| (i * 3) as u8).collect();
        let mut s = vec![0u8; 128];
        let mut d = vec![0u8; 128];
        to_affine_scalar(&bytes, &mut s);
        to_affine(&bytes, &mut d);
        assert_eq!(s, d);
    }
}
