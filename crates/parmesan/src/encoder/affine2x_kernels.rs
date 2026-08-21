use super::*;

impl RecoveryEncoder {
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,gfni")]
    pub(super) unsafe fn flush_avx2_affine2x(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        let RecoveryBufferSet::Affine2x(ref mut bufs) = self.buffers else {
            unreachable!("flush_avx2_affine2x called on non-Affine2x encoder");
        };

        unsafe {
            Self::flush_avx2_affine2x_work(
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

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,gfni")]
    pub(super) unsafe fn a2x_contrib(
        data: __m256i,
        mat_norm: __m256i,
        mat_swap: __m256i,
    ) -> __m256i {
        let r1 = _mm256_gf2p8affine_epi64_epi8(data, mat_norm, 0);
        let r2 = _mm256_gf2p8affine_epi64_epi8(data, mat_swap, 0);
        // SHUFFLE(1,0,3,2): swap the two qwords of each 128-bit lane.
        _mm256_xor_si256(r1, _mm256_shuffle_epi32::<0x4E>(r2))
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,gfni")]
    pub(super) unsafe fn flush_avx2_affine2x_work(
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
        if start_index + n_queued > logbases.len() {
            panic!(
                "PAR2 slice index overflow: start_index({}) + n_queued({}) > logbases.len({})",
                start_index,
                n_queued,
                logbases.len()
            );
        }

        // Two ymm matrices per (recovery, input): mat_norm = [ll, hh] per lane,
        // mat_swap = [hl, lh] per lane. Built from 16×4 bit-basis products
        // (no 65k dep tables).
        let all_tables: Vec<(__m256i, __m256i)> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut m_ll = 0u64;
                let mut m_lh = 0u64;
                let mut m_hl = 0u64;
                let mut m_hh = 0u64;
                for row in 0..8usize {
                    let mut row_ll = 0u8;
                    let mut row_lh = 0u8;
                    let mut row_hl = 0u8;
                    let mut row_hh = 0u8;
                    for j in 0..8usize {
                        let r_lo = gf.mul(1u16 << j, coeff);
                        if (r_lo >> row) & 1 == 1 {
                            row_ll |= 1 << j;
                        }
                        if (r_lo >> (row + 8)) & 1 == 1 {
                            row_hl |= 1 << j;
                        }
                        let r_hi = gf.mul(1u16 << (j + 8), coeff);
                        if (r_hi >> row) & 1 == 1 {
                            row_lh |= 1 << j;
                        }
                        if (r_hi >> (row + 8)) & 1 == 1 {
                            row_hh |= 1 << j;
                        }
                    }
                    let shift = (7 - row) * 8;
                    m_ll |= (row_ll as u64) << shift;
                    m_lh |= (row_lh as u64) << shift;
                    m_hl |= (row_hl as u64) << shift;
                    m_hh |= (row_hh as u64) << shift;
                }

                let mat_norm =
                    _mm256_set_epi64x(m_hh as i64, m_ll as i64, m_hh as i64, m_ll as i64);
                let mat_swap =
                    _mm256_set_epi64x(m_lh as i64, m_hl as i64, m_lh as i64, m_hl as i64);
                (mat_norm, mat_swap)
            })
            .collect();

        let prepared: Vec<Vec<u8>> = queued
            .par_iter()
            .map(|s| {
                let mut out = vec![0u8; s.len()];
                crate::affine2x::to_affine2x(s, &mut out);
                out
            })
            .collect();

        // ParPar affine2x ideal chunk is 4 KiB.
        let tile = 4096usize;

        buffers.par_iter_mut().enumerate().for_each(|(rec, buf)| {
            let base = rec * n_queued;
            let n_tiles = buf.len().div_ceil(tile);
            for t in 0..n_tiles {
                let off = t * tile;
                let end = (off + tile).min(buf.len());
                let dest = &mut buf[off..end];
                let blocks = dest.len() / 32;
                if blocks == 0 {
                    continue;
                }
                let mut q = 0usize;
                while q < n_queued {
                    let take = (n_queued - q).min(6);
                    let mats: [(__m256i, __m256i); 6] = [
                        all_tables[base + q],
                        if take > 1 {
                            all_tables[base + q + 1]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 2 {
                            all_tables[base + q + 2]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 3 {
                            all_tables[base + q + 3]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 4 {
                            all_tables[base + q + 4]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 5 {
                            all_tables[base + q + 5]
                        } else {
                            all_tables[base + q]
                        },
                    ];
                    unsafe {
                        let dst_ptr = dest.as_mut_ptr() as *mut __m256i;
                        let src_ptrs: [*const __m256i; 6] = [
                            prepared[q][off..end].as_ptr() as *const __m256i,
                            if take > 1 {
                                prepared[q + 1][off..end].as_ptr() as *const __m256i
                            } else {
                                std::ptr::null()
                            },
                            if take > 2 {
                                prepared[q + 2][off..end].as_ptr() as *const __m256i
                            } else {
                                std::ptr::null()
                            },
                            if take > 3 {
                                prepared[q + 3][off..end].as_ptr() as *const __m256i
                            } else {
                                std::ptr::null()
                            },
                            if take > 4 {
                                prepared[q + 4][off..end].as_ptr() as *const __m256i
                            } else {
                                std::ptr::null()
                            },
                            if take > 5 {
                                prepared[q + 5][off..end].as_ptr() as *const __m256i
                            } else {
                                std::ptr::null()
                            },
                        ];
                        for b in 0..blocks {
                            let mut acc = _mm256_loadu_si256(dst_ptr.add(b));
                            for s in 0..take {
                                let data = _mm256_loadu_si256(src_ptrs[s].add(b));
                                acc = _mm256_xor_si256(
                                    acc,
                                    RecoveryEncoder::a2x_contrib(data, mats[s].0, mats[s].1),
                                );
                            }
                            _mm256_storeu_si256(dst_ptr.add(b), acc);
                        }
                    }
                    q += take;
                }
            }
        });
    }

    #[cfg(target_arch = "x86_64")]
    pub(super) unsafe fn flush_avx512_affine2x(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        let RecoveryBufferSet::Affine2x(ref mut bufs) = self.buffers else {
            unreachable!("flush_avx512_affine2x called on non-Affine2x encoder");
        };

        unsafe {
            Self::flush_avx512_affine2x_work(
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

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512bw,gfni")]
    pub(super) unsafe fn a2x_contrib512(
        data: __m512i,
        mat_norm: __m512i,
        mat_swap: __m512i,
    ) -> __m512i {
        let r1 = _mm512_gf2p8affine_epi64_epi8(data, mat_norm, 0);
        let r2 = _mm512_gf2p8affine_epi64_epi8(data, mat_swap, 0);
        // SHUFFLE(1,0,3,2): swap the two qwords of each 128-bit lane.
        _mm512_xor_si512(r1, _mm512_shuffle_epi32::<0x4E>(r2))
    }

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512bw,gfni")]
    pub(super) unsafe fn flush_avx512_affine2x_work(
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
        if start_index + n_queued > logbases.len() {
            panic!(
                "PAR2 slice index overflow: start_index({}) + n_queued({}) > logbases.len({})",
                start_index,
                n_queued,
                logbases.len()
            );
        }

        // Two ymm matrices per (recovery, input): mat_norm = [ll, hh] per lane,
        // mat_swap = [hl, lh] per lane. Built from 16×4 bit-basis products
        // (no 65k dep tables).
        let all_tables: Vec<(__m512i, __m512i)> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                let mut m_ll = 0u64;
                let mut m_lh = 0u64;
                let mut m_hl = 0u64;
                let mut m_hh = 0u64;
                for row in 0..8usize {
                    let mut row_ll = 0u8;
                    let mut row_lh = 0u8;
                    let mut row_hl = 0u8;
                    let mut row_hh = 0u8;
                    for j in 0..8usize {
                        let r_lo = gf.mul(1u16 << j, coeff);
                        if (r_lo >> row) & 1 == 1 {
                            row_ll |= 1 << j;
                        }
                        if (r_lo >> (row + 8)) & 1 == 1 {
                            row_hl |= 1 << j;
                        }
                        let r_hi = gf.mul(1u16 << (j + 8), coeff);
                        if (r_hi >> row) & 1 == 1 {
                            row_lh |= 1 << j;
                        }
                        if (r_hi >> (row + 8)) & 1 == 1 {
                            row_hh |= 1 << j;
                        }
                    }
                    let shift = (7 - row) * 8;
                    m_ll |= (row_ll as u64) << shift;
                    m_lh |= (row_lh as u64) << shift;
                    m_hl |= (row_hl as u64) << shift;
                    m_hh |= (row_hh as u64) << shift;
                }

                let mat_norm = _mm512_set_epi64(
                    m_hh as i64,
                    m_ll as i64,
                    m_hh as i64,
                    m_ll as i64,
                    m_hh as i64,
                    m_ll as i64,
                    m_hh as i64,
                    m_ll as i64,
                );
                let mat_swap = _mm512_set_epi64(
                    m_lh as i64,
                    m_hl as i64,
                    m_lh as i64,
                    m_hl as i64,
                    m_lh as i64,
                    m_hl as i64,
                    m_lh as i64,
                    m_hl as i64,
                );
                (mat_norm, mat_swap)
            })
            .collect();

        let prepared: Vec<Vec<u8>> = queued
            .par_iter()
            .map(|s| {
                let mut out = vec![0u8; s.len()];
                crate::affine2x::to_affine2x(s, &mut out);
                out
            })
            .collect();

        // ParPar affine2x ideal chunk is 4 KiB.
        let tile = 4096usize;

        buffers.par_iter_mut().enumerate().for_each(|(rec, buf)| {
            let base = rec * n_queued;
            let n_tiles = buf.len().div_ceil(tile);
            for t in 0..n_tiles {
                let off = t * tile;
                let end = (off + tile).min(buf.len());
                let dest = &mut buf[off..end];
                let blocks = dest.len() / 64;
                if blocks == 0 {
                    continue;
                }
                let mut q = 0usize;
                while q < n_queued {
                    let take = (n_queued - q).min(12);
                    let mats: [(__m512i, __m512i); 12] = [
                        all_tables[base + q],
                        if take > 1 {
                            all_tables[base + q + 1]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 2 {
                            all_tables[base + q + 2]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 3 {
                            all_tables[base + q + 3]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 4 {
                            all_tables[base + q + 4]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 5 {
                            all_tables[base + q + 5]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 6 {
                            all_tables[base + q + 6]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 7 {
                            all_tables[base + q + 7]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 8 {
                            all_tables[base + q + 8]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 9 {
                            all_tables[base + q + 9]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 10 {
                            all_tables[base + q + 10]
                        } else {
                            all_tables[base + q]
                        },
                        if take > 11 {
                            all_tables[base + q + 11]
                        } else {
                            all_tables[base + q]
                        },
                    ];
                    unsafe {
                        let dst_ptr = dest.as_mut_ptr() as *mut __m512i;
                        let src_ptrs: [*const __m512i; 12] = [
                            prepared[q][off..end].as_ptr() as *const __m512i,
                            if take > 1 {
                                prepared[q + 1][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 2 {
                                prepared[q + 2][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 3 {
                                prepared[q + 3][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 4 {
                                prepared[q + 4][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 5 {
                                prepared[q + 5][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 6 {
                                prepared[q + 6][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 7 {
                                prepared[q + 7][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 8 {
                                prepared[q + 8][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 9 {
                                prepared[q + 9][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 10 {
                                prepared[q + 10][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                            if take > 11 {
                                prepared[q + 11][off..end].as_ptr() as *const __m512i
                            } else {
                                std::ptr::null()
                            },
                        ];

                        macro_rules! unroll_take {
                            ($take:expr, $src_ptrs:expr, $mats:expr, $b:expr, $acc:expr) => {
                                match $take {
                                    1 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                    }
                                    2 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                    }
                                    3 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                    }
                                    4 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[3].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[3].0, $mats[3].1,
                                            ),
                                        );
                                    }
                                    5 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[3].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[3].0, $mats[3].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[4].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[4].0, $mats[4].1,
                                            ),
                                        );
                                    }
                                    6 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[3].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[3].0, $mats[3].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[4].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[4].0, $mats[4].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[5].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[5].0, $mats[5].1,
                                            ),
                                        );
                                    }
                                    7 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[3].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[3].0, $mats[3].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[4].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[4].0, $mats[4].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[5].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[5].0, $mats[5].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[6].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[6].0, $mats[6].1,
                                            ),
                                        );
                                    }
                                    8 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[3].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[3].0, $mats[3].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[4].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[4].0, $mats[4].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[5].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[5].0, $mats[5].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[6].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[6].0, $mats[6].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[7].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[7].0, $mats[7].1,
                                            ),
                                        );
                                    }
                                    9 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[3].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[3].0, $mats[3].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[4].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[4].0, $mats[4].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[5].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[5].0, $mats[5].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[6].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[6].0, $mats[6].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[7].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[7].0, $mats[7].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[8].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[8].0, $mats[8].1,
                                            ),
                                        );
                                    }
                                    10 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[3].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[3].0, $mats[3].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[4].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[4].0, $mats[4].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[5].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[5].0, $mats[5].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[6].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[6].0, $mats[6].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[7].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[7].0, $mats[7].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[8].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[8].0, $mats[8].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[9].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[9].0, $mats[9].1,
                                            ),
                                        );
                                    }
                                    11 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[3].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[3].0, $mats[3].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[4].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[4].0, $mats[4].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[5].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[5].0, $mats[5].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[6].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[6].0, $mats[6].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[7].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[7].0, $mats[7].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[8].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[8].0, $mats[8].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[9].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[9].0, $mats[9].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[10].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data,
                                                $mats[10].0,
                                                $mats[10].1,
                                            ),
                                        );
                                    }
                                    12 => {
                                        let data = _mm512_loadu_si512($src_ptrs[0].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[0].0, $mats[0].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[1].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[1].0, $mats[1].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[2].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[2].0, $mats[2].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[3].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[3].0, $mats[3].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[4].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[4].0, $mats[4].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[5].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[5].0, $mats[5].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[6].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[6].0, $mats[6].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[7].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[7].0, $mats[7].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[8].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[8].0, $mats[8].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[9].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data, $mats[9].0, $mats[9].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[10].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data,
                                                $mats[10].0,
                                                $mats[10].1,
                                            ),
                                        );
                                        let data = _mm512_loadu_si512($src_ptrs[11].add($b));
                                        $acc = _mm512_xor_si512(
                                            $acc,
                                            RecoveryEncoder::a2x_contrib512(
                                                data,
                                                $mats[11].0,
                                                $mats[11].1,
                                            ),
                                        );
                                    }
                                    _ => unreachable!(),
                                }
                            };
                        }

                        for b in 0..blocks {
                            let mut acc = _mm512_loadu_si512(dst_ptr.add(b));
                            unroll_take!(take, src_ptrs, mats, b, acc);
                            _mm512_storeu_si512(dst_ptr.add(b), acc);
                        }
                    }
                    q += take;
                }
            }
        });
    }
}
