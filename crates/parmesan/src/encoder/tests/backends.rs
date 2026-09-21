use super::super::*;

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
#[cfg(target_arch = "x86_64")]
fn affine_nibble_scratch_matches_full_8x8() {
    // Same identity as parpar `gf16_affine_load_matrix`: XOR of 4 nibble
    // contributions equals the 8×8 matrix of the full coefficient.
    let gf = Gf16::new();
    let scratch = AffineNibbleScratch::new(&gf);
    for coeff in [0u16, 1, 2, 42, 0x00ff, 0xff00, 0x1234, 0x8000, 0xffff] {
        assert_eq!(
            scratch.load(coeff),
            gfni_affine_u64_mats(&gf, coeff),
            "nibble scratch mismatch for coeff={coeff:#06x}"
        );
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn affine512_pair_loader_matches_scalar_scratch() {
    if !affine512_kernel_available() {
        eprintln!("affine512_pair_loader_matches_scalar_scratch: skipped (no AVX-512+GFNI)");
        return;
    }

    #[target_feature(enable = "avx512f")]
    unsafe fn check() {
        let gf = Gf16::new();
        let scratch = AffineNibbleScratch::new(&gf);
        let pairs = [
            (0x0000, 0x0001),
            (0x0002, 0x002a),
            (0x00ff, 0xff00),
            (0x1234, 0x8000),
            (0xffff, 0xa55a),
        ];

        for (coeff_a, coeff_b) in pairs {
            let matrices = unsafe { affine512_load_pair(&scratch, coeff_a, coeff_b) };
            for (coeff, (m_ll, m_hl, m_lh, m_hh)) in
                [(coeff_a, matrices[0]), (coeff_b, matrices[1])]
            {
                let expected = scratch.load(coeff);
                let actual = [m_ll, m_lh, m_hl, m_hh];
                for (matrix, expected) in actual
                    .into_iter()
                    .zip([expected.0, expected.1, expected.2, expected.3])
                {
                    let lanes: [u64; 8] = unsafe { std::mem::transmute(matrix) };
                    assert_eq!(
                        lanes, [expected; 8],
                        "paired Affine512 matrix mismatch for coeff={coeff:#06x}"
                    );
                }
            }
        }
    }

    unsafe { check() };
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
#[cfg(target_arch = "x86_64")]
fn avx512_shuffle_matches_scalar() {
    if !shuffle512_kernel_available() {
        eprintln!("avx512_shuffle_matches_scalar: skipped (no AVX-512 BW)");
        return;
    }
    let slice_size = 128usize;
    let total_slices = 5usize;
    let recovery_count = 3usize;
    let slices: Vec<Vec<u8>> = (0..total_slices)
        .map(|s| {
            (0..slice_size)
                .map(|i| ((s * 41 + i * 19 + 3) & 0xFF) as u8)
                .collect()
        })
        .collect();
    let mut enc = RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count)
        .with_simd_path(crate::SimdPath::Avx512Shuffle);
    for s in &slices {
        enc.add_slice(s.clone());
    }
    let (got, _) = enc.finish();
    let gf = Gf16::new();
    let logbases = input_logbases(total_slices);
    let mut scalar_buffers = vec![vec![0u16; slice_size / 2]; recovery_count];
    RecoveryEncoder::flush_scalar_work(&mut scalar_buffers, &slices, 0, &logbases, 0, &gf);
    let scalar: Vec<Vec<u8>> = scalar_buffers
        .into_iter()
        .map(|buf| buf.into_iter().flat_map(|w| w.to_le_bytes()).collect())
        .collect();
    for (i, (g, s)) in got.iter().zip(&scalar).enumerate() {
        assert_eq!(
            g.data, *s,
            "AVX-512 shuffle disagrees on recovery block {i}"
        );
    }
}

// `simd_recovery_matches_scalar_for_larger_slices` above only exercises
// recovery_count=4, a clean multiple of the 4-wide unrolled group size in
// `flush_avx2_work`'s `buffers.par_chunks_mut(4)` — so it never touches the
// `[buf_a, buf_b]` (remainder 2) or `rest` (remainder 1 or 3) fallback arms.
// Investigating a flaky proptest failure (round_trip_reconstructs_arbitrary_missing_sets)
// whose 3 known failing inputs (recovery_count 3, 7, 11) all hit exactly
// those under-tested fallback arms — this sweeps every remainder case
// directly against the scalar reference to isolate whether the bug lives
// in AVX2 encoding itself (as opposed to the decoder or a timing issue).
#[test]
#[cfg(target_arch = "x86_64")]
fn avx2_recovery_matches_scalar_across_all_group_remainders() {
    if !std::is_x86_feature_detected!("avx2") {
        eprintln!("avx2_recovery_matches_scalar_across_all_group_remainders: skipped (no AVX2)");
        return;
    }
    let slice_size = 16;
    for total_slices in [1usize, 2, 3, 5] {
        for recovery_count in 1usize..=12 {
            let slices: Vec<Vec<u8>> = (0..total_slices)
                .map(|s| {
                    (0..slice_size)
                        .map(|i| ((s * 37 + i * 13 + 7) & 0xFF) as u8)
                        .collect()
                })
                .collect();

            let mut enc = RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count);
            for s in &slices {
                enc.add_slice(s.clone());
            }
            let (simd_recovery, _) = enc.finish();

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
                    "SIMD and scalar disagree on recovery block {i} \
                         (total_slices={total_slices}, recovery_count={recovery_count})"
                );
            }
        }
    }
}

// Validates that flush_avx512_gfni produces bit-identical output to the
// scalar reference.  Requires the `bench-internals` feature to force the
// path; skips cleanly on CPUs without AVX-512/GFNI.
//
// Run with:
//   cargo test --features bench-internals -- gfni_recovery_matches_scalar
#[cfg(all(feature = "bench-internals", target_arch = "x86_64"))]
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
