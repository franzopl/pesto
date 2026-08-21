use super::*;

impl RecoveryEncoder {
    /// Same 4-nibble shuffle algorithm as `flush_avx2` but operating on 128-bit
    /// `__m128i` registers. Covers all x86-64 CPUs with SSSE3 (2007+) that do
    /// not have AVX2.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "ssse3")]
    pub(super) unsafe fn flush_ssse3(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            let buffers = self.buffers.as_normal_mut();
            let logbases = &self.logbases;
            let exponent_start = self.exponent_start;
            let gf = &self.gf;
            let ((), cs) = rayon::join(
                || unsafe {
                    Self::flush_ssse3_work(
                        buffers,
                        &queued,
                        start_index,
                        logbases,
                        exponent_start,
                        gf,
                    )
                },
                || queued.par_iter().map(|s| slice_checksum(s)).collect(),
            );
            cs
        } else {
            unsafe {
                Self::flush_ssse3_work(
                    self.buffers.as_normal_mut(),
                    &queued,
                    start_index,
                    &self.logbases,
                    self.exponent_start,
                    &self.gf,
                );
            }
            Vec::new()
        };

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "ssse3")]
    pub(super) unsafe fn flush_ssse3_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        let mask_f = _mm_set1_epi8(0x0F_u8 as i8);
        let mask_even = _mm_set1_epi16(0x00FF_u16 as i16);

        let n_rec = buffers.len();
        let n_queued = queued.len();

        // Pre-build all SIMD coefficient tables in a single parallel pass — one Vec
        // entry per (recovery_block × input_slice) pair, laid out as [rec * n_queued + q].
        let all_tables: Vec<Ssse3Table> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| unsafe {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut tl_l = [0u8; 16];
                let mut tl_h = [0u8; 16];
                let mut th_l = [0u8; 16];
                let mut th_h = [0u8; 16];
                let mut hl_l = [0u8; 16];
                let mut hl_h = [0u8; 16];
                let mut hh_l = [0u8; 16];
                let mut hh_h = [0u8; 16];

                for val in 0..16usize {
                    let r0 = gf.mul(val as u16, coeff);
                    tl_l[val] = (r0 & 0xFF) as u8;
                    th_l[val] = (r0 >> 8) as u8;
                    let r1 = gf.mul((val as u16) << 4, coeff);
                    tl_h[val] = (r1 & 0xFF) as u8;
                    th_h[val] = (r1 >> 8) as u8;
                    let r2 = gf.mul((val as u16) << 8, coeff);
                    hl_l[val] = (r2 & 0xFF) as u8;
                    hh_l[val] = (r2 >> 8) as u8;
                    let r3 = gf.mul((val as u16) << 12, coeff);
                    hl_h[val] = (r3 & 0xFF) as u8;
                    hh_h[val] = (r3 >> 8) as u8;
                }

                let v_tl_l = _mm_loadu_si128(tl_l.as_ptr() as *const __m128i);
                let v_tl_h = _mm_loadu_si128(tl_h.as_ptr() as *const __m128i);
                let v_th_l = _mm_loadu_si128(th_l.as_ptr() as *const __m128i);
                let v_th_h = _mm_loadu_si128(th_h.as_ptr() as *const __m128i);
                let v_hl_l = _mm_loadu_si128(hl_l.as_ptr() as *const __m128i);
                let v_hl_h = _mm_loadu_si128(hl_h.as_ptr() as *const __m128i);
                let v_hh_l = _mm_loadu_si128(hh_l.as_ptr() as *const __m128i);
                let v_hh_h = _mm_loadu_si128(hh_h.as_ptr() as *const __m128i);

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                (
                    v_tl_l, v_tl_h, v_th_l, v_th_h, v_hl_l, v_hl_h, v_hh_l, v_hh_h, table_low,
                    table_high,
                )
            })
            .collect();

        // Chunk-outer loop: all rayon tasks rendezvous at each chunk boundary so
        // all threads read the same 4 MiB input window → L3 hits (same strategy as AVX2).
        let slice_words = queued[0].len() / 2;
        let chunk_size = 16384usize; // 32 KiB recovery buffer chunk stays in L1/L2
        let n_chunks = slice_words.div_ceil(chunk_size);

        for chunk_idx in 0..n_chunks {
            let word_start = chunk_idx * chunk_size;
            let word_end = (word_start + chunk_size).min(slice_words);
            let byte_offset = word_start * 2;
            let byte_len = (word_end - word_start) * 2;
            let blocks_16 = byte_len / 16;
            let remainder = byte_len % 16;

            buffers
                .par_chunks_mut(2)
                .enumerate()
                .for_each(|(pair_idx, buf_pair)| unsafe {
                    let i = pair_idx * 2;
                    match buf_pair {
                        [buf_a, buf_b] => {
                            let base_a = i * n_queued;
                            let base_b = (i + 1) * n_queued;
                            let chunk_a = &mut buf_a[word_start..word_end];
                            let chunk_b = &mut buf_b[word_start..word_end];

                            for q_idx in 0..n_queued {
                                let (
                                    v_tl_l_a,
                                    v_tl_h_a,
                                    v_th_l_a,
                                    v_th_h_a,
                                    v_hl_l_a,
                                    v_hl_h_a,
                                    v_hh_l_a,
                                    v_hh_h_a,
                                    ref tlow_a,
                                    ref thigh_a,
                                ) = all_tables[base_a + q_idx];
                                let (
                                    v_tl_l_b,
                                    v_tl_h_b,
                                    v_th_l_b,
                                    v_th_h_b,
                                    v_hl_l_b,
                                    v_hl_h_b,
                                    v_hh_l_b,
                                    v_hh_h_b,
                                    ref tlow_b,
                                    ref thigh_b,
                                ) = all_tables[base_b + q_idx];
                                let slice_chunk =
                                    &queued[q_idx][byte_offset..byte_offset + byte_len];

                                let mut ptr_in = slice_chunk.as_ptr() as *const __m128i;
                                let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m128i;
                                let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m128i;
                                let end = ptr_in.add(blocks_16);

                                while ptr_in < end {
                                    let input = _mm_loadu_si128(ptr_in);
                                    let n0_2 = _mm_and_si128(input, mask_f);
                                    let n1_3 = _mm_and_si128(_mm_srli_epi16(input, 4), mask_f);

                                    // Block A
                                    let rle_a = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_tl_l_a, n0_2),
                                        _mm_shuffle_epi8(v_tl_h_a, n1_3),
                                    );
                                    let rhe_a = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_th_l_a, n0_2),
                                        _mm_shuffle_epi8(v_th_h_a, n1_3),
                                    );
                                    let rlo_a = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hl_l_a, n0_2),
                                        _mm_shuffle_epi8(v_hl_h_a, n1_3),
                                    );
                                    let rho_a = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hh_l_a, n0_2),
                                        _mm_shuffle_epi8(v_hh_h_a, n1_3),
                                    );
                                    let sle_a = _mm_xor_si128(rle_a, _mm_srli_epi16(rlo_a, 8));
                                    let she_a = _mm_xor_si128(rhe_a, _mm_srli_epi16(rho_a, 8));
                                    let out_a = _mm_or_si128(
                                        _mm_and_si128(sle_a, mask_even),
                                        _mm_slli_epi16(she_a, 8),
                                    );
                                    let prev_a = _mm_loadu_si128(ptr_a);
                                    _mm_storeu_si128(ptr_a, _mm_xor_si128(prev_a, out_a));

                                    // Block B — reuses n0_2 and n1_3
                                    let rle_b = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_tl_l_b, n0_2),
                                        _mm_shuffle_epi8(v_tl_h_b, n1_3),
                                    );
                                    let rhe_b = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_th_l_b, n0_2),
                                        _mm_shuffle_epi8(v_th_h_b, n1_3),
                                    );
                                    let rlo_b = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hl_l_b, n0_2),
                                        _mm_shuffle_epi8(v_hl_h_b, n1_3),
                                    );
                                    let rho_b = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hh_l_b, n0_2),
                                        _mm_shuffle_epi8(v_hh_h_b, n1_3),
                                    );
                                    let sle_b = _mm_xor_si128(rle_b, _mm_srli_epi16(rlo_b, 8));
                                    let she_b = _mm_xor_si128(rhe_b, _mm_srli_epi16(rho_b, 8));
                                    let out_b = _mm_or_si128(
                                        _mm_and_si128(sle_b, mask_even),
                                        _mm_slli_epi16(she_b, 8),
                                    );
                                    let prev_b = _mm_loadu_si128(ptr_b);
                                    _mm_storeu_si128(ptr_b, _mm_xor_si128(prev_b, out_b));

                                    ptr_in = ptr_in.add(1);
                                    ptr_a = ptr_a.add(1);
                                    ptr_b = ptr_b.add(1);
                                }

                                if remainder > 0 {
                                    let ow = blocks_16 * 8;
                                    let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                    let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                    let mut p_in = slice_chunk[blocks_16 * 16..].as_ptr();
                                    let tail_end = p_in.add(remainder);
                                    while p_in < tail_end {
                                        let lo = *p_in as usize;
                                        let hi = *p_in.add(1) as usize;
                                        *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                        *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                        pw_a = pw_a.add(1);
                                        pw_b = pw_b.add(1);
                                        p_in = p_in.add(2);
                                    }
                                }
                            }
                        }
                        [buf_a] => {
                            let base = i * n_queued;
                            let chunk_a = &mut buf_a[word_start..word_end];

                            for q_idx in 0..n_queued {
                                let (
                                    v_tl_l,
                                    v_tl_h,
                                    v_th_l,
                                    v_th_h,
                                    v_hl_l,
                                    v_hl_h,
                                    v_hh_l,
                                    v_hh_h,
                                    ref table_low,
                                    ref table_high,
                                ) = all_tables[base + q_idx];
                                let slice_chunk =
                                    &queued[q_idx][byte_offset..byte_offset + byte_len];

                                let mut ptr_buf = chunk_a.as_mut_ptr() as *mut __m128i;
                                let mut ptr_in = slice_chunk.as_ptr() as *const __m128i;
                                let end = ptr_in.add(blocks_16);

                                while ptr_in < end {
                                    let input = _mm_loadu_si128(ptr_in);
                                    let n0_2 = _mm_and_si128(input, mask_f);
                                    let n1_3 = _mm_and_si128(_mm_srli_epi16(input, 4), mask_f);
                                    let res_lo_even = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_tl_l, n0_2),
                                        _mm_shuffle_epi8(v_tl_h, n1_3),
                                    );
                                    let res_hi_even = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_th_l, n0_2),
                                        _mm_shuffle_epi8(v_th_h, n1_3),
                                    );
                                    let res_lo_odd = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hl_l, n0_2),
                                        _mm_shuffle_epi8(v_hl_h, n1_3),
                                    );
                                    let res_hi_odd = _mm_xor_si128(
                                        _mm_shuffle_epi8(v_hh_l, n0_2),
                                        _mm_shuffle_epi8(v_hh_h, n1_3),
                                    );
                                    let sum_lo =
                                        _mm_xor_si128(res_lo_even, _mm_srli_epi16(res_lo_odd, 8));
                                    let sum_hi =
                                        _mm_xor_si128(res_hi_even, _mm_srli_epi16(res_hi_odd, 8));
                                    let out = _mm_or_si128(
                                        _mm_and_si128(sum_lo, mask_even),
                                        _mm_slli_epi16(sum_hi, 8),
                                    );
                                    let prev = _mm_loadu_si128(ptr_buf);
                                    _mm_storeu_si128(ptr_buf, _mm_xor_si128(prev, out));
                                    ptr_in = ptr_in.add(1);
                                    ptr_buf = ptr_buf.add(1);
                                }

                                if remainder > 0 {
                                    let ow = blocks_16 * 8;
                                    let mut pw = chunk_a[ow..].as_mut_ptr();
                                    let mut p_in = slice_chunk[blocks_16 * 16..].as_ptr();
                                    let tail_end = p_in.add(remainder);
                                    while p_in < tail_end {
                                        let lo = *p_in as usize;
                                        let hi = *p_in.add(1) as usize;
                                        *pw ^= table_low[lo] ^ table_high[hi];
                                        pw = pw.add(1);
                                        p_in = p_in.add(2);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                });
        }
    }

    // ── AArch64 CLMUL path (Phase 31) ─────────────────────────────────────────
    // Uses `pmull.8h` / `pmull2.8h` (base NEON, mandatory on ARMv8-A) instead
    // of the 8-table nibble-shuffle of `flush_neon_work`.  Each 32-byte output
    // block is loaded once; up to 8 source slices ("BATCH") are multiplied via
    // Karatsuba and reduced with Barrett before the result is XORed into the
    // output.  Reduces instruction count ~2.5× vs the shuffle path.
    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn flush_neon_clmul(&mut self) {
        // Altmap and Shuffle2x paths are x86_64-only; on AArch64 drain without processing.
        if !matches!(self.buffers, RecoveryBufferSet::Normal(_)) {
            let queued = std::mem::take(&mut self.queued_slices);
            self.next_index += queued.len();
            self.recycle_queue(queued);
            return;
        }

        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            let buffers = self.buffers.as_normal_mut();
            let logbases = &self.logbases;
            let exponent_start = self.exponent_start;
            let gf = &self.gf;
            let ((), cs) = rayon::join(
                || unsafe {
                    Self::flush_neon_clmul_work(
                        buffers,
                        &queued,
                        start_index,
                        logbases,
                        exponent_start,
                        gf,
                    )
                },
                || queued.par_iter().map(|s| slice_checksum(s)).collect(),
            );
            cs
        } else {
            unsafe {
                Self::flush_neon_clmul_work(
                    self.buffers.as_normal_mut(),
                    &queued,
                    start_index,
                    &self.logbases,
                    self.exponent_start,
                    &self.gf,
                );
            }
            Vec::new()
        };

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    /// Karatsuba multiply + Barrett reduction over GF(2^16)/0x1100B.
    ///
    /// Processes all queued input slices in batches of 8 (BATCH), one recovery
    /// block per rayon task.  For each 32-byte output window the destination is
    /// loaded once, every batch's contribution is XORed in, then stored once.
    ///
    /// Algorithm for the polynomial multiply/reduction ported from ParPar's
    /// `gf16_clmul_neon_base.h` and `gf16_clmul_neon.h` (MIT, © animetosho).
    #[cfg(target_arch = "aarch64")]
    #[target_feature(enable = "neon")]
    pub(super) unsafe fn flush_neon_clmul_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        let n_queued = queued.len();
        let n_rec = buffers.len();

        // Pre-compute (c_lo, c_hi, c_mid = c_lo^c_hi) for every
        // (recovery_block × input_slice) pair.  Layout: coeffs[r*n_queued + q].
        let coeffs: Vec<(u8, u8, u8)> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| {
                let r = flat / n_queued;
                let q = flat % n_queued;
                let exponent = exponent_start + r as u32;
                let logbase = logbases[start_index + q] as u64;
                let log_c = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let c = gf.exp(log_c);
                let lo = (c & 0xFF) as u8;
                let hi = (c >> 8) as u8;
                (lo, hi, lo ^ hi)
            })
            .collect();

        // One rayon task per recovery block: the output buffer (typically ≤ 32 KiB
        // for a 10% recovery set over a 5 GB file) stays hot in L2 across all
        // input batches.
        buffers.par_iter_mut().enumerate().for_each(|(r, buf)| {
            let n_words = buf.len();
            let byte_len = n_words * 2;
            let out_base = buf.as_mut_ptr() as *mut u8;
            let coeffs_r = &coeffs[r * n_queued..(r + 1) * n_queued];

            // Process input slices in batches of BATCH.  The outer loop is
            // over batches so that the broadcasted coefficient registers can be
            // pre-computed once and reused for every output block — matching the
            // structure of ParPar's gf16_clmul_muladd_x.
            const BATCH: usize = 8;

            // SIMD path: 32-byte (16-word) output blocks.
            let n_blocks_32 = byte_len / 32;

            unsafe {
                use std::arch::aarch64::*;

                // pmull.8h: polynomial multiply lower 8 bytes → 8 × u16 products.
                macro_rules! pmull_lo {
                    ($a:expr, $b:expr) => {{
                        let res: poly16x8_t;
                        core::arch::asm!(
                            "pmull {0:v}.8h, {1:v}.8b, {2:v}.8b",
                            out(vreg) res, in(vreg) $a, in(vreg) $b,
                            options(nostack, pure, nomem)
                        );
                        res
                    }};
                }
                // pmull2.8h: same for upper 8 bytes.
                macro_rules! pmull_hi {
                    ($a:expr, $b:expr) => {{
                        let res: poly16x8_t;
                        core::arch::asm!(
                            "pmull2 {0:v}.8h, {1:v}.16b, {2:v}.16b",
                            out(vreg) res, in(vreg) $a, in(vreg) $b,
                            options(nostack, pure, nomem)
                        );
                        res
                    }};
                }
                macro_rules! xorp16 {
                    ($a:expr, $b:expr) => {
                        vreinterpretq_p16_u16(veorq_u16(
                            vreinterpretq_u16_p16($a),
                            vreinterpretq_u16_p16($b),
                        ))
                    };
                }

                let mut q = 0usize;
                while q < n_queued {
                    let batch_end = (q + BATCH).min(n_queued);
                    let batch_size = batch_end - q;

                    // Pre-broadcast coefficients once per batch.
                    // Kept in NEON registers across all output blocks.
                    let mut klo = [vdupq_n_p8(0u8); BATCH];
                    let mut khi = [vdupq_n_p8(0u8); BATCH];
                    let mut kmid = [vdupq_n_p8(0u8); BATCH];
                    for s in 0..batch_size {
                        let (clo, chi, cmid) = coeffs_r[q + s];
                        klo[s] = vdupq_n_p8(clo);
                        khi[s] = vdupq_n_p8(chi);
                        kmid[s] = vdupq_n_p8(cmid);
                    }

                    // Inner loop: all output blocks for this batch.
                    for blk in 0..n_blocks_32 {
                        let out_ptr = out_base.add(blk * 32);
                        let src_off = blk * 32;

                        // First source: initialise the 6 Karatsuba accumulators.
                        let d0 = vld2q_u8(queued[q].as_ptr().add(src_off));
                        let lo0 = vreinterpretq_p8_u8(d0.0);
                        let hi0 = vreinterpretq_p8_u8(d0.1);
                        let mid0 = vreinterpretq_p8_u8(veorq_u8(d0.0, d0.1));
                        let mut acc_l1 = pmull_lo!(lo0, klo[0]);
                        let mut acc_l2 = pmull_hi!(lo0, klo[0]);
                        let mut acc_m1 = pmull_lo!(mid0, kmid[0]);
                        let mut acc_m2 = pmull_hi!(mid0, kmid[0]);
                        let mut acc_h1 = pmull_lo!(hi0, khi[0]);
                        let mut acc_h2 = pmull_hi!(hi0, khi[0]);

                        // Remaining sources in this batch.
                        for s in 1..batch_size {
                            let ds = vld2q_u8(queued[q + s].as_ptr().add(src_off));
                            let lo_s = vreinterpretq_p8_u8(ds.0);
                            let hi_s = vreinterpretq_p8_u8(ds.1);
                            let mid_s = vreinterpretq_p8_u8(veorq_u8(ds.0, ds.1));
                            acc_l1 = xorp16!(acc_l1, pmull_lo!(lo_s, klo[s]));
                            acc_l2 = xorp16!(acc_l2, pmull_hi!(lo_s, klo[s]));
                            acc_m1 = xorp16!(acc_m1, pmull_lo!(mid_s, kmid[s]));
                            acc_m2 = xorp16!(acc_m2, pmull_hi!(mid_s, kmid[s]));
                            acc_h1 = xorp16!(acc_h1, pmull_lo!(hi_s, khi[s]));
                            acc_h2 = xorp16!(acc_h2, pmull_hi!(hi_s, khi[s]));
                        }

                        // Barrett reduction modulo 0x1100B.
                        super::hash::gf16_clmul_reduce_neon(
                            &mut acc_l1,
                            &mut acc_l2,
                            acc_m1,
                            acc_m2,
                            &mut acc_h1,
                            &mut acc_h2,
                        );

                        // Load dst, XOR in batch result, store.
                        let mut dst = vld2q_u8(out_ptr);
                        dst.0 = veorq_u8(dst.0, vreinterpretq_u8_p16(xorp16!(acc_l1, acc_l2)));
                        dst.1 = veorq_u8(dst.1, vreinterpretq_u8_p16(xorp16!(acc_h1, acc_h2)));
                        vst2q_u8(out_ptr, dst);
                    }

                    q = batch_end;
                }
            }

            // Scalar tail: bytes that don't fill a full 32-byte block.
            let tail_bytes = byte_len % 32; // always even (multiple of 2)
            if tail_bytes > 0 {
                let tail_word_start = n_blocks_32 * 16;
                let tail_words = tail_bytes / 2;
                let out_tail = unsafe {
                    std::slice::from_raw_parts_mut(
                        buf.as_mut_ptr().add(tail_word_start),
                        tail_words,
                    )
                };
                for q in 0..n_queued {
                    let (clo, chi, _) = coeffs_r[q];
                    let coeff = (clo as u16) | ((chi as u16) << 8);
                    let src = &queued[q];
                    for (w, dst_word) in out_tail.iter_mut().enumerate() {
                        let bp = (tail_word_start + w) * 2;
                        let word = (src[bp] as u16) | ((src[bp + 1] as u16) << 8);
                        *dst_word ^= gf.mul(word, coeff);
                    }
                }
            }
        });
    }

    #[allow(dead_code)]
    pub(super) fn flush_scalar(&mut self) {
        // This kernel works on Normal-layout buffers only. `flush` no longer
        // routes a specialized layout here (its manual-`SimdPath` override is
        // gated on the layout, and the auto path has a dedicated kernel per
        // layout), so reaching this with ALTMAP/Shuffle2x buffers is a broken
        // invariant. It used to drain the queue unprocessed instead, which is
        // how `--simd scalar` on a Shuffle2x encoder returned zeroed parity
        // with no error.
        assert!(
            matches!(self.buffers, RecoveryBufferSet::Normal(_)),
            "flush_scalar requires Normal recovery buffers"
        );

        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            let buffers = self.buffers.as_normal_mut();
            let logbases = &self.logbases;
            let exponent_start = self.exponent_start;
            let gf = &self.gf;
            let ((), cs) = rayon::join(
                || {
                    Self::flush_scalar_work(
                        buffers,
                        &queued,
                        start_index,
                        logbases,
                        exponent_start,
                        gf,
                    )
                },
                || queued.par_iter().map(|s| slice_checksum(s)).collect(),
            );
            cs
        } else {
            Self::flush_scalar_work(
                self.buffers.as_normal_mut(),
                &queued,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
            Vec::new()
        };

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[allow(dead_code)]
    pub(super) fn flush_scalar_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        buffers.par_iter_mut().enumerate().for_each(|(i, buffer)| {
            let exponent = exponent_start + i as u32;

            let mut tables = Vec::with_capacity(queued.len());
            for (q_idx, _) in queued.iter().enumerate() {
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255 {
                    table_low[b as usize] = gf.mul(b as u16, coeff);
                    table_high[b as usize] = gf.mul((b as u16) << 8, coeff);
                }
                tables.push((table_low, table_high));
            }

            let chunk_size = 16384;
            for (chunk_idx, buffer_chunk) in buffer.chunks_mut(chunk_size).enumerate() {
                let byte_offset = chunk_idx * chunk_size * 2;
                let byte_len = buffer_chunk.len() * 2;

                for (q_idx, slice) in queued.iter().enumerate() {
                    let slice_chunk = &slice[byte_offset..byte_offset + byte_len];
                    let (ref table_low, ref table_high) = tables[q_idx];

                    for (word, chunk) in buffer_chunk.iter_mut().zip(slice_chunk.as_chunks::<2>().0)
                    {
                        *word ^= table_low[chunk[0] as usize] ^ table_high[chunk[1] as usize];
                    }
                }
            }
        });
    }
}
