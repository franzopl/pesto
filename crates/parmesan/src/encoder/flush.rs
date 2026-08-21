use super::*;

impl RecoveryEncoder {
    /// Move consumed queue buffers into the free-list (preserving their
    /// allocations) and restore the empty queue.
    /// Grow/reuse `affine_prepare` and shuffle-prepare each queued slice into it.
    #[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
    pub(super) fn prepare_affine_inputs(
        queued: &[Vec<u8>],
        pool: &mut Vec<Vec<u8>>,
        wide512: bool,
    ) {
        if queued.is_empty() {
            return;
        }
        let _ = wide512;
        let len = queued[0].len();
        while pool.len() < queued.len() {
            pool.push(vec![0u8; len]);
        }
        if pool.len() > queued.len() {
            pool.truncate(queued.len());
        }
        queued
            .par_iter()
            .zip(pool.par_iter_mut())
            .for_each(|(src, dst)| {
                if dst.len() != src.len() {
                    dst.clear();
                    dst.resize(src.len(), 0);
                }
                #[cfg(target_arch = "x86_64")]
                if wide512 {
                    crate::affine::to_affine512(src, dst);
                    return;
                }
                let _ = wide512;
                crate::affine::to_affine(src, dst);
            });
    }

    pub(super) fn recycle_queue(&mut self, mut queued: Vec<Vec<u8>>) {
        self.free_buffers.reserve(queued.len());
        for mut buf in queued.drain(..) {
            buf.clear();
            self.free_buffers.push(buf);
        }
        self.queued_slices = queued;
    }

    /// Remove all currently pooled free buffers and return them to the caller.
    ///
    /// Used by the background-worker path in `poster.rs` to ferry recycled
    /// slice allocations back to the producer without exposing `free_buffers`
    /// directly.
    pub fn drain_free_buffers(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.free_buffers)
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

        // Process if we hit the count limit (cache blocking) or a memory limit
        // (to keep the footprint lean). 256 MB is enough to amortize the flush
        // cost even for very few slices.
        //
        // §148: the count limit used to be 128, chosen without a documented
        // tuning pass. Every `flush_*_work` pre-builds one SIMD coefficient
        // table per (recovery_block × queued_slice) pair (`all_tables`) in one
        // rayon pass, then a second rayon pass reads it back while doing the
        // actual multiply, so this cap sets how much of that table is in
        // flight per flush. Swept on this issue's `movie-1080p` reproduction
        // (`bench/data/movie-1080p@0.25`, real `parmesan create` CLI, not just
        // the in-process micro-benchmarks) at three very different recovery
        // counts:
        //
        //   recovery_count=20:   64 ≈ 128 (no regression, table already tiny)
        //   recovery_count=200:  64 is +21% over 128 (5.7s -> 4.7s median)
        //   recovery_count=1000: 64 is +6% over 128, but noisy (this machine
        //                        runs other services — pooled across two
        //                        sweep sessions, ~19.9s vs ~21.2s median)
        //
        // A first attempt made this adaptive — shrinking the cap as
        // `recovery_count` grows, on the theory that keeping the table's
        // total byte size under this machine's 12 MiB L3 was the mechanism
        // (perf stat showed 81% more cache-references and 92% more
        // cache-misses than parpar's equivalent run at recovery_count=200,
        // consistent with an overflowing working set at 128). That
        // hypothesis does not fully hold up: at recovery_count=1000 the
        // adaptive formula's shrunk cap (16) measured *worse* than the flat
        // 128 it was meant to replace, and a follow-up `perf stat` on the
        // fixed build showed cache-misses essentially unchanged (only CPU
        // utilization and page-faults improved) — so the true mechanism is
        // not fully understood, and a flat 64 (empirically safe and better
        // across all three measured points, unlike the adaptive formula) is
        // the honest, validated fix rather than a theory dressed up as one.
        // See `bench/FINDINGS.md` §3 for the full writeup.
        let queued_bytes = self.queued_slices.len() * self.slice_words * 2;

        let batch_limit = match self.buffers {
            RecoveryBufferSet::Affine512(_) => 12,
            _ => 64,
        };

        if self.queued_slices.len() >= batch_limit || queued_bytes >= self.flush_limit_bytes {
            self.flush();
        }
    }
    fn flush(&mut self) {
        if self.queued_slices.is_empty() {
            return;
        }

        // ── Manual Override (SimdPath) ───────────────────────────────────────
        //
        // Only honoured for Normal-layout buffers. Every kernel below reads and
        // writes recovery buffers as plain `u16` slices; pointing one at
        // ALTMAP or Shuffle2x buffers means it either panics on
        // `as_normal_mut` (SSSE3/AVX2/GFNI) or, worse, silently produces
        // parity that `finish` then runs through a layout conversion that was
        // never applied (scalar) — `--simd scalar` on an AVX2-without-GFNI CPU
        // did exactly that, since `try_new_smart` builds a Shuffle2x encoder
        // there. A specialized layout already implies its own kernel is
        // present (the constructors fall back otherwise), so fall through to
        // auto-detection instead, the same way an unavailable path does.
        let layout_is_normal = matches!(self.buffers, RecoveryBufferSet::Normal(_));
        match self.simd_path {
            _ if !layout_is_normal => {} // specialized layout: auto-detect below
            SimdPath::Auto => {}         // proceed to auto-detection
            SimdPath::Scalar => {
                self.flush_scalar();
                return;
            }
            #[cfg(target_arch = "x86_64")]
            SimdPath::Ssse3 if std::is_x86_feature_detected!("ssse3") => {
                unsafe { self.flush_ssse3() };
                return;
            }
            #[cfg(target_arch = "x86_64")]
            SimdPath::Avx2 if std::is_x86_feature_detected!("avx2") => {
                unsafe { self.flush_avx2() };
                return;
            }
            #[cfg(target_arch = "x86_64")]
            SimdPath::Avx2Gfni
                if std::is_x86_feature_detected!("avx2")
                    && std::is_x86_feature_detected!("gfni") =>
            {
                unsafe { self.flush_avx2_gfni() };
                return;
            }
            #[cfg(target_arch = "x86_64")]
            SimdPath::Avx512Gfni
                if std::is_x86_feature_detected!("avx512f")
                    && std::is_x86_feature_detected!("avx512bw")
                    && std::is_x86_feature_detected!("gfni") =>
            {
                unsafe { self.flush_avx512_gfni() };
                return;
            }
            #[cfg(target_arch = "x86_64")]
            SimdPath::Avx512Shuffle
                if std::is_x86_feature_detected!("avx512f")
                    && std::is_x86_feature_detected!("avx512bw") =>
            {
                unsafe { self.flush_avx512_shuffle() };
                return;
            }
            #[cfg(target_arch = "aarch64")]
            SimdPath::Neon => {
                unsafe { self.flush_neon_clmul() };
                return;
            }
            _ => {} // specified path not supported/available; fall through to auto
        }

        // ALTMAP path: AVX2 XOR bit-dependency kernel (Phase 27e).
        #[cfg(target_arch = "x86_64")]
        if matches!(self.buffers, RecoveryBufferSet::Altmap(_)) {
            if std::is_x86_feature_detected!("avx2") && self.dep_tables.is_some() {
                unsafe {
                    self.flush_avx2_altmap();
                }
                return;
            }
            // `try_new_altmap` only builds ALTMAP buffers once it has confirmed
            // this kernel is available, so reaching here means that invariant
            // broke. This used to drain the queue unprocessed, which turned a
            // broken invariant into all-zero recovery blocks handed back as if
            // they were real parity.
            unreachable!(
                "ALTMAP buffers without the ALTMAP kernel — try_new_altmap must \
                 fall back to the portable layout when AVX2/dep_tables are absent"
            );
        }

        // Shuffle2x path: AVX2 nibble-shuffle kernel with Shuffle2x buffer layout (Phase 28b).
        #[cfg(target_arch = "x86_64")]
        if matches!(self.buffers, RecoveryBufferSet::Shuffle2x(_)) {
            if std::is_x86_feature_detected!("avx2") {
                unsafe { self.flush_avx2_shuffle2x() };
                return;
            }
            // Same invariant as the ALTMAP arm above.
            unreachable!(
                "Shuffle2x buffers without AVX2 — try_new_shuffle2x must fall \
                 back to the portable layout"
            );
        }

        #[cfg(target_arch = "x86_64")]
        if matches!(self.buffers, RecoveryBufferSet::Affine2x(_)) {
            if affine512_kernel_available() {
                unsafe { self.flush_avx512_affine2x() };
                return;
            } else if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("gfni")
            {
                unsafe { self.flush_avx2_affine2x() };
                return;
            }
            unreachable!(
                "Affine2x buffers without AVX2+GFNI — try_new_affine2x must fall \
                 back to the portable layout"
            );
        }

        #[cfg(target_arch = "x86_64")]
        if matches!(self.buffers, RecoveryBufferSet::Affine(_)) {
            if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("gfni") {
                unsafe { self.flush_avx2_affine() };
                return;
            }
            unreachable!(
                "Affine buffers without AVX2+GFNI — try_new_affine must fall \
                 back to the portable layout"
            );
        }

        #[cfg(target_arch = "x86_64")]
        if matches!(self.buffers, RecoveryBufferSet::Affine512(_)) {
            if affine512_kernel_available() {
                unsafe { self.flush_avx512_affine() };
                return;
            }
            unreachable!("Affine512 buffers without AVX-512+GFNI");
        }

        // When bench-internals is active a forced path overrides auto-detection.
        #[cfg(feature = "bench-internals")]
        if let Some(path) = self.forced_path {
            match path {
                BenchPath::Scalar => {
                    self.flush_scalar();
                    return;
                }
                #[cfg(target_arch = "x86_64")]
                BenchPath::Ssse3 => unsafe {
                    self.flush_ssse3();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx2 => unsafe {
                    self.flush_avx2();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx2Gfni => unsafe {
                    self.flush_avx2_gfni();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx512Gfni => unsafe {
                    self.flush_avx512_gfni();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx2Altmap => unsafe {
                    self.flush_avx2_altmap();
                    return;
                },
                #[cfg(target_arch = "x86_64")]
                BenchPath::Avx2Shuffle2x => unsafe {
                    self.flush_avx2_shuffle2x();
                    return;
                },
                #[cfg(target_arch = "aarch64")]
                BenchPath::NeonClmul => unsafe {
                    self.flush_neon_clmul();
                    return;
                },
            }
        }

        // AVX-512+GFNI path: verified correct on Intel Ice Lake Xeon (AWS m6i)
        // via gfni_recovery_matches_scalar (bench-internals feature).
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx512f")
            && std::is_x86_feature_detected!("avx512bw")
            && std::is_x86_feature_detected!("gfni")
        {
            unsafe {
                self.flush_avx512_gfni();
            }
            return;
        }

        // AVX2+GFNI path: verified correct on i5-14400 (simd_recovery_matches_scalar).
        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") && std::is_x86_feature_detected!("gfni") {
            unsafe {
                self.flush_avx2_gfni();
            }
            return;
        }

        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx512f") && std::is_x86_feature_detected!("avx512bw") {
            unsafe {
                self.flush_avx512_shuffle();
            }
            return;
        }

        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("avx2") {
            unsafe {
                self.flush_avx2();
            }
            return;
        }

        #[cfg(target_arch = "x86_64")]
        if std::is_x86_feature_detected!("ssse3") {
            unsafe {
                self.flush_ssse3();
            }
        }

        // NEON is mandatory on AArch64; pmull.8h is part of base NEON (ARMv8-A).
        #[cfg(target_arch = "aarch64")]
        unsafe {
            self.flush_neon_clmul();
        }
        #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
        self.flush_scalar();
    }

    /// Consume the encoder and return the finished recovery slices together
    /// with all accumulated per-slice checksums (empty when checksums were
    /// not enabled via [`Self::with_checksums`]).
    ///
    /// This conversion is intentionally sequential rather than parallel: each
    /// input buffer is converted and dropped before the next one starts, so
    /// the transient old+new duplication is bounded to a single extra slice
    /// instead of `rayon::current_num_threads()` extra slices held at once.
    /// With hundreds of MB per recovery slice and dozens of cores, running
    /// this in parallel (as before) could spike peak memory by several GiB
    /// right at the point `poster`'s memory budget assumes we're done
    /// allocating, which is what caused OOM aborts on memory-constrained
    /// hosts.
    pub fn finish(mut self) -> (Vec<RecoverySlice>, Vec<SliceChecksum>) {
        self.flush();
        let checksums = self.pending_checksums;
        let exponent_start = self.exponent_start;
        let slice_words = self.slice_words;
        let slices: Vec<RecoverySlice> = match self.buffers {
            RecoveryBufferSet::Normal(bufs) => bufs
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
                .collect(),
            RecoveryBufferSet::Altmap(bufs) => bufs
                .into_iter()
                .enumerate()
                .map(|(i, altmap_buf)| {
                    let mut words = vec![0u16; slice_words];
                    super::altmap::from_altmap(&altmap_buf, &mut words);
                    let mut data = Vec::with_capacity(slice_words * 2);
                    for word in words {
                        data.extend_from_slice(&word.to_le_bytes());
                    }
                    RecoverySlice {
                        exponent: exponent_start + i as u32,
                        data,
                    }
                })
                .collect(),
            RecoveryBufferSet::Shuffle2x(bufs) => bufs
                .into_iter()
                .enumerate()
                .map(|(i, s2x_buf)| {
                    let mut normal = vec![0u8; s2x_buf.len()];
                    super::shuffle2x::from_shuffle2x(&s2x_buf, &mut normal);
                    RecoverySlice {
                        exponent: exponent_start + i as u32,
                        data: normal,
                    }
                })
                .collect(),
            RecoveryBufferSet::Affine2x(bufs) => bufs
                .into_iter()
                .enumerate()
                .map(|(i, a2x_buf)| {
                    let mut normal = vec![0u8; a2x_buf.len()];
                    super::affine2x::from_affine2x(&a2x_buf, &mut normal);
                    RecoverySlice {
                        exponent: exponent_start + i as u32,
                        data: normal,
                    }
                })
                .collect(),
            RecoveryBufferSet::Affine(bufs) => bufs
                .into_iter()
                .enumerate()
                .map(|(i, af_buf)| {
                    let mut normal = vec![0u8; af_buf.len()];
                    super::affine::from_affine(&af_buf, &mut normal);
                    RecoverySlice {
                        exponent: exponent_start + i as u32,
                        data: normal,
                    }
                })
                .collect(),
            RecoveryBufferSet::Affine512(acc) => {
                #[cfg(target_arch = "x86_64")]
                {
                    unsafe { affine512_acc_to_slices(acc, exponent_start) }
                }
                #[cfg(not(target_arch = "x86_64"))]
                {
                    let _ = (acc, exponent_start);
                    Vec::new()
                }
            }
        };
        (slices, checksums)
    }
}
