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

#[test]
fn file_hasher_16k_equals_full_for_small_files() {
    let mut hasher = FileHasher::new();
    hasher.update(b"hello ");
    hasher.update(b"world");
    let hashes = hasher.finish();
    assert_eq!(hashes.length, 11);
    assert_eq!(hashes.md5_full, crate::packet::md5(b"hello world"));
    assert_eq!(hashes.md5_16k, crate::packet::md5(b"hello world"));
}

#[test]
fn file_hasher_16k_covers_only_the_first_16k() {
    let data = vec![0x5Au8; HEAD_LEN + 5000];
    let mut hasher = FileHasher::new();
    hasher.update(&data[..10_000]);
    hasher.update(&data[10_000..]);
    let hashes = hasher.finish();
    assert_eq!(hashes.length as usize, data.len());
    assert_eq!(hashes.md5_full, crate::packet::md5(&data));
    assert_eq!(hashes.md5_16k, crate::packet::md5(&data[..HEAD_LEN]));
}

#[test]
fn slice_checksum_matches_md5_and_crc32() {
    let slice = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let checksum = slice_checksum(&slice);
    assert_eq!(checksum.md5, crate::packet::md5(&slice));
    assert_eq!(checksum.crc32, crate::yenc::crc32(&slice));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn batched_slice_checksums_match_individual_checksums() {
    let slices: Vec<Vec<u8>> = (0..17)
        .map(|i| {
            (0..4096)
                .map(|offset| (offset as u8).wrapping_mul(31).wrapping_add(i))
                .collect()
        })
        .collect();
    let expected: Vec<_> = slices.iter().map(|slice| slice_checksum(slice)).collect();

    let actual = slice_checksums_batch(&slices);
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(actual.md5, expected.md5);
        assert_eq!(actual.crc32, expected.crc32);
    }
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
#[cfg(target_arch = "x86_64")]
fn new_altmap_produces_correct_recovery_data() {
    // Verify that new_altmap() produces byte-identical recovery data to new().
    // Runs on every CPU: where the ALTMAP kernel exists this exercises it,
    // and where it doesn't (no AVX2, or a GFNI machine, where
    // build_dep_tables returns None) it exercises the constructor's
    // fallback to the portable layout. This test used to skip GFNI
    // hardware, which is exactly where the encoder was silently returning
    // all-zero recovery blocks.

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
    let mut enc_altmap = RecoveryEncoder::new_altmap(slice_size, total_slices, 0, recovery_count);
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
        assert!(
            a.data.iter().any(|b| *b != 0),
            "ALTMAP recovery slice {i} is all zeros — the kernel never ran"
        );
    }
}

/// Every layout-specific constructor must produce the same recovery data as
/// the portable one, on whatever CPU the tests happen to run on. A layout
/// whose kernel is unavailable has to fall back, not silently return an
/// unprocessed (all-zero) buffer.
#[test]
fn layout_constructors_agree_with_the_portable_encoder() {
    let (slice_size, total_slices, recovery_count) = (512usize, 5usize, 3usize);
    let slices: Vec<Vec<u8>> = (0..total_slices)
        .map(|s| {
            (0..slice_size)
                .map(|i| ((s * 131 + i * 7 + 11) & 0xFF) as u8)
                .collect()
        })
        .collect();

    let encode = |mut enc: RecoveryEncoder| {
        for s in &slices {
            enc.add_slice(s.clone());
        }
        enc.finish().0
    };

    let reference = encode(RecoveryEncoder::new(
        slice_size,
        total_slices,
        0,
        recovery_count,
    ));
    assert!(
        reference.iter().any(|r| r.data.iter().any(|b| *b != 0)),
        "reference encoder produced nothing to compare against"
    );

    for (name, built) in [
        (
            "altmap",
            encode(RecoveryEncoder::new_altmap(
                slice_size,
                total_slices,
                0,
                recovery_count,
            )),
        ),
        (
            "shuffle2x",
            encode(RecoveryEncoder::new_shuffle2x(
                slice_size,
                total_slices,
                0,
                recovery_count,
            )),
        ),
        (
            "affine2x",
            encode(RecoveryEncoder::new_affine2x(
                slice_size,
                total_slices,
                0,
                recovery_count,
            )),
        ),
        (
            "affine",
            encode(RecoveryEncoder::new_affine(
                slice_size,
                total_slices,
                0,
                recovery_count,
            )),
        ),
        (
            "affine512",
            encode(RecoveryEncoder::new_affine512(
                slice_size,
                total_slices,
                0,
                recovery_count,
            )),
        ),
        (
            "smart",
            encode(RecoveryEncoder::new_smart(
                slice_size,
                total_slices,
                0,
                recovery_count,
            )),
        ),
    ] {
        assert_eq!(built.len(), reference.len(), "{name}: block count mismatch");
        for (i, (got, want)) in built.iter().zip(reference.iter()).enumerate() {
            assert_eq!(
                got.data, want.data,
                "{name}: recovery block {i} differs from the portable encoder"
            );
        }
    }
}

/// `altmap_kernel_available` / `shuffle2x_kernel_available` exist so a
/// benchmark can tell whether measuring `new_altmap`/`new_shuffle2x` on this
/// machine measures the specialized kernel or the portable fallback. They
/// duplicate the constructors' feature checks, so pin them to the layout the
/// constructors actually pick — a drift here makes a bench row silently
/// mislabel which kernel produced its number.
#[test]
fn kernel_availability_predicates_match_the_layout_constructors() {
    let (slice_size, total_slices, recovery_count) = (512usize, 3usize, 2usize);

    let altmap = RecoveryEncoder::new_altmap(slice_size, total_slices, 0, recovery_count);
    assert_eq!(
        matches!(altmap.buffers, RecoveryBufferSet::Altmap(_)),
        altmap_kernel_available(),
        "altmap_kernel_available disagrees with the layout new_altmap chose"
    );

    let shuffle2x = RecoveryEncoder::new_shuffle2x(slice_size, total_slices, 0, recovery_count);
    assert_eq!(
        matches!(shuffle2x.buffers, RecoveryBufferSet::Shuffle2x(_)),
        shuffle2x_kernel_available(),
        "shuffle2x_kernel_available disagrees with the layout new_shuffle2x chose"
    );

    let affine2x = RecoveryEncoder::new_affine2x(slice_size, total_slices, 0, recovery_count);
    assert_eq!(
        matches!(affine2x.buffers, RecoveryBufferSet::Affine2x(_)),
        affine2x_kernel_available(),
        "affine2x_kernel_available disagrees with the layout new_affine2x chose"
    );

    // Auto path must not pick Affine2x: c7i movie create 220 vs ~298
    // Normal+GFNI. Explicit `new_affine2x` remains for experiments.
    let smart = RecoveryEncoder::new_smart(slice_size, total_slices, 0, recovery_count);
    assert!(
        !matches!(smart.buffers, RecoveryBufferSet::Affine2x(_)),
        "try_new_smart must not select Affine2x"
    );

    let affine = RecoveryEncoder::new_affine(slice_size, total_slices, 0, recovery_count);
    assert_eq!(
        matches!(affine.buffers, RecoveryBufferSet::Affine(_)),
        affine_kernel_available(),
        "affine_kernel_available disagrees with new_affine"
    );
    if affine512_kernel_available() {
        assert!(
            matches!(smart.buffers, RecoveryBufferSet::Affine2x(_)),
            "try_new_smart must pick packed Affine2x on AVX-512+GFNI"
        );
    } else if affine_kernel_available() {
        assert!(
            matches!(smart.buffers, RecoveryBufferSet::Affine(_)),
            "try_new_smart must pick Affine AVX2 on GFNI without 512"
        );
    }

    let a512 = RecoveryEncoder::new_affine512(slice_size, total_slices, 0, recovery_count);
    assert_eq!(
        matches!(a512.buffers, RecoveryBufferSet::Affine512(_)),
        affine512_kernel_available(),
        "affine512_kernel_available disagrees with new_affine512"
    );
}

/// A manual `--simd` override must never be applied to a specialized buffer
/// layout. `try_new_smart` builds a Shuffle2x encoder on AVX2-without-GFNI
/// hardware, so `--simd scalar` there used to run a Normal-layout kernel
/// against Shuffle2x buffers: no panic, no warning, just wrong parity.
#[test]
fn manual_simd_path_never_corrupts_a_specialized_layout() {
    let (slice_size, total_slices, recovery_count) = (512usize, 4usize, 2usize);
    let slices: Vec<Vec<u8>> = (0..total_slices)
        .map(|s| {
            (0..slice_size)
                .map(|i| ((s * 97 + i * 13 + 5) & 0xFF) as u8)
                .collect()
        })
        .collect();

    let encode = |mut enc: RecoveryEncoder| {
        for s in &slices {
            enc.add_slice(s.clone());
        }
        enc.finish().0
    };

    let reference = encode(RecoveryEncoder::new(
        slice_size,
        total_slices,
        0,
        recovery_count,
    ));

    let paths = [
        SimdPath::Auto,
        SimdPath::Scalar,
        #[cfg(target_arch = "x86_64")]
        SimdPath::Ssse3,
        #[cfg(target_arch = "x86_64")]
        SimdPath::Avx2,
        #[cfg(target_arch = "x86_64")]
        SimdPath::Avx2Gfni,
        #[cfg(target_arch = "x86_64")]
        SimdPath::Avx512Gfni,
        #[cfg(target_arch = "x86_64")]
        SimdPath::Avx512Shuffle,
        #[cfg(target_arch = "aarch64")]
        SimdPath::Neon,
    ];

    for path in paths {
        for (name, enc) in [
            (
                "altmap",
                RecoveryEncoder::new_altmap(slice_size, total_slices, 0, recovery_count),
            ),
            (
                "shuffle2x",
                RecoveryEncoder::new_shuffle2x(slice_size, total_slices, 0, recovery_count),
            ),
            (
                "affine2x",
                RecoveryEncoder::new_affine2x(slice_size, total_slices, 0, recovery_count),
            ),
            (
                "affine",
                RecoveryEncoder::new_affine(slice_size, total_slices, 0, recovery_count),
            ),
            (
                "affine512",
                RecoveryEncoder::new_affine512(slice_size, total_slices, 0, recovery_count),
            ),
            (
                "smart",
                RecoveryEncoder::new_smart(slice_size, total_slices, 0, recovery_count),
            ),
        ] {
            let got = encode(enc.with_simd_path(path));
            for (i, (g, want)) in got.iter().zip(reference.iter()).enumerate() {
                assert_eq!(
                    g.data, want.data,
                    "{name} encoder with --simd {path:?}: recovery block {i} is wrong"
                );
            }
        }
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
#[cfg(target_arch = "x86_64")]
fn new_shuffle2x_produces_correct_recovery_data() {
    // Verify that new_shuffle2x() produces byte-identical recovery data to new().
    // Only meaningful on x86_64 with AVX2 hardware; skip otherwise.
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
    let mut enc_s2x = RecoveryEncoder::new_shuffle2x(slice_size, total_slices, 0, recovery_count);
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
#[cfg(target_arch = "x86_64")]
fn new_shuffle2x_exponent_start_offset() {
    // Verify that exponent_start != 0 works correctly with Shuffle2x.
    // Only meaningful on x86_64 with AVX2 hardware; skip otherwise.
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

    let mut enc_s2x =
        RecoveryEncoder::new_shuffle2x(slice_size, total_slices, exponent_start, recovery_count);
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

#[test]
#[cfg(target_arch = "x86_64")]
fn new_affine_produces_correct_recovery_data() {
    if !affine_kernel_available() {
        return;
    }
    let slice_size = 64usize;
    let total_slices = 7;
    let recovery_count = 5;
    let slices: Vec<Vec<u8>> = (0..total_slices)
        .map(|s| {
            (0..slice_size)
                .map(|i| ((s * 19 + i * 7 + 11) & 0xFF) as u8)
                .collect()
        })
        .collect();
    let mut enc_n = RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count);
    let mut enc_a = RecoveryEncoder::new_affine(slice_size, total_slices, 0, recovery_count);
    for s in &slices {
        enc_n.add_slice(s.clone());
        enc_a.add_slice(s.clone());
    }
    let (n, _) = enc_n.finish();
    let (a, _) = enc_a.finish();
    assert_eq!(a.len(), n.len());
    for (i, (got, want)) in a.iter().zip(n.iter()).enumerate() {
        assert_eq!(got.data, want.data, "Affine recovery slice {i}");
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn new_affine2x_produces_correct_recovery_data() {
    if !std::is_x86_feature_detected!("avx2") || !std::is_x86_feature_detected!("gfni") {
        return;
    }

    let slice_size = 64usize;
    let total_slices = 7;
    let recovery_count = 5;

    let slices: Vec<Vec<u8>> = (0..total_slices)
        .map(|s| {
            (0..slice_size)
                .map(|i| ((s * 19 + i * 7 + 11) & 0xFF) as u8)
                .collect()
        })
        .collect();

    let mut enc_normal = RecoveryEncoder::new(slice_size, total_slices, 0, recovery_count);
    for s in &slices {
        enc_normal.add_slice(s.clone());
    }
    let (normal_recovery, _) = enc_normal.finish();

    let mut enc_a2x = RecoveryEncoder::new_affine2x(slice_size, total_slices, 0, recovery_count);
    for s in &slices {
        enc_a2x.add_slice(s.clone());
    }
    let (a2x_recovery, _) = enc_a2x.finish();

    assert_eq!(a2x_recovery.len(), normal_recovery.len());
    for (i, (got, want)) in a2x_recovery.iter().zip(normal_recovery.iter()).enumerate() {
        assert_eq!(
            got.data, want.data,
            "Affine2x recovery slice {i} differs from normal encoder output"
        );
    }

    let mut enc_off = RecoveryEncoder::new_affine2x(slice_size, total_slices, 5, recovery_count);
    let mut enc_n2 = RecoveryEncoder::new(slice_size, total_slices, 5, recovery_count);
    for s in &slices {
        enc_off.add_slice(s.clone());
        enc_n2.add_slice(s.clone());
    }
    let (off, _) = enc_off.finish();
    let (n2, _) = enc_n2.finish();
    for (i, (got, want)) in off.iter().zip(n2.iter()).enumerate() {
        assert_eq!(got.data, want.data, "Affine2x exponent_start=5 slice {i}");
    }
}

// Ad hoc timing comparison for issue #148: on AVX2-without-GFNI hardware
// (this test's target), does the Shuffle2x layout still win, and by how
// much, when the multiply kernel (plain AVX2 either way) is held fixed?
// `try_new_smart` only ever picks Shuffle2x when GFNI is absent, and no
// Shuffle2x+GFNI kernel exists, so every GFNI-hardware benchmark run to
// date measured the Normal layout exclusively. This isolates the layout
// axis from the kernel axis on hardware where both layouts use the same
// kernel, as groundwork for deciding whether a combined Shuffle2x+GFNI
// kernel is worth building. `#[ignore]`d: takes several seconds, not
// routine-test material (`cargo test --release -p parmesan -- --ignored
// shuffle2x_vs_normal_layout_throughput_movie_1080p`).
#[test]
#[ignore]
fn shuffle2x_vs_normal_layout_throughput_movie_1080p() {
    use std::time::Instant;

    // Same geometry as bench/FINDINGS.md's `movie-1080p` workload
    // (bench/results/ip-172-31-41-50/20260818T013317Z/raw.csv).
    const SLICE_SIZE: usize = 806_912;
    const TOTAL_SLICES: usize = 1997;
    const RECOVERY_COUNT: usize = 200;
    const EXPONENT_START: u32 = 0;
    const REPS: u32 = 5;

    // Deterministic pseudo-random content per slice, not all-zero/same
    // bytes, so neither kernel takes a degenerate fast path.
    fn xorshift_fill(seed: u64, buf: &mut [u8]) {
        let mut state = seed | 1;
        for chunk in buf.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
    let slices: Vec<Vec<u8>> = (0..TOTAL_SLICES)
        .map(|i| {
            let mut buf = vec![0u8; SLICE_SIZE];
            xorshift_fill(0x9E3779B97F4A7C15 ^ (i as u64), &mut buf);
            buf
        })
        .collect();

    #[cfg(target_arch = "x86_64")]
    assert!(
        !std::is_x86_feature_detected!("gfni"),
        "this comparison is meaningless on GFNI hardware: Normal layout \
             would auto-dispatch to the GFNI kernel instead of plain AVX2, \
             breaking the kernel-held-fixed premise of this test"
    );
    #[cfg(target_arch = "x86_64")]
    assert!(
        std::is_x86_feature_detected!("avx2"),
        "need AVX2 for a meaningful Shuffle2x-vs-Normal comparison"
    );

    let run_normal = || {
        let mut enc =
            RecoveryEncoder::new(SLICE_SIZE, TOTAL_SLICES, EXPONENT_START, RECOVERY_COUNT);
        for s in &slices {
            enc.add_slice(s.clone());
        }
        let _ = enc.finish();
    };
    let run_shuffle2x = || {
        let mut enc = RecoveryEncoder::new_shuffle2x(
            SLICE_SIZE,
            TOTAL_SLICES,
            EXPONENT_START,
            RECOVERY_COUNT,
        );
        for s in &slices {
            enc.add_slice(s.clone());
        }
        let _ = enc.finish();
    };

    // Warm-up (page-in slices, prime allocator) — unmeasured.
    run_normal();
    run_shuffle2x();

    let mut normal_ms: Vec<f64> = Vec::with_capacity(REPS as usize);
    let mut s2x_ms: Vec<f64> = Vec::with_capacity(REPS as usize);
    for _ in 0..REPS {
        let t = Instant::now();
        run_normal();
        normal_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    for _ in 0..REPS {
        let t = Instant::now();
        run_shuffle2x();
        s2x_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }
    let input_mib = (SLICE_SIZE * TOTAL_SLICES) as f64 / (1024.0 * 1024.0);
    let normal_med_ms = median(normal_ms);
    let s2x_med_ms = median(s2x_ms);
    let normal_mibs = input_mib / (normal_med_ms / 1000.0);
    let s2x_mibs = input_mib / (s2x_med_ms / 1000.0);

    eprintln!(
        "\n== Normal vs Shuffle2x, movie-1080p geometry, {input_mib:.1} MiB, {REPS} reps ==\n\
             Normal+AVX2:    {normal_med_ms:.1} ms median -> {normal_mibs:.1} MiB/s\n\
             Shuffle2x+AVX2: {s2x_med_ms:.1} ms median -> {s2x_mibs:.1} MiB/s\n\
             Shuffle2x vs Normal: {:+.1}%\n",
        (s2x_mibs / normal_mibs - 1.0) * 100.0
    );
}

// §148 continued: is ALTMAP (parmesan's own "XOR Bit Dependencies" kernel,
// `crates/parmesan/src/gf16.rs`'s `xor_dep_matrix` + `flush_avx2_altmap_work`)
// competitive with Shuffle2x now that its per-vector dependency-mask decode
// is hoisted out of the hot loop (see `decode_plane_deps`)? ParPar's own
// `fast-gf-multiplication.md` calls the XOR Bit Dependencies technique "the
// fastest technique I've found for most x86 CPUs... for w=16", ahead of the
// Vector Split Lookup (shuffle) technique Shuffle2x/Normal both use — and
// `Galois16Mul::default_method()` in ParPar's own `gf16mul.cpp` confirms
// this isn't just a claim: on any AVX2 x86-64 host that can JIT (`canMemWX`,
// `propFastJit`, not emulated), it picks `GF16_XOR_JIT_AVX2` ahead of
// `GF16_SHUFFLE_AVX2` — exactly this machine's class of hardware (AVX2,
// no GFNI). ParPar's version is JIT-compiled per coefficient (zero
// interpretation overhead, plus common-subexpression elimination across
// output bits that this fixed, non-JIT port does not attempt); this test
// measures how far the branch-free-but-uncompressed version lands before
// deciding whether investing in a real JIT (or a static CSE pass) is
// worthwhile. `#[ignore]`d: takes several seconds (`cargo test --release
// -p parmesan-par2 -- --ignored altmap_vs_shuffle2x_layout_throughput_movie_1080p`).
#[test]
#[ignore]
fn altmap_vs_shuffle2x_layout_throughput_movie_1080p() {
    use std::time::Instant;

    // Same geometry as the sibling Normal-vs-Shuffle2x test above.
    const SLICE_SIZE: usize = 806_912;
    const TOTAL_SLICES: usize = 1997;
    const RECOVERY_COUNT: usize = 200;
    const EXPONENT_START: u32 = 0;
    const REPS: u32 = 5;

    fn xorshift_fill(seed: u64, buf: &mut [u8]) {
        let mut state = seed | 1;
        for chunk in buf.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
    let slices: Vec<Vec<u8>> = (0..TOTAL_SLICES)
        .map(|i| {
            let mut buf = vec![0u8; SLICE_SIZE];
            xorshift_fill(0x9E3779B97F4A7C15 ^ (i as u64), &mut buf);
            buf
        })
        .collect();

    #[cfg(target_arch = "x86_64")]
    assert!(
        !std::is_x86_feature_detected!("gfni"),
        "ALTMAP has no kernel on GFNI hardware (try_new_altmap falls back \
             to Normal there), so this comparison needs a non-GFNI AVX2 host"
    );
    #[cfg(target_arch = "x86_64")]
    assert!(
        std::is_x86_feature_detected!("avx2"),
        "need AVX2 for both the Shuffle2x and ALTMAP kernels"
    );

    let run_shuffle2x = || {
        let mut enc = RecoveryEncoder::new_shuffle2x(
            SLICE_SIZE,
            TOTAL_SLICES,
            EXPONENT_START,
            RECOVERY_COUNT,
        );
        for s in &slices {
            enc.add_slice(s.clone());
        }
        let _ = enc.finish();
    };
    let run_altmap = || {
        let mut enc =
            RecoveryEncoder::new_altmap(SLICE_SIZE, TOTAL_SLICES, EXPONENT_START, RECOVERY_COUNT);
        for s in &slices {
            enc.add_slice(s.clone());
        }
        let _ = enc.finish();
    };

    // Warm-up (page-in slices, prime allocator, build the 2 MiB dep_tables
    // once) — unmeasured.
    run_shuffle2x();
    run_altmap();

    // Reps interleaved (Shuffle2x, ALTMAP, Shuffle2x, ...), not blocked —
    // see the sibling GFNI test's rationale for why, on a shared machine.
    let mut s2x_ms: Vec<f64> = Vec::with_capacity(REPS as usize);
    let mut altmap_ms: Vec<f64> = Vec::with_capacity(REPS as usize);
    for _ in 0..REPS {
        let t = Instant::now();
        run_shuffle2x();
        s2x_ms.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        run_altmap();
        altmap_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }
    let input_mib = (SLICE_SIZE * TOTAL_SLICES) as f64 / (1024.0 * 1024.0);
    let s2x_med_ms = median(s2x_ms);
    let altmap_med_ms = median(altmap_ms);
    let s2x_mibs = input_mib / (s2x_med_ms / 1000.0);
    let altmap_mibs = input_mib / (altmap_med_ms / 1000.0);

    eprintln!(
        "\n== Shuffle2x vs ALTMAP (hoisted), movie-1080p geometry, {input_mib:.1} MiB, \
             {REPS} reps ==\n\
             Shuffle2x+AVX2: {s2x_med_ms:.1} ms median -> {s2x_mibs:.1} MiB/s\n\
             ALTMAP+AVX2:    {altmap_med_ms:.1} ms median -> {altmap_mibs:.1} MiB/s\n\
             ALTMAP vs Shuffle2x: {:+.1}%\n",
        (altmap_mibs / s2x_mibs - 1.0) * 100.0
    );
}

// Step 2 for issue #148, on GFNI hardware: does Shuffle2x's layout
// advantage survive being run against the Normal layout's GFNI kernel
// (its fastest available path there), rather than against Normal's
// plain-AVX2 kernel as in the sibling test above? No Shuffle2x+GFNI
// kernel exists, so Shuffle2x here still runs plain AVX2
// (`flush_avx2_shuffle2x`, layout-fixed regardless of `simd_path`) while
// Normal auto-dispatches to whatever this CPU's best kernel is — GFNI,
// on the hardware this test requires. If Shuffle2x+AVX2 still wins (or
// even just stays close) against Normal+GFNI, a combined Shuffle2x+GFNI
// kernel is a concretely promising fix candidate for #148; if Normal+GFNI
// pulls decisively ahead, GFNI's dedicated instruction has made the
// layout's plain-AVX2 multiply trick moot and this lead is closed.
//
// Reps are interleaved (Normal, Shuffle2x, Normal, Shuffle2x, ...)
// instead of run in two back-to-back blocks like the sibling test above.
// On the dev machine that test was first written on, back-to-back
// blocks let a single mid-run load spike (this is a shared box, not a
// dedicated bench machine) selectively contaminate one layout's whole
// block — one of five manual trials read +8.2% against a median of
// +51.3% across the other four, traced to exactly that. A dedicated
// cloud instance shouldn't have that problem, but interleaving is free
// insurance against it either way.
#[test]
#[ignore]
fn shuffle2x_avx2_vs_normal_gfni_layout_throughput_movie_1080p() {
    use std::time::Instant;

    const SLICE_SIZE: usize = 806_912;
    const TOTAL_SLICES: usize = 1997;
    const RECOVERY_COUNT: usize = 200;
    const EXPONENT_START: u32 = 0;
    const REPS: u32 = 7;

    fn xorshift_fill(seed: u64, buf: &mut [u8]) {
        let mut state = seed | 1;
        for chunk in buf.chunks_mut(8) {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            let bytes = state.to_le_bytes();
            chunk.copy_from_slice(&bytes[..chunk.len()]);
        }
    }
    let slices: Vec<Vec<u8>> = (0..TOTAL_SLICES)
        .map(|i| {
            let mut buf = vec![0u8; SLICE_SIZE];
            xorshift_fill(0x9E3779B97F4A7C15 ^ (i as u64), &mut buf);
            buf
        })
        .collect();

    #[cfg(target_arch = "x86_64")]
    assert!(
        std::is_x86_feature_detected!("gfni"),
        "this test compares Shuffle2x+AVX2 against Normal's *GFNI* \
             kernel specifically — on non-GFNI hardware Normal would fall \
             back to plain AVX2 and this would just re-measure the sibling \
             test above. Run shuffle2x_vs_normal_layout_throughput_movie_1080p \
             instead on non-GFNI hardware."
    );

    let run_normal = || {
        let mut enc =
            RecoveryEncoder::new(SLICE_SIZE, TOTAL_SLICES, EXPONENT_START, RECOVERY_COUNT);
        for s in &slices {
            enc.add_slice(s.clone());
        }
        let _ = enc.finish();
    };
    let run_shuffle2x = || {
        let mut enc = RecoveryEncoder::new_shuffle2x(
            SLICE_SIZE,
            TOTAL_SLICES,
            EXPONENT_START,
            RECOVERY_COUNT,
        );
        for s in &slices {
            enc.add_slice(s.clone());
        }
        let _ = enc.finish();
    };

    // Warm-up both — unmeasured.
    run_normal();
    run_shuffle2x();

    let mut normal_ms: Vec<f64> = Vec::with_capacity(REPS as usize);
    let mut s2x_ms: Vec<f64> = Vec::with_capacity(REPS as usize);
    for _ in 0..REPS {
        let t = Instant::now();
        run_normal();
        normal_ms.push(t.elapsed().as_secs_f64() * 1000.0);

        let t = Instant::now();
        run_shuffle2x();
        s2x_ms.push(t.elapsed().as_secs_f64() * 1000.0);
    }

    fn median(mut v: Vec<f64>) -> f64 {
        v.sort_by(|a, b| a.partial_cmp(b).unwrap());
        v[v.len() / 2]
    }
    let input_mib = (SLICE_SIZE * TOTAL_SLICES) as f64 / (1024.0 * 1024.0);
    let normal_med_ms = median(normal_ms);
    let s2x_med_ms = median(s2x_ms);
    let normal_mibs = input_mib / (normal_med_ms / 1000.0);
    let s2x_mibs = input_mib / (s2x_med_ms / 1000.0);

    eprintln!(
        "\n== Normal+GFNI vs Shuffle2x+AVX2, movie-1080p geometry, {input_mib:.1} MiB, \
             {REPS} interleaved reps ==\n\
             Normal (auto/GFNI): {normal_med_ms:.1} ms median -> {normal_mibs:.1} MiB/s\n\
             Shuffle2x+AVX2:     {s2x_med_ms:.1} ms median -> {s2x_mibs:.1} MiB/s\n\
             Shuffle2x vs Normal+GFNI: {:+.1}%\n",
        (s2x_mibs / normal_mibs - 1.0) * 100.0
    );
}
