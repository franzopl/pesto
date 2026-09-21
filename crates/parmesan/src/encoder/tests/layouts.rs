use super::super::*;

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
            matches!(smart.buffers, RecoveryBufferSet::Affine512(_)),
            "try_new_smart must pick Affine512 on AVX-512+GFNI"
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
