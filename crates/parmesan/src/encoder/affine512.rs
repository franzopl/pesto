use super::*;

#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(super) fn pack_affine_tile(
    prepared: &[Vec<u8>],
    off: usize,
    tile_len: usize,
    block: usize,
) -> Vec<u8> {
    let n = prepared.len();
    let blocks = tile_len / block;
    let mut out = vec![0u8; n * tile_len];
    for b in 0..blocks {
        for s in 0..n {
            let src = &prepared[s][off + b * block..off + (b + 1) * block];
            let dst = &mut out[(b * n + s) * block..(b * n + s + 1) * block];
            dst.copy_from_slice(src);
        }
    }
    out
}

/// Parpar Affine AVX-512 packed: 128-byte blocks, `srcCount` 6, 4 KiB tiles.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(super) const AFFINE512_BLOCK: usize = 128;
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(super) const AFFINE512_INTERLEAVE: usize = 6;
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(super) const AFFINE512_CHUNK: usize = 4096;

#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(super) fn affine512_src_scale(n_queued: usize, source: usize) -> usize {
    let il = AFFINE512_INTERLEAVE;
    let last = n_queued - n_queued % il;
    if !n_queued.is_multiple_of(il) && source >= last {
        n_queued % il
    } else {
        il
    }
}

/// Dest tile `tile`, recovery `rec`: `tile * n_rec * CHUNK + rec * CHUNK`.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(super) fn affine512_dest_off(n_rec: usize, rec: usize, tile: usize) -> usize {
    tile * n_rec * AFFINE512_CHUNK + rec * AFFINE512_CHUNK
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw")]
pub(super) unsafe fn affine512_acc_to_slices(
    acc: Affine512Acc,
    exponent_start: u32,
) -> Vec<RecoverySlice> {
    (0..acc.n_rec)
        .into_par_iter()
        .map(|i| {
            let mut data = vec![0u8; acc.slice_len];
            let n_tiles = acc.slice_len.div_ceil(AFFINE512_CHUNK);
            for t in 0..n_tiles {
                let off = t * AFFINE512_CHUNK;
                let tile_len = (acc.slice_len - off).min(AFFINE512_CHUNK);
                let src = acc.data.as_ptr().add(affine512_dest_off(acc.n_rec, i, t));
                let dst = data.as_mut_ptr().add(off);
                for b in 0..tile_len / AFFINE512_BLOCK {
                    super::affine::affine512_finish_block(
                        src.add(b * AFFINE512_BLOCK),
                        dst.add(b * AFFINE512_BLOCK),
                    );
                }
            }
            RecoverySlice {
                exponent: exponent_start + i as u32,
                data,
            }
        })
        .collect()
}

/// Byte offset of source `s`, 128-byte block `block` inside tile `tile`.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(super) fn affine512_packed_off(
    n_queued: usize,
    tile: usize,
    source: usize,
    block: usize,
) -> usize {
    let il = AFFINE512_INTERLEAVE;
    let b = AFFINE512_BLOCK;
    let chunk = AFFINE512_CHUNK;
    let group = source / il;
    let lane = source % il;
    let scale = affine512_src_scale(n_queued, source);
    tile * chunk * n_queued + group * chunk * il + lane * b + block * b * scale
}

/// Shuffle-prepare raw slices into the ParPar packed Affine512 layout (once).
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512f,avx512bw")]
pub(super) unsafe fn prepare_packed_affine512(
    queued: &[Vec<u8>],
    packed: &mut [u8],
    slice_len: usize,
) {
    let n = queued.len();
    if n == 0 {
        return;
    }
    let n_tiles = slice_len.div_ceil(AFFINE512_CHUNK);
    let dst = packed.as_mut_ptr() as usize;
    queued.par_iter().enumerate().for_each(|(s, src)| {
        for t in 0..n_tiles {
            let off = t * AFFINE512_CHUNK;
            let tile_len = (slice_len - off).min(AFFINE512_CHUNK);
            let blocks = tile_len / AFFINE512_BLOCK;
            for b in 0..blocks {
                let src_p = src.as_ptr().add(off + b * AFFINE512_BLOCK);
                let dst_p = (dst as *mut u8).add(affine512_packed_off(n, t, s, b));
                crate::affine::affine512_prepare_block(src_p, dst_p);
            }
        }
    });
}

impl RecoveryEncoder {
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512bw,gfni")]
    pub(super) unsafe fn flush_avx512_affine(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();
        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };
        let RecoveryBufferSet::Affine512(ref mut acc) = self.buffers else {
            unreachable!("flush_avx512_affine on non-Affine512 encoder");
        };
        let mut prepared = std::mem::take(&mut self.affine_prepare);
        let slice_len = acc.slice_len;
        let n_tiles = slice_len.div_ceil(AFFINE512_CHUNK);
        let packed_len = n_tiles * AFFINE512_CHUNK * queued.len();
        if prepared.is_empty() {
            prepared.push(vec![0u8; packed_len]);
        } else {
            prepared[0].resize(packed_len, 0);
        }
        unsafe {
            prepare_packed_affine512(&queued, &mut prepared[0], slice_len);
            Self::flush_avx512_affine_work(
                acc,
                &prepared[0],
                queued.len(),
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
    #[target_feature(enable = "avx512f,avx512bw,gfni")]
    pub(super) unsafe fn flush_avx512_affine_work(
        acc: &mut Affine512Acc,
        packed: &[u8],
        n_queued: usize,
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        let n_rec = acc.n_rec;
        if n_queued == 0 || n_rec == 0 {
            return;
        }
        let slice_len = acc.slice_len;
        let n_tiles = slice_len.div_ceil(AFFINE512_CHUNK);
        let scratch = AffineNibbleScratch::new(gf);
        let coeffs: Vec<u16> = (0..n_rec * n_queued)
            .map(|flat| {
                let rec = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + rec as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                gf.exp(log_coeff)
            })
            .collect();

        let dest_base = acc.data.as_mut_ptr() as usize;
        let packed_addr = packed.as_ptr() as usize;

        (0..n_tiles).into_par_iter().for_each(|t| {
            let off = t * AFFINE512_CHUNK;
            let tile_len = (slice_len - off).min(AFFINE512_CHUNK);
            let blocks = tile_len / AFFINE512_BLOCK;
            if blocks == 0 {
                return;
            }
            if t + 1 < n_tiles {
                unsafe {
                    _mm_prefetch::<_MM_HINT_T1>(
                        (packed_addr as *const i8).add(affine512_packed_off(n_queued, t + 1, 0, 0)),
                    );
                }
            }
            for rec in 0..n_rec {
                let dst = (dest_base as *mut u8).add(affine512_dest_off(n_rec, rec, t));
                if rec + 1 < n_rec {
                    unsafe {
                        _mm_prefetch::<_MM_HINT_T0>(
                            (dest_base as *const i8).add(affine512_dest_off(n_rec, rec + 1, t)),
                        );
                    }
                }
                let mut q = 0usize;
                while q < n_queued {
                    let take = (n_queued - q).min(AFFINE512_INTERLEAVE);
                    let mut mats = [(
                        _mm512_setzero_si512(),
                        _mm512_setzero_si512(),
                        _mm512_setzero_si512(),
                        _mm512_setzero_si512(),
                    ); 6];
                    let base = rec * n_queued + q;
                    for s in 0..take {
                        let (m_ll, m_lh, m_hl, m_hh) = scratch.load(coeffs[base + s]);
                        mats[s] = (
                            _mm512_set1_epi64(m_ll as i64),
                            _mm512_set1_epi64(m_hl as i64),
                            _mm512_set1_epi64(m_lh as i64),
                            _mm512_set1_epi64(m_hh as i64),
                        );
                    }
                    unsafe {
                        for b in 0..blocks {
                            let p = dst.add(b * AFFINE512_BLOCK).cast::<__m512i>();
                            let mut tph = _mm512_loadu_si512(p);
                            let mut tpl = _mm512_loadu_si512(p.add(1));
                            for (s, &(mll, mhl, mlh, mhh)) in mats.iter().enumerate().take(take) {
                                let sp = (packed_addr as *const u8)
                                    .add(affine512_packed_off(n_queued, t, q + s, b))
                                    .cast::<__m512i>();
                                let ta = _mm512_loadu_si512(sp);
                                let tb = _mm512_loadu_si512(sp.add(1));
                                tpl = _mm512_ternarylogic_epi32(
                                    _mm512_gf2p8affine_epi64_epi8(ta, mlh, 0),
                                    _mm512_gf2p8affine_epi64_epi8(tb, mll, 0),
                                    tpl,
                                    0x96,
                                );
                                tph = _mm512_ternarylogic_epi32(
                                    _mm512_gf2p8affine_epi64_epi8(ta, mhh, 0),
                                    _mm512_gf2p8affine_epi64_epi8(tb, mhl, 0),
                                    tph,
                                    0x96,
                                );
                            }
                            _mm512_storeu_si512(p, tph);
                            _mm512_storeu_si512(p.add(1), tpl);
                        }
                    }
                    q += take;
                }
            }
        });
    }
}
