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

    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx2,gfni")]
    pub(super) unsafe fn flush_avx2_gfni(&mut self) {
        let start_index = self.next_index;
        let queued = std::mem::take(&mut self.queued_slices);
        self.next_index += queued.len();

        let new_cs: Vec<SliceChecksum> = if self.compute_checksums {
            queued.par_iter().map(|s| slice_checksum(s)).collect()
        } else {
            Vec::new()
        };

        unsafe {
            Self::flush_avx2_gfni_work(
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
    #[target_feature(enable = "avx2,gfni")]
    #[allow(dead_code)]
    pub(super) unsafe fn flush_avx2_gfni_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        use std::arch::x86_64::*;

        let deint_mask = _mm256_broadcastsi128_si256(_mm_setr_epi8(
            0, 2, 4, 6, 8, 10, 12, 14, // lo bytes of 8 words → positions 0..7
            1, 3, 5, 7, 9, 11, 13, 15, // hi bytes of 8 words → positions 8..15
        ));

        let n_rec = buffers.len();
        let n_queued = queued.len();

        let all_tables: Vec<Avx2GfniTable> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                // gf2p8affineqb uses byte (7-row) of the u64 matrix operand for
                // output bit `row` — so store each row at position (7-row)*8.
                let mut m_ll = 0u64; // lo input byte → lo output byte
                let mut m_lh = 0u64; // hi input byte → lo output byte
                let mut m_hl = 0u64; // lo input byte → hi output byte
                let mut m_hh = 0u64; // hi input byte → hi output byte
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

                let mat_lo = _mm256_set_epi64x(m_lh as i64, m_ll as i64, m_lh as i64, m_ll as i64);
                let mat_hi = _mm256_set_epi64x(m_hh as i64, m_hl as i64, m_hh as i64, m_hl as i64);

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                (mat_lo, mat_hi, table_low, table_high)
            })
            .collect();

        // 32 KiB recovery buffer chunk: chunk × group fits L1/L2 and amortizes
        // the rayon task overhead. 8 × 32 KiB = 256 KiB stays in L2 on most
        // modern CPUs; if L2 is smaller (older Skylake clients) the hardware
        // prefetcher compensates adequately.
        let chunk_size = 16384usize;

        buffers
            .par_chunks_mut(8)
            .enumerate()
            .for_each(|(group_idx, buf_group)| {
                let i = group_idx * 8;
                match buf_group {
                    // 8-way unroll: one input load + one deinterleave feeds 8 recovery blocks,
                    // halving the loadu/shuffle overhead compared with the 4-way arm.
                    [buf_a, buf_b, buf_c, buf_d, buf_e, buf_f, buf_g, buf_h] => {
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        let base_c = (i + 2) * n_queued;
                        let base_d = (i + 3) * n_queued;
                        let base_e = (i + 4) * n_queued;
                        let base_f = (i + 5) * n_queued;
                        let base_g = (i + 6) * n_queued;
                        let base_h = (i + 7) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .zip(buf_c.par_chunks_mut(chunk_size))
                            .zip(buf_d.par_chunks_mut(chunk_size))
                            .zip(buf_e.par_chunks_mut(chunk_size))
                            .zip(buf_f.par_chunks_mut(chunk_size))
                            .zip(buf_g.par_chunks_mut(chunk_size))
                            .zip(buf_h.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(
                                |(
                                    chunk_idx,
                                    (
                                        (
                                            (
                                                ((((chunk_a, chunk_b), chunk_c), chunk_d), chunk_e),
                                                chunk_f,
                                            ),
                                            chunk_g,
                                        ),
                                        chunk_h,
                                    ),
                                )| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk_a.len() * 2;
                                    let blocks_32 = byte_len / 32;
                                    let remainder = byte_len % 32;

                                    for q_idx in 0..n_queued {
                                        let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                            all_tables[base_a + q_idx];
                                        let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                            all_tables[base_b + q_idx];
                                        let (mat_lo_c, mat_hi_c, ref tlow_c, ref thigh_c) =
                                            all_tables[base_c + q_idx];
                                        let (mat_lo_d, mat_hi_d, ref tlow_d, ref thigh_d) =
                                            all_tables[base_d + q_idx];
                                        let (mat_lo_e, mat_hi_e, ref tlow_e, ref thigh_e) =
                                            all_tables[base_e + q_idx];
                                        let (mat_lo_f, mat_hi_f, ref tlow_f, ref thigh_f) =
                                            all_tables[base_f + q_idx];
                                        let (mat_lo_g, mat_hi_g, ref tlow_g, ref thigh_g) =
                                            all_tables[base_g + q_idx];
                                        let (mat_lo_h, mat_hi_h, ref tlow_h, ref thigh_h) =
                                            all_tables[base_h + q_idx];

                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_c = chunk_c.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_d = chunk_d.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_e = chunk_e.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_f = chunk_f.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_g = chunk_g.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_h = chunk_h.as_mut_ptr() as *mut __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let separated = _mm256_shuffle_epi8(input, deint_mask);

                                            macro_rules! gfni_block {
                                                ($mat_lo:expr, $mat_hi:expr, $ptr:expr) => {{
                                                    let vlo = _mm256_gf2p8affine_epi64_epi8(
                                                        separated, $mat_lo, 0,
                                                    );
                                                    let vhi = _mm256_gf2p8affine_epi64_epi8(
                                                        separated, $mat_hi, 0,
                                                    );
                                                    let out = _mm256_unpacklo_epi8(
                                                        _mm256_xor_si256(
                                                            vlo,
                                                            _mm256_bsrli_epi128::<8>(vlo),
                                                        ),
                                                        _mm256_xor_si256(
                                                            vhi,
                                                            _mm256_bsrli_epi128::<8>(vhi),
                                                        ),
                                                    );
                                                    _mm256_storeu_si256(
                                                        $ptr,
                                                        _mm256_xor_si256(
                                                            _mm256_loadu_si256($ptr),
                                                            out,
                                                        ),
                                                    );
                                                }};
                                            }

                                            gfni_block!(mat_lo_a, mat_hi_a, ptr_a);
                                            gfni_block!(mat_lo_b, mat_hi_b, ptr_b);
                                            gfni_block!(mat_lo_c, mat_hi_c, ptr_c);
                                            gfni_block!(mat_lo_d, mat_hi_d, ptr_d);
                                            gfni_block!(mat_lo_e, mat_hi_e, ptr_e);
                                            gfni_block!(mat_lo_f, mat_hi_f, ptr_f);
                                            gfni_block!(mat_lo_g, mat_hi_g, ptr_g);
                                            gfni_block!(mat_lo_h, mat_hi_h, ptr_h);

                                            ptr_in = ptr_in.add(1);
                                            ptr_a = ptr_a.add(1);
                                            ptr_b = ptr_b.add(1);
                                            ptr_c = ptr_c.add(1);
                                            ptr_d = ptr_d.add(1);
                                            ptr_e = ptr_e.add(1);
                                            ptr_f = ptr_f.add(1);
                                            ptr_g = ptr_g.add(1);
                                            ptr_h = ptr_h.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_32 * 16;
                                            let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                            let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                            let mut pw_c = chunk_c[ow..].as_mut_ptr();
                                            let mut pw_d = chunk_d[ow..].as_mut_ptr();
                                            let mut pw_e = chunk_e[ow..].as_mut_ptr();
                                            let mut pw_f = chunk_f[ow..].as_mut_ptr();
                                            let mut pw_g = chunk_g[ow..].as_mut_ptr();
                                            let mut pw_h = chunk_h[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_32 * 32..].as_ptr();
                                            let tail_end = p_in.add(remainder);
                                            while p_in < tail_end {
                                                let lo = *p_in as usize;
                                                let hi = *p_in.add(1) as usize;
                                                *pw_a ^= tlow_a[lo] ^ thigh_a[hi];
                                                *pw_b ^= tlow_b[lo] ^ thigh_b[hi];
                                                *pw_c ^= tlow_c[lo] ^ thigh_c[hi];
                                                *pw_d ^= tlow_d[lo] ^ thigh_d[hi];
                                                *pw_e ^= tlow_e[lo] ^ thigh_e[hi];
                                                *pw_f ^= tlow_f[lo] ^ thigh_f[hi];
                                                *pw_g ^= tlow_g[lo] ^ thigh_g[hi];
                                                *pw_h ^= tlow_h[lo] ^ thigh_h[hi];
                                                pw_a = pw_a.add(1);
                                                pw_b = pw_b.add(1);
                                                pw_c = pw_c.add(1);
                                                pw_d = pw_d.add(1);
                                                pw_e = pw_e.add(1);
                                                pw_f = pw_f.add(1);
                                                pw_g = pw_g.add(1);
                                                pw_h = pw_h.add(1);
                                                p_in = p_in.add(2);
                                            }
                                        }
                                    }
                                },
                            );
                    }
                    [buf_a, buf_b, buf_c, buf_d] => {
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
                                        let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                            all_tables[base_a + q_idx];
                                        let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                            all_tables[base_b + q_idx];
                                        let (mat_lo_c, mat_hi_c, ref tlow_c, ref thigh_c) =
                                            all_tables[base_c + q_idx];
                                        let (mat_lo_d, mat_hi_d, ref tlow_d, ref thigh_d) =
                                            all_tables[base_d + q_idx];

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
                                            let separated = _mm256_shuffle_epi8(input, deint_mask);

                                            // Block A
                                            let vlo_a = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_lo_a, 0,
                                            );
                                            let new_lo_a = _mm256_xor_si256(
                                                vlo_a,
                                                _mm256_bsrli_epi128::<8>(vlo_a),
                                            );
                                            let vhi_a = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_hi_a, 0,
                                            );
                                            let new_hi_a = _mm256_xor_si256(
                                                vhi_a,
                                                _mm256_bsrli_epi128::<8>(vhi_a),
                                            );
                                            let out_a = _mm256_unpacklo_epi8(new_lo_a, new_hi_a);
                                            _mm256_storeu_si256(
                                                ptr_a,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_a), out_a),
                                            );

                                            // Block B
                                            let vlo_b = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_lo_b, 0,
                                            );
                                            let new_lo_b = _mm256_xor_si256(
                                                vlo_b,
                                                _mm256_bsrli_epi128::<8>(vlo_b),
                                            );
                                            let vhi_b = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_hi_b, 0,
                                            );
                                            let new_hi_b = _mm256_xor_si256(
                                                vhi_b,
                                                _mm256_bsrli_epi128::<8>(vhi_b),
                                            );
                                            let out_b = _mm256_unpacklo_epi8(new_lo_b, new_hi_b);
                                            _mm256_storeu_si256(
                                                ptr_b,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_b), out_b),
                                            );

                                            // Block C
                                            let vlo_c = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_lo_c, 0,
                                            );
                                            let new_lo_c = _mm256_xor_si256(
                                                vlo_c,
                                                _mm256_bsrli_epi128::<8>(vlo_c),
                                            );
                                            let vhi_c = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_hi_c, 0,
                                            );
                                            let new_hi_c = _mm256_xor_si256(
                                                vhi_c,
                                                _mm256_bsrli_epi128::<8>(vhi_c),
                                            );
                                            let out_c = _mm256_unpacklo_epi8(new_lo_c, new_hi_c);
                                            _mm256_storeu_si256(
                                                ptr_c,
                                                _mm256_xor_si256(_mm256_loadu_si256(ptr_c), out_c),
                                            );

                                            // Block D
                                            let vlo_d = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_lo_d, 0,
                                            );
                                            let new_lo_d = _mm256_xor_si256(
                                                vlo_d,
                                                _mm256_bsrli_epi128::<8>(vlo_d),
                                            );
                                            let vhi_d = _mm256_gf2p8affine_epi64_epi8(
                                                separated, mat_hi_d, 0,
                                            );
                                            let new_hi_d = _mm256_xor_si256(
                                                vhi_d,
                                                _mm256_bsrli_epi128::<8>(vhi_d),
                                            );
                                            let out_d = _mm256_unpacklo_epi8(new_lo_d, new_hi_d);
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
                                    let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                        all_tables[base_a + q_idx];
                                    let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                        all_tables[base_b + q_idx];
                                    let slice_chunk =
                                        &queued[q_idx][byte_offset..byte_offset + byte_len];

                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                    let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m256i;
                                    let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m256i;
                                    let end = ptr_in.add(blocks_32);

                                    while ptr_in < end {
                                        _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                        let input = _mm256_loadu_si256(ptr_in);
                                        let separated = _mm256_shuffle_epi8(input, deint_mask);

                                        let vlo_a =
                                            _mm256_gf2p8affine_epi64_epi8(separated, mat_lo_a, 0);
                                        let new_lo_a = _mm256_xor_si256(
                                            vlo_a,
                                            _mm256_bsrli_epi128::<8>(vlo_a),
                                        );
                                        let vhi_a =
                                            _mm256_gf2p8affine_epi64_epi8(separated, mat_hi_a, 0);
                                        let new_hi_a = _mm256_xor_si256(
                                            vhi_a,
                                            _mm256_bsrli_epi128::<8>(vhi_a),
                                        );
                                        let out_a = _mm256_unpacklo_epi8(new_lo_a, new_hi_a);
                                        _mm256_storeu_si256(
                                            ptr_a,
                                            _mm256_xor_si256(_mm256_loadu_si256(ptr_a), out_a),
                                        );

                                        let vlo_b =
                                            _mm256_gf2p8affine_epi64_epi8(separated, mat_lo_b, 0);
                                        let new_lo_b = _mm256_xor_si256(
                                            vlo_b,
                                            _mm256_bsrli_epi128::<8>(vlo_b),
                                        );
                                        let vhi_b =
                                            _mm256_gf2p8affine_epi64_epi8(separated, mat_hi_b, 0);
                                        let new_hi_b = _mm256_xor_si256(
                                            vhi_b,
                                            _mm256_bsrli_epi128::<8>(vhi_b),
                                        );
                                        let out_b = _mm256_unpacklo_epi8(new_lo_b, new_hi_b);
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
                        for (j, buf) in rest.iter_mut().enumerate() {
                            let base = (i + j) * n_queued;
                            buf.par_chunks_mut(chunk_size).enumerate().for_each(
                                |(chunk_idx, chunk)| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk.len() * 2;
                                    let blocks_32 = byte_len / 32;
                                    let remainder = byte_len % 32;

                                    for q_idx in 0..n_queued {
                                        let (mat_lo, mat_hi, ref tlow, ref thigh) =
                                            all_tables[base + q_idx];
                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_buf = chunk.as_mut_ptr() as *mut __m256i;
                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m256i;
                                        let end = ptr_in.add(blocks_32);

                                        while ptr_in < end {
                                            _mm_prefetch(ptr_in.add(4) as *const i8, _MM_HINT_T0);
                                            let input = _mm256_loadu_si256(ptr_in);
                                            let separated = _mm256_shuffle_epi8(input, deint_mask);
                                            let vlo =
                                                _mm256_gf2p8affine_epi64_epi8(separated, mat_lo, 0);
                                            let vhi =
                                                _mm256_gf2p8affine_epi64_epi8(separated, mat_hi, 0);
                                            let out = _mm256_unpacklo_epi8(
                                                _mm256_xor_si256(
                                                    vlo,
                                                    _mm256_bsrli_epi128::<8>(vlo),
                                                ),
                                                _mm256_xor_si256(
                                                    vhi,
                                                    _mm256_bsrli_epi128::<8>(vhi),
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
                                                *pw ^= tlow[lo] ^ thigh[hi];
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

    /// GF(2^16) multiply-by-coefficient using AVX-512BW + GFNI instructions.
    ///
    /// The `vgf2p8affineqb` instruction applies an 8×8 GF(2) matrix to each input
    /// byte in a single cycle. Any GF(2^16) multiply-by-constant is a GF(2)-linear
    /// map on 16 bits, which decomposes into four 8×8 matrices (one per pair of
    /// input/output byte halves). Processing 512-bit vectors yields 32 GF(2^16)
    /// words per loop iteration — roughly twice the AVX2 throughput.
    ///
    /// Inner-loop layout (per 512-bit iteration):
    ///   1. De-interleave bytes within each 128-bit lane so lo bytes occupy the
    ///      low qword and hi bytes the high qword.
    ///   2. Apply two GFNI affine transforms (mat_lo, mat_hi) — each call covers
    ///      both the M_ll/M_lh or M_hl/M_hh matrices simultaneously by placing
    ///      different matrices in the two qwords of each lane.
    ///   3. Fold the two qword results within each lane (bsrli + xor) to produce
    ///      the combined lo and hi result bytes.
    ///   4. Re-interleave with `vunpcklbw` and XOR into the recovery buffer.
    #[cfg(target_arch = "x86_64")]
    #[target_feature(enable = "avx512f,avx512bw,gfni")]
    pub(super) unsafe fn flush_avx512_gfni(&mut self) {
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
                    Self::flush_avx512_gfni_work(
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
                Self::flush_avx512_gfni_work(
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
    #[target_feature(enable = "avx512f,avx512bw,gfni")]
    pub(super) unsafe fn flush_avx512_gfni_work(
        buffers: &mut [Vec<u16>],
        queued: &[Vec<u8>],
        start_index: usize,
        logbases: &[u32],
        exponent_start: u32,
        gf: &Gf16,
    ) {
        use std::arch::x86_64::*;

        // Broadcast the de-interleave shuffle to all four 128-bit lanes:
        // within each lane, move lo bytes (even positions 0,2,…,14) to the low
        // qword (positions 0..7) and hi bytes (odd positions 1,3,…,15) to the
        // high qword (positions 8..15). This lets us apply different GFNI
        // matrices to lo vs hi bytes in a single vgf2p8affineqb call.
        let deint_mask = _mm512_broadcast_i32x4(_mm_setr_epi8(
            0, 2, 4, 6, 8, 10, 12, 14, // lo bytes of 8 words → positions 0..7
            1, 3, 5, 7, 9, 11, 13, 15, // hi bytes of 8 words → positions 8..15
        ));

        let n_rec = buffers.len();
        let n_queued = queued.len();

        // Pre-build all coefficient tables in a single parallel pass.
        // Layout: all_tables[rec * n_queued + q_idx].
        let all_tables: Vec<Avx512GfniTable> = (0..n_rec * n_queued)
            .into_par_iter()
            .map(|flat| {
                let i = flat / n_queued;
                let q_idx = flat % n_queued;
                let exponent = exponent_start + i as u32;
                let logbase = logbases[start_index + q_idx] as u64;
                let log_coeff = ((logbase * exponent as u64) % ORDER as u64) as u32;
                let coeff = gf.exp(log_coeff);

                // Decompose GF(2^16) multiply-by-coeff into four 8×8 GF(2) matrices.
                // For each 16-bit word w = (lo_byte, hi_byte):
                //   result_lo = M_ll * lo  ^  M_lh * hi
                //   result_hi = M_hl * lo  ^  M_hh * hi
                //
                // gf2p8affineqb uses byte (7-row) of the u64 matrix operand for
                // output bit `row` — so store each row at position (7-row)*8.
                let mut m_ll = 0u64; // lo input byte → lo output byte
                let mut m_lh = 0u64; // hi input byte → lo output byte
                let mut m_hl = 0u64; // lo input byte → hi output byte
                let mut m_hh = 0u64; // hi input byte → hi output byte
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

                // Each 128-bit lane has two qwords: the low qword handles lo bytes
                // (positions 0..7 after de-interleave) and the high qword handles hi
                // bytes (positions 8..15). Alternating the two matrices in adjacent
                // qwords lets one vgf2p8affineqb cover both contributions at once.
                // _mm512_set_epi64 takes arguments from high (e7) to low (e0).
                let mat_lo = _mm512_set_epi64(
                    m_lh as i64,
                    m_ll as i64, // lane 3: hi→lo, lo→lo
                    m_lh as i64,
                    m_ll as i64, // lane 2
                    m_lh as i64,
                    m_ll as i64, // lane 1
                    m_lh as i64,
                    m_ll as i64, // lane 0
                );
                let mat_hi = _mm512_set_epi64(
                    m_hh as i64,
                    m_hl as i64, // lane 3: hi→hi, lo→hi
                    m_hh as i64,
                    m_hl as i64, // lane 2
                    m_hh as i64,
                    m_hl as i64, // lane 1
                    m_hh as i64,
                    m_hl as i64, // lane 0
                );

                let mut table_low = [0u16; 256];
                let mut table_high = [0u16; 256];
                for b in 0..=255usize {
                    table_low[b] = gf.mul(b as u16, coeff);
                    table_high[b] = gf.mul((b as u16) << 8, coeff);
                }

                (mat_lo, mat_hi, table_low, table_high)
            })
            .collect();

        // 2D parallel loop: outer = recovery block groups, inner = 32 KiB chunks of
        // each recovery buffer. 4 × 32 KiB = 128 KiB fits comfortably in L2 on all
        // current AVX-512 CPUs (512 KB L2 on Ice Lake/Sapphire Rapids).
        let chunk_size = 16384usize;

        buffers
            .par_chunks_mut(4)
            .enumerate()
            .for_each(|(group_idx, buf_group)| {
                let i = group_idx * 4;
                match buf_group {
                    // 4-way: one input load + deinterleave feeds 4 recovery blocks.
                    [buf_a, buf_b, buf_c, buf_d] => {
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
                                    let blocks_64 = byte_len / 64;
                                    let remainder = byte_len % 64;

                                    for q_idx in 0..n_queued {
                                        let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                            all_tables[base_a + q_idx];
                                        let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                            all_tables[base_b + q_idx];
                                        let (mat_lo_c, mat_hi_c, ref tlow_c, ref thigh_c) =
                                            all_tables[base_c + q_idx];
                                        let (mat_lo_d, mat_hi_d, ref tlow_d, ref thigh_d) =
                                            all_tables[base_d + q_idx];
                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m512i;
                                        let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m512i;
                                        let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m512i;
                                        let mut ptr_c = chunk_c.as_mut_ptr() as *mut __m512i;
                                        let mut ptr_d = chunk_d.as_mut_ptr() as *mut __m512i;
                                        let end = ptr_in.add(blocks_64);

                                        while ptr_in < end {
                                            let input = _mm512_loadu_si512(ptr_in.cast());
                                            let separated = _mm512_shuffle_epi8(input, deint_mask);

                                            macro_rules! gfni512_block {
                                                ($mat_lo:expr, $mat_hi:expr, $ptr:expr) => {{
                                                    let vlo = _mm512_gf2p8affine_epi64_epi8(
                                                        separated, $mat_lo, 0,
                                                    );
                                                    let vhi = _mm512_gf2p8affine_epi64_epi8(
                                                        separated, $mat_hi, 0,
                                                    );
                                                    let out = _mm512_unpacklo_epi8(
                                                        _mm512_xor_si512(
                                                            vlo,
                                                            _mm512_bsrli_epi128::<8>(vlo),
                                                        ),
                                                        _mm512_xor_si512(
                                                            vhi,
                                                            _mm512_bsrli_epi128::<8>(vhi),
                                                        ),
                                                    );
                                                    _mm512_storeu_si512(
                                                        ($ptr as *mut __m512i).cast(),
                                                        _mm512_xor_si512(
                                                            _mm512_loadu_si512(
                                                                ($ptr as *const __m512i).cast(),
                                                            ),
                                                            out,
                                                        ),
                                                    );
                                                }};
                                            }

                                            gfni512_block!(mat_lo_a, mat_hi_a, ptr_a);
                                            gfni512_block!(mat_lo_b, mat_hi_b, ptr_b);
                                            gfni512_block!(mat_lo_c, mat_hi_c, ptr_c);
                                            gfni512_block!(mat_lo_d, mat_hi_d, ptr_d);

                                            ptr_in = ptr_in.add(1);
                                            ptr_a = ptr_a.add(1);
                                            ptr_b = ptr_b.add(1);
                                            ptr_c = ptr_c.add(1);
                                            ptr_d = ptr_d.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_64 * 32;
                                            let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                            let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                            let mut pw_c = chunk_c[ow..].as_mut_ptr();
                                            let mut pw_d = chunk_d[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_64 * 64..].as_ptr();
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
                        let base_a = i * n_queued;
                        let base_b = (i + 1) * n_queued;
                        buf_a
                            .par_chunks_mut(chunk_size)
                            .zip(buf_b.par_chunks_mut(chunk_size))
                            .enumerate()
                            .for_each(|(chunk_idx, (chunk_a, chunk_b))| unsafe {
                                let byte_offset = chunk_idx * chunk_size * 2;
                                let byte_len = chunk_a.len() * 2;
                                let blocks_64 = byte_len / 64;
                                let remainder = byte_len % 64;

                                for q_idx in 0..n_queued {
                                    let (mat_lo_a, mat_hi_a, ref tlow_a, ref thigh_a) =
                                        all_tables[base_a + q_idx];
                                    let (mat_lo_b, mat_hi_b, ref tlow_b, ref thigh_b) =
                                        all_tables[base_b + q_idx];
                                    let slice_chunk =
                                        &queued[q_idx][byte_offset..byte_offset + byte_len];

                                    let mut ptr_in = slice_chunk.as_ptr() as *const __m512i;
                                    let mut ptr_a = chunk_a.as_mut_ptr() as *mut __m512i;
                                    let mut ptr_b = chunk_b.as_mut_ptr() as *mut __m512i;
                                    let end = ptr_in.add(blocks_64);

                                    while ptr_in < end {
                                        let input = _mm512_loadu_si512(ptr_in.cast());
                                        let separated = _mm512_shuffle_epi8(input, deint_mask);

                                        // Block A
                                        let vlo_a =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_lo_a, 0);
                                        let new_lo_a = _mm512_xor_si512(
                                            vlo_a,
                                            _mm512_bsrli_epi128::<8>(vlo_a),
                                        );
                                        let vhi_a =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_hi_a, 0);
                                        let new_hi_a = _mm512_xor_si512(
                                            vhi_a,
                                            _mm512_bsrli_epi128::<8>(vhi_a),
                                        );
                                        let out_a = _mm512_unpacklo_epi8(new_lo_a, new_hi_a);
                                        let prev_a = _mm512_loadu_si512(ptr_a.cast());
                                        _mm512_storeu_si512(
                                            ptr_a.cast(),
                                            _mm512_xor_si512(prev_a, out_a),
                                        );

                                        // Block B — reuses `separated`
                                        let vlo_b =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_lo_b, 0);
                                        let new_lo_b = _mm512_xor_si512(
                                            vlo_b,
                                            _mm512_bsrli_epi128::<8>(vlo_b),
                                        );
                                        let vhi_b =
                                            _mm512_gf2p8affine_epi64_epi8(separated, mat_hi_b, 0);
                                        let new_hi_b = _mm512_xor_si512(
                                            vhi_b,
                                            _mm512_bsrli_epi128::<8>(vhi_b),
                                        );
                                        let out_b = _mm512_unpacklo_epi8(new_lo_b, new_hi_b);
                                        let prev_b = _mm512_loadu_si512(ptr_b.cast());
                                        _mm512_storeu_si512(
                                            ptr_b.cast(),
                                            _mm512_xor_si512(prev_b, out_b),
                                        );

                                        ptr_in = ptr_in.add(1);
                                        ptr_a = ptr_a.add(1);
                                        ptr_b = ptr_b.add(1);
                                    }

                                    if remainder > 0 {
                                        let ow = blocks_64 * 32;
                                        let mut pw_a = chunk_a[ow..].as_mut_ptr();
                                        let mut pw_b = chunk_b[ow..].as_mut_ptr();
                                        let mut p_in = slice_chunk[blocks_64 * 64..].as_ptr();
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
                    // Fallback for 1 or 3 remaining buffers (2 is handled
                    // above). Previously only `[buf_a]` (exactly 1) was
                    // matched here, with everything else — crucially, a
                    // remainder of *3*, which is exactly what
                    // `par_chunks_mut(4)` leaves whenever `recovery_count %
                    // 4 == 3` — silently falling through to an empty `_ =>
                    // {}` arm. Those recovery blocks were never written to
                    // at all, left as zero-initialized `buffers` entries
                    // instead of real recovery data: this was the root
                    // cause of issue #51's "silent data corruption" (the
                    // decoder faithfully reconstructing from all-zero
                    // recovery blocks). See the issue for the investigation
                    // history — this is what every local repro attempt
                    // couldn't catch, since it only reproduces on hardware
                    // with AVX-512+GFNI, which none of those attempts had.
                    rest => {
                        for (j, buf_a) in rest.iter_mut().enumerate() {
                            let base = (i + j) * n_queued;
                            buf_a.par_chunks_mut(chunk_size).enumerate().for_each(
                                |(chunk_idx, chunk_a)| unsafe {
                                    let byte_offset = chunk_idx * chunk_size * 2;
                                    let byte_len = chunk_a.len() * 2;
                                    let blocks_64 = byte_len / 64;
                                    let remainder = byte_len % 64;

                                    for q_idx in 0..n_queued {
                                        let (mat_lo, mat_hi, ref table_low, ref table_high) =
                                            all_tables[base + q_idx];
                                        let slice_chunk =
                                            &queued[q_idx][byte_offset..byte_offset + byte_len];

                                        let mut ptr_buf = chunk_a.as_mut_ptr() as *mut __m512i;
                                        let mut ptr_in = slice_chunk.as_ptr() as *const __m512i;
                                        let end = ptr_in.add(blocks_64);

                                        while ptr_in < end {
                                            let input = _mm512_loadu_si512(ptr_in.cast());
                                            let separated = _mm512_shuffle_epi8(input, deint_mask);

                                            let v_lo =
                                                _mm512_gf2p8affine_epi64_epi8(separated, mat_lo, 0);
                                            let new_lo = _mm512_xor_si512(
                                                v_lo,
                                                _mm512_bsrli_epi128::<8>(v_lo),
                                            );
                                            let v_hi =
                                                _mm512_gf2p8affine_epi64_epi8(separated, mat_hi, 0);
                                            let new_hi = _mm512_xor_si512(
                                                v_hi,
                                                _mm512_bsrli_epi128::<8>(v_hi),
                                            );
                                            let out = _mm512_unpacklo_epi8(new_lo, new_hi);
                                            let prev = _mm512_loadu_si512(ptr_buf.cast());
                                            _mm512_storeu_si512(
                                                ptr_buf.cast(),
                                                _mm512_xor_si512(prev, out),
                                            );

                                            ptr_in = ptr_in.add(1);
                                            ptr_buf = ptr_buf.add(1);
                                        }

                                        if remainder > 0 {
                                            let ow = blocks_64 * 32;
                                            let mut pw = chunk_a[ow..].as_mut_ptr();
                                            let mut p_in = slice_chunk[blocks_64 * 64..].as_ptr();
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
}
