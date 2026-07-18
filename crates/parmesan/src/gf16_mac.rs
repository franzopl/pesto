//! GF(2¹⁶) multiply-accumulate primitive used by repair (decode).
//!
//! `mac(gf, dst, src, coeff)` computes `dst ^= coeff * src`, word by word.
//! `RecoveryEncoder`'s SIMD kernels (`crates/parmesan/src/encoder.rs`)
//! compute exactly this operation internally, and sharing that code with
//! decode was the original plan (see `ROADMAP.md` Phase 22d.2). That
//! extraction is still not done — it's the one place in the roadmap that
//! would touch existing production code — so the SIMD kernels here are a
//! fresh, independent implementation instead, using the same well-proven
//! nibble-lookup technique (`Ssse3Table`/`Avx2Table` in `encoder.rs`, in
//! turn adapted from ParPar's `gf16_shuffle` approach) but written from
//! scratch against `mac`'s single-coefficient signature rather than
//! extracted from the encoder's per-flush, many-coefficients-at-once loop.
//! Every SIMD path here is checked against the scalar reference
//! exhaustively (all 65536 coefficients) in this module's tests, the same
//! discipline `gf16.rs` uses for `xor_dep_matrix`.
//!
//! GFNI and NEON paths are not implemented yet (tracked as follow-up in
//! `ROADMAP.md` Phase 22e); `mac` falls back to AVX2, then SSSE3, then
//! scalar.

use crate::gf16::Gf16;

/// `dst ^= coeff * src`, interpreting both buffers as little-endian `u16`
/// words — the on-disk PAR2 slice layout. A no-op when `coeff == 0`.
/// Automatically uses the best available SIMD path at runtime.
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

    #[cfg(target_arch = "x86_64")]
    {
        if std::is_x86_feature_detected!("avx2") {
            unsafe { mac_avx2(gf, dst, src, coeff) };
            return;
        }
        if std::is_x86_feature_detected!("ssse3") {
            unsafe { mac_ssse3(gf, dst, src, coeff) };
            return;
        }
    }

    mac_scalar(gf, dst, src, coeff);
}

fn mac_scalar(gf: &Gf16, dst: &mut [u8], src: &[u8], coeff: u16) {
    for (d, s) in dst.chunks_exact_mut(2).zip(src.chunks_exact(2)) {
        let sv = u16::from_le_bytes([s[0], s[1]]);
        let dv = u16::from_le_bytes([d[0], d[1]]);
        let result = dv ^ gf.mul(sv, coeff);
        d.copy_from_slice(&result.to_le_bytes());
    }
}

/// Eight 16-entry nibble-contribution tables plus two 256-entry byte tables
/// for one GF(2¹⁶) coefficient — the same decomposition `encoder.rs` uses
/// (`Ssse3Table`): a 16-bit word is 4 nibbles (2 from each byte), and
/// GF(2¹⁶) multiplication is linear over XOR, so the product of the whole
/// word by `coeff` is the XOR of each nibble's independent contribution.
/// `tl_*`/`th_*` come from the low byte's two nibbles (weights 1 and 16);
/// `hl_*`/`hh_*` come from the high byte's two nibbles (weights 256 and
/// 4096); `_l`/`_h` suffixes select the low/high output byte.
struct NibbleTables {
    tl_l: [u8; 16],
    tl_h: [u8; 16],
    th_l: [u8; 16],
    th_h: [u8; 16],
    hl_l: [u8; 16],
    hl_h: [u8; 16],
    hh_l: [u8; 16],
    hh_h: [u8; 16],
    /// `byte_low[b] = gf.mul(b, coeff)` — full-word product of a byte in
    /// the low position, used for the scalar tail.
    byte_low: [u16; 256],
    /// `byte_high[b] = gf.mul(b << 8, coeff)`, used for the scalar tail.
    byte_high: [u16; 256],
}

impl NibbleTables {
    fn build(gf: &Gf16, coeff: u16) -> Self {
        let mut t = NibbleTables {
            tl_l: [0; 16],
            tl_h: [0; 16],
            th_l: [0; 16],
            th_h: [0; 16],
            hl_l: [0; 16],
            hl_h: [0; 16],
            hh_l: [0; 16],
            hh_h: [0; 16],
            byte_low: [0; 256],
            byte_high: [0; 256],
        };
        for val in 0..16usize {
            let r0 = gf.mul(val as u16, coeff);
            t.tl_l[val] = (r0 & 0xFF) as u8;
            t.th_l[val] = (r0 >> 8) as u8;
            let r1 = gf.mul((val as u16) << 4, coeff);
            t.tl_h[val] = (r1 & 0xFF) as u8;
            t.th_h[val] = (r1 >> 8) as u8;
            let r2 = gf.mul((val as u16) << 8, coeff);
            t.hl_l[val] = (r2 & 0xFF) as u8;
            t.hh_l[val] = (r2 >> 8) as u8;
            let r3 = gf.mul((val as u16) << 12, coeff);
            t.hl_h[val] = (r3 & 0xFF) as u8;
            t.hh_h[val] = (r3 >> 8) as u8;
        }
        for b in 0..=255usize {
            t.byte_low[b] = gf.mul(b as u16, coeff);
            t.byte_high[b] = gf.mul((b as u16) << 8, coeff);
        }
        t
    }

    /// Apply the scalar (byte-table) tail to `dst[off..]`/`src[off..]` —
    /// used for whatever whole words don't fit in a SIMD chunk.
    fn scalar_tail(&self, dst: &mut [u8], src: &[u8], off: usize) {
        let mut i = off;
        while i < dst.len() {
            let lo = src[i] as usize;
            let hi = src[i + 1] as usize;
            let word = self.byte_low[lo] ^ self.byte_high[hi];
            let dv = u16::from_le_bytes([dst[i], dst[i + 1]]);
            dst[i..i + 2].copy_from_slice(&(dv ^ word).to_le_bytes());
            i += 2;
        }
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn mac_ssse3(gf: &Gf16, dst: &mut [u8], src: &[u8], coeff: u16) {
    use std::arch::x86_64::*;

    let t = NibbleTables::build(gf, coeff);
    let v_tl_l = _mm_loadu_si128(t.tl_l.as_ptr() as *const __m128i);
    let v_tl_h = _mm_loadu_si128(t.tl_h.as_ptr() as *const __m128i);
    let v_th_l = _mm_loadu_si128(t.th_l.as_ptr() as *const __m128i);
    let v_th_h = _mm_loadu_si128(t.th_h.as_ptr() as *const __m128i);
    let v_hl_l = _mm_loadu_si128(t.hl_l.as_ptr() as *const __m128i);
    let v_hl_h = _mm_loadu_si128(t.hl_h.as_ptr() as *const __m128i);
    let v_hh_l = _mm_loadu_si128(t.hh_l.as_ptr() as *const __m128i);
    let v_hh_h = _mm_loadu_si128(t.hh_h.as_ptr() as *const __m128i);
    let mask_f = _mm_set1_epi8(0x0F_u8 as i8);
    let mask_even = _mm_set1_epi16(0x00FF_u16 as i16);

    let len = dst.len();
    let blocks_16 = len / 16;

    let mut ptr_in = src.as_ptr() as *const __m128i;
    let mut ptr_out = dst.as_mut_ptr() as *mut __m128i;
    let end = ptr_in.add(blocks_16);

    while ptr_in < end {
        let input = _mm_loadu_si128(ptr_in);
        let n0_2 = _mm_and_si128(input, mask_f);
        let n1_3 = _mm_and_si128(_mm_srli_epi16(input, 4), mask_f);

        let rle = _mm_xor_si128(
            _mm_shuffle_epi8(v_tl_l, n0_2),
            _mm_shuffle_epi8(v_tl_h, n1_3),
        );
        let rhe = _mm_xor_si128(
            _mm_shuffle_epi8(v_th_l, n0_2),
            _mm_shuffle_epi8(v_th_h, n1_3),
        );
        let rlo = _mm_xor_si128(
            _mm_shuffle_epi8(v_hl_l, n0_2),
            _mm_shuffle_epi8(v_hl_h, n1_3),
        );
        let rho = _mm_xor_si128(
            _mm_shuffle_epi8(v_hh_l, n0_2),
            _mm_shuffle_epi8(v_hh_h, n1_3),
        );

        let sle = _mm_xor_si128(rle, _mm_srli_epi16(rlo, 8));
        let she = _mm_xor_si128(rhe, _mm_srli_epi16(rho, 8));
        let out = _mm_or_si128(_mm_and_si128(sle, mask_even), _mm_slli_epi16(she, 8));

        let prev = _mm_loadu_si128(ptr_out);
        _mm_storeu_si128(ptr_out, _mm_xor_si128(prev, out));

        ptr_in = ptr_in.add(1);
        ptr_out = ptr_out.add(1);
    }

    t.scalar_tail(dst, src, blocks_16 * 16);
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn mac_avx2(gf: &Gf16, dst: &mut [u8], src: &[u8], coeff: u16) {
    use std::arch::x86_64::*;

    let t = NibbleTables::build(gf, coeff);
    // VPSHUFB operates within each 128-bit lane independently, so the
    // 16-entry table is broadcast into both lanes of the 256-bit register.
    let v_tl_l = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.tl_l.as_ptr() as *const __m128i));
    let v_tl_h = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.tl_h.as_ptr() as *const __m128i));
    let v_th_l = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.th_l.as_ptr() as *const __m128i));
    let v_th_h = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.th_h.as_ptr() as *const __m128i));
    let v_hl_l = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.hl_l.as_ptr() as *const __m128i));
    let v_hl_h = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.hl_h.as_ptr() as *const __m128i));
    let v_hh_l = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.hh_l.as_ptr() as *const __m128i));
    let v_hh_h = _mm256_broadcastsi128_si256(_mm_loadu_si128(t.hh_h.as_ptr() as *const __m128i));
    let mask_f = _mm256_set1_epi8(0x0F_u8 as i8);
    let mask_even = _mm256_set1_epi16(0x00FF_u16 as i16);

    let len = dst.len();
    let blocks_32 = len / 32;

    let mut ptr_in = src.as_ptr() as *const __m256i;
    let mut ptr_out = dst.as_mut_ptr() as *mut __m256i;
    let end = ptr_in.add(blocks_32);

    while ptr_in < end {
        let input = _mm256_loadu_si256(ptr_in);
        let n0_2 = _mm256_and_si256(input, mask_f);
        let n1_3 = _mm256_and_si256(_mm256_srli_epi16(input, 4), mask_f);

        let rle = _mm256_xor_si256(
            _mm256_shuffle_epi8(v_tl_l, n0_2),
            _mm256_shuffle_epi8(v_tl_h, n1_3),
        );
        let rhe = _mm256_xor_si256(
            _mm256_shuffle_epi8(v_th_l, n0_2),
            _mm256_shuffle_epi8(v_th_h, n1_3),
        );
        let rlo = _mm256_xor_si256(
            _mm256_shuffle_epi8(v_hl_l, n0_2),
            _mm256_shuffle_epi8(v_hl_h, n1_3),
        );
        let rho = _mm256_xor_si256(
            _mm256_shuffle_epi8(v_hh_l, n0_2),
            _mm256_shuffle_epi8(v_hh_h, n1_3),
        );

        let sle = _mm256_xor_si256(rle, _mm256_srli_epi16(rlo, 8));
        let she = _mm256_xor_si256(rhe, _mm256_srli_epi16(rho, 8));
        let out = _mm256_or_si256(_mm256_and_si256(sle, mask_even), _mm256_slli_epi16(she, 8));

        let prev = _mm256_loadu_si256(ptr_out);
        _mm256_storeu_si256(ptr_out, _mm256_xor_si256(prev, out));

        ptr_in = ptr_in.add(1);
        ptr_out = ptr_out.add(1);
    }

    t.scalar_tail(dst, src, blocks_32 * 32);
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

    /// Odd byte lengths (not SIMD-block-aligned) exercise the scalar tail
    /// path inside `mac_ssse3`/`mac_avx2`.
    #[test]
    fn works_for_lengths_that_are_not_simd_block_aligned() {
        let gf = Gf16::new();
        for words in [1usize, 3, 7, 9, 15, 17, 31, 33, 63, 65, 100] {
            let len = words * 2;
            let src: Vec<u8> = (0..len as u32).map(|i| (i * 37 + 11) as u8).collect();
            let mut dst = vec![0u8; len];
            mac(&gf, &mut dst, &src, 0x1234);

            let mut expected = vec![0u8; len];
            for i in 0..words {
                let w = u16::from_le_bytes([src[i * 2], src[i * 2 + 1]]);
                let product = gf.mul(w, 0x1234);
                expected[i * 2..i * 2 + 2].copy_from_slice(&product.to_le_bytes());
            }
            assert_eq!(dst, expected, "words={words}");
        }
    }

    /// Informal throughput comparison, not a correctness check — run with
    /// `cargo test -p parmesan-par2 --lib gf16_mac::tests::throughput -- \
    /// --ignored --nocapture`. A proper `criterion` benchmark is tracked
    /// separately in `ROADMAP.md` Phase 22h; this exists to have real
    /// numbers on hand while the SIMD kernels above were being added.
    #[test]
    #[ignore = "informational timing; run with --ignored --nocapture"]
    fn throughput_scalar_vs_dispatched() {
        let gf = Gf16::new();
        let words = 1_000_000usize; // 2 MB per call
        let src = vec![0xABu8; words * 2];
        let mut dst = vec![0u8; words * 2];
        let coeff = 0x1234u16;
        let iters = 100u32;

        let start = std::time::Instant::now();
        for _ in 0..iters {
            mac_scalar(&gf, &mut dst, &src, coeff);
        }
        let scalar_elapsed = start.elapsed();

        let start = std::time::Instant::now();
        for _ in 0..iters {
            mac(&gf, &mut dst, &src, coeff); // auto-dispatch: best available path
        }
        let dispatched_elapsed = start.elapsed();

        let bytes = (words * 2) as f64 * iters as f64;
        let scalar_mb_s = bytes / scalar_elapsed.as_secs_f64() / 1e6;
        let dispatched_mb_s = bytes / dispatched_elapsed.as_secs_f64() / 1e6;
        println!("scalar:     {scalar_mb_s:.1} MB/s");
        println!(
            "dispatched: {dispatched_mb_s:.1} MB/s ({:.2}x)",
            dispatched_mb_s / scalar_mb_s
        );
    }

    #[cfg(target_arch = "x86_64")]
    mod x86_simd {
        use super::*;

        /// All three implementations agree, for every one of the 65536
        /// possible GF(2¹⁶) coefficients, on a buffer that exercises SIMD
        /// main loops, their scalar tails, and sub-block-size inputs all at
        /// once (200 words: 6×32 + 8 leftover words for AVX2; 12×16 + 8 for
        /// SSSE3). This is the same exhaustive-coefficient discipline
        /// `gf16.rs` uses for `xor_dep_matrix`.
        #[test]
        fn scalar_ssse3_and_avx2_agree_for_every_coefficient() {
            if !std::is_x86_feature_detected!("ssse3") || !std::is_x86_feature_detected!("avx2") {
                eprintln!("skipping: SSSE3/AVX2 not available on this CPU");
                return;
            }
            let gf = Gf16::new();
            let words = 200usize;
            let src: Vec<u8> = (0..words as u32 * 2)
                .map(|i| (i.wrapping_mul(2654435761) >> 24) as u8)
                .collect();

            for coeff in 0u32..=65535 {
                let coeff = coeff as u16;

                let mut d_scalar = vec![0xFFu8; src.len()];
                mac_scalar(&gf, &mut d_scalar, &src, coeff);

                let mut d_ssse3 = vec![0xFFu8; src.len()];
                unsafe { mac_ssse3(&gf, &mut d_ssse3, &src, coeff) };

                let mut d_avx2 = vec![0xFFu8; src.len()];
                unsafe { mac_avx2(&gf, &mut d_avx2, &src, coeff) };

                assert_eq!(
                    d_ssse3, d_scalar,
                    "SSSE3 vs scalar mismatch at coeff={coeff:#06x}"
                );
                assert_eq!(
                    d_avx2, d_scalar,
                    "AVX2 vs scalar mismatch at coeff={coeff:#06x}"
                );
            }
        }

        /// Same agreement check but accumulating (non-zero starting `dst`,
        /// like real decode usage where multiple `mac` calls XOR into the
        /// same buffer) rather than starting from a fixed fill value.
        #[test]
        fn simd_paths_agree_when_accumulating_onto_existing_data() {
            if !std::is_x86_feature_detected!("ssse3") || !std::is_x86_feature_detected!("avx2") {
                eprintln!("skipping: SSSE3/AVX2 not available on this CPU");
                return;
            }
            let gf = Gf16::new();
            let words = 97usize; // deliberately not a multiple of 16 or 32
            let src: Vec<u8> = (0..words as u32 * 2).map(|i| (i * 91 + 3) as u8).collect();
            let seed: Vec<u8> = (0..words as u32 * 2).map(|i| (i * 53 + 17) as u8).collect();

            for &coeff in &[1u16, 2, 3, 0x1234, 0x8000, 0xFFFE, 0xFFFF] {
                let mut d_scalar = seed.clone();
                mac_scalar(&gf, &mut d_scalar, &src, coeff);
                let mut d_ssse3 = seed.clone();
                unsafe { mac_ssse3(&gf, &mut d_ssse3, &src, coeff) };
                let mut d_avx2 = seed.clone();
                unsafe { mac_avx2(&gf, &mut d_avx2, &src, coeff) };

                assert_eq!(d_ssse3, d_scalar, "coeff={coeff:#06x}");
                assert_eq!(d_avx2, d_scalar, "coeff={coeff:#06x}");
            }
        }
    }
}
