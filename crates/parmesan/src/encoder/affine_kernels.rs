use super::*;

impl RecoveryEncoder {
    /// Parpar Affine: shuffle-prepare inputs, 4× gf2p8affine per 64 B, up to
    /// three sources per dest store (AVX2 `idealInputMultiple`).
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,gfni")]
    pub(super) unsafe fn flush_avx2_affine(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();
        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };
        let RecoveryBufferSet::Affine(ref mut bufs) = self.buffers else {
            unreachable!("flush_avx2_affine on non-Affine encoder");
        };
        let mut prepared = std::mem::take(&mut self.affine_prepare);
        Self::prepare_affine_inputs(&queued, &mut prepared, false);
        unsafe {
            Self::flush_avx2_affine_work(
                bufs,
                &prepared,
                start_index,
                &self.logbases,
                self.exponent_start,
                &self.gf,
            );
        }
        self.affine_prepare = prepared;
        self.pending_checksums.extend(new_cs);
        self.recycle_queue(queued);
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,gfni")]
    pub(super) unsafe fn flush_avx2_affine_work(
        buffers: &mut [Vec<u8>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        let n_rec = buffers.len();
        let n_queued = queued.len();
        if n_queued == 0 {
            return;
        }
        let scratch = AffineNibbleScratch::new(gf);
        let all_tables: Vec<(__m256i, __m256i, __m256i, __m256i)> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);
                let (m_ll, m_lh, m_hl, m_hh) = scratch.load(coeff);
                (
                    _mm256_set1_epi64x(m_ll as i64),
                    _mm256_set1_epi64x(m_hl as i64),
                    _mm256_set1_epi64x(m_lh as i64),
                    _mm256_set1_epi64x(m_hh as i64),
                )
            })
            .collect();

        let tile = 4096usize;
        if buffers.is_empty() {
            return;
        }
        let slice_len = buffers[0].len();
        let n_tiles = slice_len.div_ceil(tile);
        for t in 0..n_tiles {
            let off = t * tile;
            let end = (off + tile).min(slice_len);
            let tile_len = end - off;
            if tile_len < 64 {
                continue;
            }
            let packed = pack_affine_tile(queued, off, tile_len, 64);
            let blocks = tile_len / 64;
            buffers.par_iter_mut().enumerate().for_each(|(rec, buf)| {
                let base = rec * n_queued;
                let dest = &mut buf[off..end];
                let mut q = 0usize;
                while q < n_queued {
                    let take = (n_queued - q).min(3);
                    unsafe {
                        let dst = dest.as_mut_ptr();
                        let pk = packed.as_ptr();
                        for b in 0..blocks {
                            let p = dst.add(b * 64).cast::<__m256i>();
                            let mut tph = _mm256_loadu_si256(p);
                            let mut tpl = _mm256_loadu_si256(p.add(1));
                            for s in 0..take {
                                let (mll, mhl, mlh, mhh) = all_tables[base + q + s];
                                let sp = pk.add((b * n_queued + q + s) * 64).cast::<__m256i>();
                                let ta = _mm256_loadu_si256(sp);
                                let tb = _mm256_loadu_si256(sp.add(1));
                                tpl = _mm256_xor_si256(
                                    tpl,
                                    _mm256_xor_si256(
                                        _mm256_gf2p8affine_epi64_epi8(ta, mlh, 0),
                                        _mm256_gf2p8affine_epi64_epi8(tb, mll, 0),
                                    ),
                                );
                                tph = _mm256_xor_si256(
                                    tph,
                                    _mm256_xor_si256(
                                        _mm256_gf2p8affine_epi64_epi8(ta, mhh, 0),
                                        _mm256_gf2p8affine_epi64_epi8(tb, mhl, 0),
                                    ),
                                );
                            }
                            _mm256_storeu_si256(p, tph);
                            _mm256_storeu_si256(p.add(1), tpl);
                        }
                    }
                    q += take;
                }
            });
        }
    }
}
