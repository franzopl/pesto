//! GF(2¹⁶) multiply-accumulate primitive used by repair (decode).
//!
//! This is a fresh, independent scalar implementation — **not** extracted
//! from [`crate::encoder`]. `RecoveryEncoder`'s SIMD kernels compute exactly
//! this same operation (`dst ^= coeff * src`, word by word) internally, and
//! sharing that code with decode was the original plan (see `ROADMAP.md`
//! Phase 22d.2/22e). That extraction is high-risk — it's the one place in
//! this roadmap that touches existing production code — so it's deliberately
//! decoupled from getting repair *correct* first. This module exists so
//! [`crate::decoder`] never has to touch `encoder.rs` to work at all; SIMD
//! acceleration for decode is tracked separately as pure follow-up
//! performance work.

use crate::gf16::Gf16;

/// `dst ^= coeff * src`, interpreting both buffers as little-endian `u16`
/// words — the on-disk PAR2 slice layout. A no-op when `coeff == 0`.
///
/// # Panics
///
/// Panics if `dst.len() != src.len()` or the shared length is odd (PAR2
/// slices are always a whole number of 16-bit words).
pub fn mac(gf: &Gf16, dst: &mut [u8], src: &[u8], coeff: u16) {
    assert_eq!(
        dst.len(),
        src.len(),
        "mac: dst ({}) and src ({}) must be the same length",
        dst.len(),
        src.len()
    );
    assert!(
        dst.len().is_multiple_of(2),
        "mac: buffer length ({}) must be a whole number of 16-bit words",
        dst.len()
    );
    if coeff == 0 {
        return;
    }
    for (d, s) in dst.chunks_exact_mut(2).zip(src.chunks_exact(2)) {
        let sv = u16::from_le_bytes([s[0], s[1]]);
        let dv = u16::from_le_bytes([d[0], d[1]]);
        let result = dv ^ gf.mul(sv, coeff);
        d.copy_from_slice(&result.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coeff_zero_is_a_no_op() {
        let gf = Gf16::new();
        let mut dst = vec![0xAAu8, 0xBB, 0xCC, 0xDD];
        let before = dst.clone();
        let src = vec![1u8, 2, 3, 4];
        mac(&gf, &mut dst, &src, 0);
        assert_eq!(dst, before);
    }

    #[test]
    fn coeff_one_xors_src_directly_into_dst() {
        let gf = Gf16::new();
        let src: Vec<u8> = (0..8u16).flat_map(|i| i.to_le_bytes()).collect();
        let mut dst = vec![0u8; src.len()];
        mac(&gf, &mut dst, &src, 1);
        assert_eq!(dst, src);
    }

    #[test]
    fn matches_manual_word_by_word_gf_multiplication() {
        let gf = Gf16::new();
        let words: Vec<u16> = vec![0, 1, 0xFFFF, 0x1234, 0xABCD, 12345];
        let src: Vec<u8> = words.iter().flat_map(|w| w.to_le_bytes()).collect();
        let mut dst = vec![0u8; src.len()];

        for &coeff in &[2u16, 3, 7, 0x8000, 0xFFFE] {
            let mut expected: Vec<u8> = vec![0u8; src.len()];
            for (i, &w) in words.iter().enumerate() {
                let product = gf.mul(w, coeff);
                expected[i * 2..i * 2 + 2].copy_from_slice(&product.to_le_bytes());
            }
            dst.fill(0);
            mac(&gf, &mut dst, &src, coeff);
            assert_eq!(dst, expected, "coeff={coeff:#06x}");
        }
    }

    #[test]
    fn accumulates_across_repeated_calls() {
        let gf = Gf16::new();
        let mut dst = vec![0u8; 4];
        let src_a = 5u16.to_le_bytes().to_vec();
        let src_b = 9u16.to_le_bytes().to_vec();
        // Pad to 4 bytes (2 words) — reuse the same 2-byte pattern twice.
        let src_a = [src_a.clone(), src_a].concat();
        let src_b = [src_b.clone(), src_b].concat();

        mac(&gf, &mut dst, &src_a, 3);
        mac(&gf, &mut dst, &src_b, 6);

        let expected_word = gf.mul(5, 3) ^ gf.mul(9, 6);
        let got_word = u16::from_le_bytes([dst[0], dst[1]]);
        assert_eq!(got_word, expected_word);
    }

    #[test]
    #[should_panic(expected = "same length")]
    fn panics_on_mismatched_lengths() {
        let gf = Gf16::new();
        let mut dst = vec![0u8; 4];
        let src = vec![0u8; 2];
        mac(&gf, &mut dst, &src, 1);
    }
}
