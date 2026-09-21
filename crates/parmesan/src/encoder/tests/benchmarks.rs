use super::super::*;

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
