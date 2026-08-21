use super::*;

impl RecoveryEncoder {
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn flush_avx2(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        unsafe {
            Self::flush_avx2_work(
                self.buffers.as_normal_mut(),
                &queued,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
        }

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn flush_avx2_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        let mask_f = _mm256_set1_epi8(0x0F);
        let mask_even = _mm256_set1_epi16(0x00FF);

        let n_rec = buffers.len();
        let n_queued = queued.len();

        // Pre-build all SIMD coefficient tables in a single parallel pass — one Vec
        // entry per (recovery_block × input_slice) pair, laid out as [rec * n_queued + q].
        //
        // Building tables outside the chunk loop means they are computed once per flush
        // rather than once per (flush × chunk). The chunk loop below can then reference
        // pre-built tables without any GF-table lookups in the hot path.
        //
        // __m256i is Send+Sync (it is [i64; 4] under the hood) so storing it in a Vec
        // that is shared across rayon tasks is safe.
        let all_tables: Vec<Avx2Table> = (0..n_rec * n_queued)
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

                let v_tl_l =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(tl_l.as_ptr() as *const __m128i));
                let v_tl_h =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(tl_h.as_ptr() as *const __m128i));
                let v_th_l =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(th_l.as_ptr() as *const __m128i));
                let v_th_h =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(th_h.as_ptr() as *const __m128i));
                let v_hl_l =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(hl_l.as_ptr() as *const __m128i));
                let v_hl_h =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(hl_h.as_ptr() as *const __m128i));
                let v_hh_l =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(hh_l.as_ptr() as *const __m128i));
                let v_hh_h =
                    _mm256_broadcastsi128_si256(_mm_loadu_si128(hh_h.as_ptr() as *const __m128i));

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

        // 2D parallel loop: outer dimension = recovery block pairs (91 tasks for 183
        // blocks), inner dimension = 32 KiB chunks of each recovery buffer (960 chunks
        // for a 30 MiB slice). Total rayon tasks = 91 × 960 = ~87 K, saturating all
        // available cores instead of the previous 91-task ceiling.
        //
        // Each rayon task handles a group of consecutive recovery blocks (4× unrolling
        // over the recovery dimension). One input load + one nibble decomposition serves
        // all blocks in the group, halving the load and AND/SRL overhead per byte processed.
        let chunk_size = 16384usize; // 32 KiB recovery buffer chunk (see avx2_gfni A/B notes)

        buffers
            .par_chunks_mut(4)
            .enumerate()
            .for_each(|(group_idx, buf_group)| {
                let i = group_idx * 4;
                match buf_group {
                    [buf_a, buf_b, buf_c, buf_d] => {
                        // 4× unrolled: four recovery blocks share one input load.
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        let base_c = (i + 2) * n_queued;
                        let base_d = (i + 3) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .zip(buf_c.par_chunks_mut(chunk_size))
                            .zip(buf_d.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(
                                |(chunk_idx, (((chunk_a, chunk_b), chunk_c), chunk_d))| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk_a.len() * 2;
                                    let blocks_32 = byte_len / 32;
                                    let remainder = byte_len % 32;

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
                                        let (
                                            v_tl_l_c,
                                            v_tl_h_c,
                                            v_th_l_c,
                                            v_th_h_c,
                                            v_hl_l_c,
                                            v_hl_h_c,
                                            v_hh_l_c,
                                            v_hh_h_c,
                                            ref tlow_c,
                                            ref thigh_c,
                                        ) = all_tables[base_c + q_idx];
                                        let (
                                            v_tl_l_d,
                                            v_tl_h_d,
                                            v_th_l_d,
                                            v_th_h_d,
                                            v_hl_l_d,
                                            v_hl_h_d,
                                            v_hh_l_d,
                                            v_hh_h_d,
                                            ref tlow_d,
                                            ref thigh_d,
                                        ) = all_tables[base_d + q_idx];

                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_c = chunk_c.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_d = chunk_d.as_mut_ptr() as *mut __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let n0_2 = _mm256_and_si256(input, mask_f);
                                            let n1_3 = _mm256_and_si256(
                                                _mm256_srli_epi16(input, 4),
                                                mask_f,
                                            );

                                            // Block A
                                            let rle_a = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l_a, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h_a, n1_3),
                                            );
                                            let rhe_a = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l_a, n0_2),
                                                _mm256_shuffle_epi8(v_th_h_a, n1_3),
                                            );
                                            let rlo_a = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l_a, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h_a, n1_3),
                                            );
                                            let rho_a = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l_a, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h_a, n1_3),
                                            );
                                            let out_a = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        rle_a,
                                                        _mm256_srli_epi16(rlo_a, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        rhe_a,
                                                        _mm256_srli_epi16(rho_a, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_a,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_a), out_a),
                                            );

                                            // Block B
                                            let rle_b = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l_b, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h_b, n1_3),
                                            );
                                            let rhe_b = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l_b, n0_2),
                                                _mm256_shuffle_epi8(v_th_h_b, n1_3),
                                            );
                                            let rlo_b = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l_b, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h_b, n1_3),
                                            );
                                            let rho_b = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l_b, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h_b, n1_3),
                                            );
                                            let out_b = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        rle_b,
                                                        _mm256_srli_epi16(rlo_b, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        rhe_b,
                                                        _mm256_srli_epi16(rho_b, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_b,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_b), out_b),
                                            );

                                            // Block C
                                            let rle_c = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l_c, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h_c, n1_3),
                                            );
                                            let rhe_c = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l_c, n0_2),
                                                _mm256_shuffle_epi8(v_th_h_c, n1_3),
                                            );
                                            let rlo_c = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l_c, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h_c, n1_3),
                                            );
                                            let rho_c = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l_c, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h_c, n1_3),
                                            );
                                            let out_c = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        rle_c,
                                                        _mm256_srli_epi16(rlo_c, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        rhe_c,
                                                        _mm256_srli_epi16(rho_c, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_c,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_c), out_c),
                                            );

                                            // Block D
                                            let rle_d = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l_d, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h_d, n1_3),
                                            );
                                            let rhe_d = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l_d, n0_2),
                                                _mm256_shuffle_epi8(v_th_h_d, n1_3),
                                            );
                                            let rlo_d = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l_d, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h_d, n1_3),
                                            );
                                            let rho_d = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l_d, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h_d, n1_3),
                                            );
                                            let out_d = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        rle_d,
                                                        _mm256_srli_epi16(rlo_d, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        rhe_d,
                                                        _mm256_srli_epi16(rho_d, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_d,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_d), out_d),
                                            );

                                            ptr_in = ptr_in.add(1);
                                            ptr_a = ptr_a.add(1);
                                            ptr_b = ptr_b.add(1);
                                            ptr_c = ptr_c.add(1);
                                            ptr_d = ptr_d.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_32 * 16;
                                            let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                            let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                            let mut pw_c = chunk_c[ow..].as_mut_ptr();
                                            let mut pw_d = chunk_d[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
                                            let tail_end = p_in.add(remainder);
                                            while p_in < tail_end {
                                                let lo = *p_in as usize;
                                                let hi = *p_in.add(1) as usize;
                                                *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                                *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                                *pw_c ^= tlow_c[lo] ^ thigh_c[hi];
                                                *pw_d ^= tlow_d[lo] ^ thigh_d[hi];
                                                pw_a = pw_a.add(1);
                                                pw_b = pw_b.add(1);
                                                pw_c = pw_c.add(1);
                                                pw_d = pw_d.add(1);
                                                p_in = p_in.add(2);
                                            }
                                        }
                                    }
                                },
                            );
                    }
                    [buf_a, buf_b] => {
                        // Fallback for 2 blocks (remains 2× unrolled).
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(|(chunk_idx, (chunk_a, chunk_b))| unsafe {
                                let byte_offset = chunk_idx * chunk_size * 2;
                                let byte_len = chunk_a.len() * 2;
                                let blocks_32 = byte_len / 32;
                                let remainder = byte_len % 32;

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

                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                    let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                    let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                    let end = ptr_in.add(blocks_32);

                                    while ptr_in < end {
                                        _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                        let input = _mm256_loadu_si256(ptr_in);
                                        let n0_2 = _mm256_and_si256(input, mask_f);
                                        let n1_3 =
                                            _mm256_and_si256(_mm256_srli_epi16(input, 4), mask_f);

                                        let rle_a = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_tl_l_a, n0_2),
                                            _mm256_shuffle_epi8(v_tl_h_a, n1_3),
                                        );
                                        let rhe_a = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_th_l_a, n0_2),
                                            _mm256_shuffle_epi8(v_th_h_a, n1_3),
                                        );
                                        let rlo_a = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_hl_l_a, n0_2),
                                            _mm256_shuffle_epi8(v_hl_h_a, n1_3),
                                        );
                                        let rho_a = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_hh_l_a, n0_2),
                                            _mm256_shuffle_epi8(v_hh_h_a, n1_3),
                                        );
                                        let out_a = _mm256_or_si256(
                                            _mm256_and_si256(
                                                _mm256_xor_si256(
                                                    rle_a,
                                                    _mm256_srli_epi16(rlo_a, 8),
                                                ),
                                                mask_even,
                                            ),
                                            _mm256_slli_epi16(
                                                _mm256_xor_si256(
                                                    rhe_a,
                                                    _mm256_srli_epi16(rho_a, 8),
                                                ),
                                                8,
                                            ),
                                        );
                                        _mm256_storeu_si256(
                                            ptr_a,
                                            _mm256_xor_si256(_mm256_loadu_si256(ptr_a), out_a),
                                        );

                                        let rle_b = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_tl_l_b, n0_2),
                                            _mm256_shuffle_epi8(v_tl_h_b, n1_3),
                                        );
                                        let rhe_b = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_th_l_b, n0_2),
                                            _mm256_shuffle_epi8(v_th_h_b, n1_3),
                                        );
                                        let rlo_b = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_hl_l_b, n0_2),
                                            _mm256_shuffle_epi8(v_hl_h_b, n1_3),
                                        );
                                        let rho_b = _mm256_xor_si256(
                                            _mm256_shuffle_epi8(v_hh_l_b, n0_2),
                                            _mm256_shuffle_epi8(v_hh_h_b, n1_3),
                                        );
                                        let out_b = _mm256_or_si256(
                                            _mm256_and_si256(
                                                _mm256_xor_si256(
                                                    rle_b,
                                                    _mm256_srli_epi16(rlo_b, 8),
                                                ),
                                                mask_even,
                                            ),
                                            _mm256_slli_epi16(
                                                _mm256_xor_si256(
                                                    rhe_b,
                                                    _mm256_srli_epi16(rho_b, 8),
                                                ),
                                                8,
                                            ),
                                        );
                                        _mm256_storeu_si256(
                                            ptr_b,
                                            _mm256_xor_si256(_mm256_loadu_si256(ptr_b), out_b),
                                        );

                                        ptr_in = ptr_in.add(1);
                                        ptr_a = ptr_a.add(1);
                                        ptr_b = ptr_b.add(1);
                                    }

                                    if remainder > 0 {
                                        let ow = blocks_32 * 16;
                                        let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                        let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                        let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
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
                            });
                    }
                    rest => {
                        // Fallback for remaining 1 or 3 blocks (scalar for SIMD simplicity here).
                        for (j, buf) in rest.iter_mut().enumerate() {
                            let base = (i + j) * n_queued;
                            buf.par_chunks_mut(chunk_size).enumerate().for_each(
                                |(chunk_idx, chunk)| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk.len() * 2;
                                    let blocks_32 = byte_len / 32;
                                    let remainder = byte_len % 32;

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

                                        let mut ptr_buf = chunk.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let n0_2 = _mm256_and_si256(input, mask_f);
                                            let n1_3 = _mm256_and_si256(
                                                _mm256_srli_epi16(input, 4),
                                                mask_f,
                                            );
                                            let res_lo_even = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_tl_l, n0_2),
                                                _mm256_shuffle_epi8(v_tl_h, n1_3),
                                            );
                                            let res_hi_even = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_th_l, n0_2),
                                                _mm256_shuffle_epi8(v_th_h, n1_3),
                                            );
                                            let res_lo_odd = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hl_l, n0_2),
                                                _mm256_shuffle_epi8(v_hl_h, n1_3),
                                            );
                                            let res_hi_odd = _mm256_xor_si256(
                                                _mm256_shuffle_epi8(v_hh_l, n0_2),
                                                _mm256_shuffle_epi8(v_hh_h, n1_3),
                                            );
                                            let out = _mm256_or_si256(
                                                _mm256_and_si256(
                                                    _mm256_xor_si256(
                                                        res_lo_even,
                                                        _mm256_srli_epi16(res_lo_odd, 8),
                                                    ),
                                                    mask_even,
                                                ),
                                                _mm256_slli_epi16(
                                                    _mm256_xor_si256(
                                                        res_hi_even,
                                                        _mm256_srli_epi16(res_hi_odd, 8),
                                                    ),
                                                    8,
                                                ),
                                            );
                                            _mm256_storeu_si256(
                                                ptr_buf,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_buf), out),
                                            );
                                            ptr_in = ptr_in.add(1);
                                            ptr_buf = ptr_buf.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_32 * 16;
                                            let mut pw = chunk[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
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
                                },
                            );
                        }
                    }
                }
            });
    }

    /// AVX-512 BW nibble-shuffle on Normal `u16` buffers (parpar Shuffle AVX-512).
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512bw")]
    pub(super) unsafe fn flush_avx512_shuffle(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();
        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };
        unsafe {
            Self::flush_avx512_shuffle_work(
                self.buffers.as_normal_mut(),
                &queued,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
        }
        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512bw")]
    pub(super) unsafe fn flush_avx512_shuffle_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        let mask_f = _mm512_set1_epi8(0x0F_u8 as i8);
        let mask_even = _mm512_set1_epi16(0x00FF_u16 as i16);
        let n_rec = buffers.len();
        let n_queued = queued.len();
        if n_queued == 0 {
            return;
        }
        let all_tables: Vec<Avx512ShuffleTable> = (0..n_rec * n_queued)
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
                let bcast =
                    |t: &[u8; 16]| _mm512_broadcast_i32x4(_mm_loadu_si128(t.as_ptr().cast()));
                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }
                (
                    bcast(&tl_l),
                    bcast(&tl_h),
                    bcast(&th_l),
                    bcast(&th_h),
                    bcast(&hl_l),
                    bcast(&hl_h),
                    bcast(&hh_l),
                    bcast(&hh_h),
                    table_low,
                    table_high,
                )
            })
            .collect();

        buffers.par_iter_mut().enumerate().for_each(|(rec, buf)| {
            let base = rec * n_queued;
            let byte_len = buf.len() * 2;
            let blocks = byte_len / 64;
            let remainder = byte_len % 64;
            for q_idx in 0..n_queued {
                let (tl_l, tl_h, th_l, th_h, hl_l, hl_h, hh_l, hh_h, ref tlow, ref thigh) =
                    all_tables[base + q_idx];
                let slice = &queued[q_idx][..byte_len];
                unsafe {
                    let mut pin = slice.as_ptr().cast::<__m512i>();
                    let mut pout = buf.as_mut_ptr().cast::<__m512i>();
                    for _ in 0..blocks {
                        let input = _mm512_loadu_si512(pin);
                        let n0 = _mm512_and_si512(input, mask_f);
                        let n1 = _mm512_and_si512(_mm512_srli_epi16(input, 4), mask_f);
                        let rle = _mm512_xor_si512(
                            _mm512_shuffle_epi8(tl_l, n0),
                            _mm512_shuffle_epi8(tl_h, n1),
                        );
                        let rhe = _mm512_xor_si512(
                            _mm512_shuffle_epi8(th_l, n0),
                            _mm512_shuffle_epi8(th_h, n1),
                        );
                        let rlo = _mm512_xor_si512(
                            _mm512_shuffle_epi8(hl_l, n0),
                            _mm512_shuffle_epi8(hl_h, n1),
                        );
                        let rho = _mm512_xor_si512(
                            _mm512_shuffle_epi8(hh_l, n0),
                            _mm512_shuffle_epi8(hh_h, n1),
                        );
                        let out = _mm512_or_si512(
                            _mm512_and_si512(
                                _mm512_xor_si512(rle, _mm512_srli_epi16(rlo, 8)),
                                mask_even,
                            ),
                            _mm512_slli_epi16(_mm512_xor_si512(rhe, _mm512_srli_epi16(rho, 8)), 8),
                        );
                        _mm512_storeu_si512(pout, _mm512_xor_si512(_mm512_loadu_si512(pout), out));
                        pin = pin.add(1);
                        pout = pout.add(1);
                    }
                    if remainder > 0 {
                        let ow = blocks * 32;
                        let mut pw = buf[ow..].as_mut_ptr();
                        let mut p_in = slice[blocks * 64..].as_ptr();
                        let end = p_in.add(remainder);
                        while p_in < end {
                            let lo = *p_in as usize;
                            let hi = *p_in.add(1) as usize;
                            *pw ^= tlow[lo] ^ thigh[hi];
                            pw = pw.add(1);
                            p_in = p_in.add(2);
                        }
                    }
                }
            }
        });
    }

    /// AVX2 Shuffle2x flush: accumulates queued slices into Shuffle2x recovery buffers.
    ///
    /// Input slices are in normal u16 layout. Recovery buffers are in Shuffle2x layout
    /// (lo-bytes in lane 0, hi-bytes in lane 1 of each 32-byte chunk). Uses 4 PSHUFB
    /// per recovery block per 32-byte input chunk instead of the 8 used by the plain
    /// AVX2 nibble-shuffle path, achieving ~33% fewer instructions per block.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn flush_avx2_shuffle2x(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        let RecoveryBufferSet::Shuffle2x(ref mut bufs) = self.buffers else {
            unreachable!("flush_avx2_shuffle2x called on non-Shuffle2x encoder");
        };

        unsafe {
            Self::flush_avx2_shuffle2x_work(
                bufs,
                &queued,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
        }

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    /// Shuffle2x madd of one prepared 32-byte vector into a recovery buffer.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn s2x_contrib(
        s: __m256i,
        mask_f: __m256i,
        tna: __m256i,
        tnb: __m256i,
        tsa: __m256i,
        tsb: __m256i,
    ) -> __m256i {
        let sw = _mm256_permute2x128_si256(s, s, 0x01);
        let lo_nib_s = _mm256_and_si256(s, mask_f);
        let hi_nib_s = _mm256_and_si256(_mm256_srli_epi16(s, 4), mask_f);
        let lo_nib_sw = _mm256_and_si256(sw, mask_f);
        let hi_nib_sw = _mm256_and_si256(_mm256_srli_epi16(sw, 4), mask_f);
        let norm = _mm256_xor_si256(
            _mm256_shuffle_epi8(tna, lo_nib_s),
            _mm256_shuffle_epi8(tnb, hi_nib_s),
        );
        let swap = _mm256_xor_si256(
            _mm256_shuffle_epi8(tsa, lo_nib_sw),
            _mm256_shuffle_epi8(tsb, hi_nib_sw),
        );
        _mm256_xor_si256(norm, swap)
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn flush_avx2_shuffle2x_work(
        buffers: &mut [Vec<u8>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        let mask_f = _mm256_set1_epi8(0x0F_u8 as i8);

        let n_rec = buffers.len();
        let n_queued = queued.len();

        // Pre-build 4-register Shuffle2x coefficient tables in parallel.
        // For coefficient c, 4-bit nibble index n (0..15):
        //   loNk[n] = (gf.mul(n << 4k, c) & 0xFF) as u8
        //   hiNk[n] = (gf.mul(n << 4k, c) >> 8) as u8
        // Table layout (each __m256i packs two 128-bit sub-tables into its two lanes):
        //   tNormA: lane0 = loN0, lane1 = hiN2
        //   tNormB: lane0 = loN1, lane1 = hiN3
        //   tSwapA: lane0 = loN2, lane1 = hiN0
        //   tSwapB: lane0 = loN3, lane1 = hiN1
        // Bounds check: ensure start_index + q_idx is within logbases
        if start_index + n_queued > logbases.len() {
            panic!(
                "PAR2 slice index overflow: start_index({}) + n_queued({}) > logbases.len({})",
                start_index,
                n_queued,
                logbases.len()
            );
        }

        let all_tables: Vec<Avx2Shuffle2xTable> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| unsafe {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut lo_n0 = [0u8; 16];
                let mut lo_n1 = [0u8; 16];
                let mut lo_n2 = [0u8; 16];
                let mut lo_n3 = [0u8; 16];
                let mut hi_n0 = [0u8; 16];
                let mut hi_n1 = [0u8; 16];
                let mut hi_n2 = [0u8; 16];
                let mut hi_n3 = [0u8; 16];

                for n in 0..16usize {
                    let r0 = gf.mul(n as u16, coeff);
                    lo_n0[n] = (r0 & 0xFF) as u8;
                    hi_n0[n] = (r0 >> 8) as u8;
                    let r1 = gf.mul((n as u16) << 4, coeff);
                    lo_n1[n] = (r1 & 0xFF) as u8;
                    hi_n1[n] = (r1 >> 8) as u8;
                    let r2 = gf.mul((n as u16) << 8, coeff);
                    lo_n2[n] = (r2 & 0xFF) as u8;
                    hi_n2[n] = (r2 >> 8) as u8;
                    let r3 = gf.mul((n as u16) << 12, coeff);
                    lo_n3[n] = (r3 & 0xFF) as u8;
                    hi_n3[n] = (r3 >> 8) as u8;
                }

                let t_norm_a = _mm256_set_m128i(
                    _mm_loadu_si128(hi_n2.as_ptr() as *const __m128i),
                    _mm_loadu_si128(lo_n0.as_ptr() as *const __m128i),
                );
                let t_norm_b = _mm256_set_m128i(
                    _mm_loadu_si128(hi_n3.as_ptr() as *const __m128i),
                    _mm_loadu_si128(lo_n1.as_ptr() as *const __m128i),
                );
                let t_swap_a = _mm256_set_m128i(
                    _mm_loadu_si128(hi_n0.as_ptr() as *const __m128i),
                    _mm_loadu_si128(lo_n2.as_ptr() as *const __m128i),
                );
                let t_swap_b = _mm256_set_m128i(
                    _mm_loadu_si128(hi_n1.as_ptr() as *const __m128i),
                    _mm_loadu_si128(lo_n3.as_ptr() as *const __m128i),
                );

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                (
                    t_norm_a, t_norm_b, t_swap_a, t_swap_b, table_low, table_high,
                )
            })
            .collect();

        // Convert each input once (Normal → Shuffle2x). The inner loop used to
        // vpshufb+vpermq every 32-byte block for every recovery group.
        let prepared: Vec<Vec<u8>> = queued
            .par_iter()
            .map(|s| {
                let mut out = vec![0u8; s.len()];
                crate::shuffle2x::to_shuffle2x(s, &mut out);
                out
            })
            .collect();

        // 8 KiB tiles — ParPar's Shuffle2x idealChunkSize. 32 KiB was oversized
        // for L2 when several recovery buffers share the same input stream.
        let chunk_size_bytes = 8192usize;

        buffers
            .par_chunks_mut(4)
            .enumerate()
            .for_each(|(group_idx, buf_group)| {
                let i = group_idx * 4;
                match buf_group {
                    [buf_a, buf_b, buf_c, buf_d] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        let base_c = (i + 2) * n_queued;
                        let base_d = (i + 3) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size_bytes)
                            .zip(buf_b.par_chunks_mut(chunk_size_bytes))
                            .zip(buf_c.par_chunks_mut(chunk_size_bytes))
                            .zip(buf_d.par_chunks_mut(chunk_size_bytes))
                            .enumerate()
                            .for_each(
                                |(chunk_idx, (((chunk_a, chunk_b), chunk_c), chunk_d))| unsafe {
                                    let byte_offset = chunk_idx * chunk_size_bytes;
                                    let byte_len = chunk_a.len();
                                    let blocks_32 = byte_len / 32;

                                    let mut q_idx = 0usize;
                                    while q_idx < n_queued {
                                        let pair = q_idx + 1 < n_queued;
                                        let (tna_a, tnb_a, tsa_a, tsb_a, _, _) =
                                            all_tables[base_a + q_idx];
                                        let (tna_b, tnb_b, tsa_b, tsb_b, _, _) =
                                            all_tables[base_b + q_idx];
                                        let (tna_c, tnb_c, tsa_c, tsb_c, _, _) =
                                            all_tables[base_c + q_idx];
                                        let (tna_d, tnb_d, tsa_d, tsb_d, _, _) =
                                            all_tables[base_d + q_idx];

                                        let chunk0 =
                                            &prepared[q_idx][byte_offset..byte_offset + byte_len];
                                        let mut ptr0 = chunk0.as_ptr() as *const __m256i;
                                        let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_c = chunk_c.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_d = chunk_d.as_mut_ptr() as *mut __m256i;
                                        let end = ptr0.add(blocks_32);

                                        if pair {
                                            let (tna_a2, tnb_a2, tsa_a2, tsb_a2, _, _) =
                                                all_tables[base_a + q_idx + 1];
                                            let (tna_b2, tnb_b2, tsa_b2, tsb_b2, _, _) =
                                                all_tables[base_b + q_idx + 1];
                                            let (tna_c2, tnb_c2, tsa_c2, tsb_c2, _, _) =
                                                all_tables[base_c + q_idx + 1];
                                            let (tna_d2, tnb_d2, tsa_d2, tsb_d2, _, _) =
                                                all_tables[base_d + q_idx + 1];
                                            let chunk1 = &prepared[q_idx + 1]
                                                [byte_offset..byte_offset + byte_len];
                                            let mut ptr1 = chunk1.as_ptr() as *const __m256i;
                                            while ptr0 < end {
                                                _mm_prefetch(ptr0.add(4) as *const i8, _MM_HINT_T0);
                                                let s0 = _mm256_loadu_si256(ptr0);
                                                let s1 = _mm256_loadu_si256(ptr1);
                                                let c0 = RecoveryEncoder::s2x_contrib(
                                                    s0, mask_f, tna_a, tnb_a, tsa_a, tsb_a,
                                                );
                                                let c1 = RecoveryEncoder::s2x_contrib(
                                                    s1, mask_f, tna_a2, tnb_a2, tsa_a2, tsb_a2,
                                                );
                                                _mm256_storeu_si256(
                                                    ptr_a,
                                                    _mm256_xor_si256(
                                                        _mm256_loadu_si256(ptr_a),
                                                        _mm256_xor_si256(c0, c1),
                                                    ),
                                                );
                                                let c0 = RecoveryEncoder::s2x_contrib(
                                                    s0, mask_f, tna_b, tnb_b, tsa_b, tsb_b,
                                                );
                                                let c1 = RecoveryEncoder::s2x_contrib(
                                                    s1, mask_f, tna_b2, tnb_b2, tsa_b2, tsb_b2,
                                                );
                                                _mm256_storeu_si256(
                                                    ptr_b,
                                                    _mm256_xor_si256(
                                                        _mm256_loadu_si256(ptr_b),
                                                        _mm256_xor_si256(c0, c1),
                                                    ),
                                                );
                                                let c0 = RecoveryEncoder::s2x_contrib(
                                                    s0, mask_f, tna_c, tnb_c, tsa_c, tsb_c,
                                                );
                                                let c1 = RecoveryEncoder::s2x_contrib(
                                                    s1, mask_f, tna_c2, tnb_c2, tsa_c2, tsb_c2,
                                                );
                                                _mm256_storeu_si256(
                                                    ptr_c,
                                                    _mm256_xor_si256(
                                                        _mm256_loadu_si256(ptr_c),
                                                        _mm256_xor_si256(c0, c1),
                                                    ),
                                                );
                                                let c0 = RecoveryEncoder::s2x_contrib(
                                                    s0, mask_f, tna_d, tnb_d, tsa_d, tsb_d,
                                                );
                                                let c1 = RecoveryEncoder::s2x_contrib(
                                                    s1, mask_f, tna_d2, tnb_d2, tsa_d2, tsb_d2,
                                                );
                                                _mm256_storeu_si256(
                                                    ptr_d,
                                                    _mm256_xor_si256(
                                                        _mm256_loadu_si256(ptr_d),
                                                        _mm256_xor_si256(c0, c1),
                                                    ),
                                                );
                                                ptr0 = ptr0.add(1);
                                                ptr1 = ptr1.add(1);
                                                ptr_a = ptr_a.add(1);
                                                ptr_b = ptr_b.add(1);
                                                ptr_c = ptr_c.add(1);
                                                ptr_d = ptr_d.add(1);
                                            }
                                            q_idx += 2;
                                        } else {
                                            while ptr0 < end {
                                                _mm_prefetch(ptr0.add(4) as *const i8, _MM_HINT_T0);
                                                let s0 = _mm256_loadu_si256(ptr0);
                                                let c0 = RecoveryEncoder::s2x_contrib(
                                                    s0, mask_f, tna_a, tnb_a, tsa_a, tsb_a,
                                                );
                                                _mm256_storeu_si256(
                                                    ptr_a,
                                                    _mm256_xor_si256(_mm256_loadu_si256(ptr_a), c0),
                                                );
                                                let c0 = RecoveryEncoder::s2x_contrib(
                                                    s0, mask_f, tna_b, tnb_b, tsa_b, tsb_b,
                                                );
                                                _mm256_storeu_si256(
                                                    ptr_b,
                                                    _mm256_xor_si256(_mm256_loadu_si256(ptr_b), c0),
                                                );
                                                let c0 = RecoveryEncoder::s2x_contrib(
                                                    s0, mask_f, tna_c, tnb_c, tsa_c, tsb_c,
                                                );
                                                _mm256_storeu_si256(
                                                    ptr_c,
                                                    _mm256_xor_si256(_mm256_loadu_si256(ptr_c), c0),
                                                );
                                                let c0 = RecoveryEncoder::s2x_contrib(
                                                    s0, mask_f, tna_d, tnb_d, tsa_d, tsb_d,
                                                );
                                                _mm256_storeu_si256(
                                                    ptr_d,
                                                    _mm256_xor_si256(_mm256_loadu_si256(ptr_d), c0),
                                                );
                                                ptr0 = ptr0.add(1);
                                                ptr_a = ptr_a.add(1);
                                                ptr_b = ptr_b.add(1);
                                                ptr_c = ptr_c.add(1);
                                                ptr_d = ptr_d.add(1);
                                            }
                                            q_idx += 1;
                                        }
                                    }
                                },
                            );
                    }
                    [buf_a, buf_b] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size_bytes)
                            .zip(buf_b.par_chunks_mut(chunk_size_bytes))
                            .enumerate()
                            .for_each(|(chunk_idx, (chunk_a, chunk_b))| unsafe {
                                let byte_offset = chunk_idx * chunk_size_bytes;
                                let byte_len = chunk_a.len();
                                let blocks_32 = byte_len / 32;

                                let mut q_idx = 0usize;
                                while q_idx < n_queued {
                                    let (tna_a, tnb_a, tsa_a, tsb_a, _, _) =
                                        all_tables[base_a + q_idx];
                                    let (tna_b, tnb_b, tsa_b, tsb_b, _, _) =
                                        all_tables[base_b + q_idx];
                                    let slice_chunk =
                                        &prepared[q_idx][byte_offset..byte_offset + byte_len];
                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                    let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                    let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                    let end = ptr_in.add(blocks_32);
                                    while ptr_in < end {
                                        _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                        let s = _mm256_loadu_si256(ptr_in);
                                        let c = RecoveryEncoder::s2x_contrib(
                                            s, mask_f, tna_a, tnb_a, tsa_a, tsb_a,
                                        );
                                        _mm256_storeu_si256(
                                            ptr_a,
                                            _mm256_xor_si256(_mm256_loadu_si256(ptr_a), c),
                                        );
                                        let c = RecoveryEncoder::s2x_contrib(
                                            s, mask_f, tna_b, tnb_b, tsa_b, tsb_b,
                                        );
                                        _mm256_storeu_si256(
                                            ptr_b,
                                            _mm256_xor_si256(_mm256_loadu_si256(ptr_b), c),
                                        );
                                        ptr_in = ptr_in.add(1);
                                        ptr_a = ptr_a.add(1);
                                        ptr_b = ptr_b.add(1);
                                    }
                                    q_idx += 1;
                                }
                            });
                    }
                    rest => {
                        for (j, buf) in rest.iter_mut().enumerate() {
                            let base = (i + j) * n_queued;
                            buf.par_chunks_mut(chunk_size_bytes).enumerate().for_each(
                                |(chunk_idx, chunk)| unsafe {
                                    let byte_offset = chunk_idx * chunk_size_bytes;
                                    let byte_len = chunk.len();
                                    let blocks_32 = byte_len / 32;

                                    for q_idx in 0..n_queued {
                                        let (tna, tnb, tsa, tsb, _, _) = all_tables[base + q_idx];
                                        let slice_chunk =
                                            &prepared[q_idx][byte_offset..byte_offset + byte_len];
                                        let mut ptr_buf = chunk.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let end = ptr_in.add(blocks_32);
                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let s = _mm256_loadu_si256(ptr_in);
                                            let c = RecoveryEncoder::s2x_contrib(
                                                s, mask_f, tna, tnb, tsa, tsb,
                                            );
                                            _mm256_storeu_si256(
                                                ptr_buf,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_buf), c),
                                            );
                                            ptr_in = ptr_in.add(1);
                                            ptr_buf = ptr_buf.add(1);
                                        }
                                    }
                                },
                            );
                        }
                    }
                }
            });
    }

    /// Affine2x + GFNI: two `gf2p8affine` + 64-bit swap, fused `srcCount=6`,
    /// 4 KiB tiles. Inputs are prepared once (Normal → Affine2x).
    /// ALTMAP XOR bit-dependency kernel (Phase 27e).
    ///
    /// Transposes each queued raw slice into ALTMAP layout, then applies the
    /// pre-built dep-matrix table via `vpxor` — one 256-bit vector per
    /// (output-plane, vec-index) pair.  4-way unroll over recovery blocks.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    pub(super) unsafe fn flush_avx2_altmap(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        let slice_words = self.slice_words;
        let altmap_slices: Vec<Vec<u8>> = queued
            .par_iter()
            .map(|s| {
                let mut am = vec![0u8; super::altmap::altmap_size(slice_words)];
                // SAFETY: slice_size is always even; s is exactly slice_size bytes.
                let words =
                    unsafe { std::slice::from_raw_parts(s.as_ptr().cast::<u16>(), slice_words) };
                super::altmap::to_altmap(words, &mut am);
                am
            })
            .collect();

        let dep_tables = self
            .dep_tables
            .as_deref()
            .expect("dep_tables must be built for ALTMAP path");

        let buffers = match &mut self.buffers {
            RecoveryBufferSet::Altmap(b) => b.as_mut_slice(),
            _ => panic!("flush_avx2_altmap called on non-ALTMAP encoder"),
        };

        unsafe {
            Self::flush_avx2_altmap_work(
                buffers,
                &altmap_slices,
                start_index,
                &self.logbases,
                self.exponent_start,
                dep_tables,
                &self.gf,
            );
        }

        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    /// Static worker for [`flush_avx2_altmap`].
    ///
    /// `buffers`: one `Vec<u8>` per recovery block (ALTMAP layout).
    /// `queued`:  one `Vec<u8>` per input slice (already in ALTMAP layout).
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2")]
    #[allow(clippy::needless_range_loop)]
    pub(super) unsafe fn flush_avx2_altmap_work(
        buffers: &mut [Vec<u8>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        dep_tables: &[[u16; 16]; 65536],
        gf: &Gf16,
    ) {
        use std::arch::x86_64::*;

        let n_rec = buffers.len();
        if n_rec == 0 || queued.is_empty() {
            return;
        }

        // plane_bytes = slice_words / 8.  ALTMAP invariant: buf.len() == plane_bytes * 16.
        let plane_bytes = buffers[0].len() / 16;
        let n_vec = plane_bytes / 32; // full 256-bit vectors per plane section

        // Process 4 recovery blocks at a time.
        buffers
            .par_chunks_mut(4)
            .enumerate()
            .for_each(|(chunk_idx, rec_chunk)| {
                let rec_base = chunk_idx * 4;
                let chunk_len = rec_chunk.len(); // 1..=4

                for (q, slice_am) in queued.iter().enumerate() {
                    let slice_index = start_index + q;
                    let logbase = logbases[slice_index] as u64;

                    // Coefficient = antilog[(logbase * exponent) % ORDER].
                    let mut coeffs = [0u16; 4];
                    for r in 0..chunk_len {
                        let exponent = exponent_start + (rec_base + r) as u32;
                        let log_coeff =
                            ((logbase * exponent as u64) % super::gf16::ORDER as u64) as u32;
                        coeffs[r] = gf.exp(log_coeff);
                    }

                    // Decode each active coefficient's dependency matrix once per
                    // (recovery-chunk, input-slice) pair — reused across every `vi`
                    // and every tail byte below. See `decode_plane_deps`.
                    let deps: [Option<([PlaneOutDeps; 16], usize)>; 4] = std::array::from_fn(|r| {
                        if r >= chunk_len || coeffs[r] == 0 {
                            None
                        } else {
                            Some(decode_plane_deps(&dep_tables[coeffs[r] as usize]))
                        }
                    });

                    // AVX2 path: one 256-bit vector per plane per vec-index.
                    for vi in 0..n_vec {
                        // Load 16 input planes at this vector position.
                        let mut in_planes = [_mm256_setzero_si256(); 16];
                        for p in 0..16usize {
                            let off = p * plane_bytes + vi * 32;
                            // SAFETY: bounds guaranteed by ALTMAP layout invariant.
                            in_planes[p] =
                                unsafe { _mm256_loadu_si256(slice_am.as_ptr().add(off).cast()) };
                        }

                        for r in 0..chunk_len {
                            let Some((ref plane_deps, count)) = deps[r] else {
                                continue;
                            };
                            for pd in &plane_deps[..count] {
                                let mut acc = _mm256_setzero_si256();
                                for &plane_in in &pd.plane_ins[..pd.n_ins as usize] {
                                    acc = _mm256_xor_si256(acc, in_planes[plane_in as usize]);
                                }
                                let off = pd.plane_out as usize * plane_bytes + vi * 32;
                                // SAFETY: off + 32 <= plane_bytes * 16 == buf.len().
                                let ptr = rec_chunk[r].as_mut_ptr().add(off).cast::<__m256i>();
                                let prev = unsafe { _mm256_loadu_si256(ptr) };
                                unsafe {
                                    _mm256_storeu_si256(ptr, _mm256_xor_si256(prev, acc));
                                }
                            }
                        }
                    }

                    // Scalar tail for remainder bytes within each plane.
                    let tail_start = n_vec * 32;
                    if tail_start < plane_bytes {
                        for r in 0..chunk_len {
                            let Some((ref plane_deps, count)) = deps[r] else {
                                continue;
                            };
                            for pd in &plane_deps[..count] {
                                for byte_off in tail_start..plane_bytes {
                                    let mut acc: u8 = 0;
                                    for &plane_in in &pd.plane_ins[..pd.n_ins as usize] {
                                        acc ^= slice_am[plane_in as usize * plane_bytes + byte_off];
                                    }
                                    rec_chunk[r][pd.plane_out as usize * plane_bytes + byte_off] ^=
                                        acc;
                                }
                            }
                        }
                    }
                }
            });
    }
}
