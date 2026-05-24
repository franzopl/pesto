//! x86-64 yEnc encoders: SSSE3 and AVX2.

use super::scalar::encode_scalar;
use super::tables::{LEN_TABLE, SHUFFLE_TABLE};

/// SSSE3-accelerated yEnc encoder. Falls back to [`encode_scalar`] when the
/// CPU does not support SSSE3 (detected at runtime via `is_x86_feature_detected!`).
///
/// Produces identical output to [`encode_scalar`] for all inputs.
pub fn encode_ssse3(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    if is_x86_feature_detected!("ssse3") {
        unsafe { encode_ssse3_impl(out, data, line_len) }
    } else {
        encode_scalar(out, data, line_len)
    }
}

pub fn encode_avx2(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    if is_x86_feature_detected!("avx2") {
        unsafe { encode_avx2_impl(out, data, line_len) }
    } else {
        encode_ssse3(out, data, line_len)
    }
}

pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    // Prefer SSSE3 over AVX2 even when AVX2 is available.
    // On hybrid CPUs (Intel 12th gen+) E-cores execute AVX2 ~5% slower than
    // SSSE3 at line_len=128, while P-cores are within noise (<0.3% difference).
    // SSSE3 is the safe choice across all core types with no P-core penalty.
    // AVX2 remains available via encode_avx2() for explicit benchmarking or
    // future multi-line strategies that would amortise per-line overhead.
    if is_x86_feature_detected!("ssse3") {
        unsafe { encode_ssse3_impl(out, data, line_len) }
    } else {
        encode_scalar(out, data, line_len)
    }
}

#[target_feature(enable = "ssse3")]
unsafe fn encode_ssse3_impl(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    use std::arch::x86_64::*;
    let line_len = line_len.max(1);
    if data.is_empty() {
        return;
    }
    out.reserve(data.len() * 2 + (data.len() / line_len + 1) * 2);
    let last = data.len() - 1;
    let add42 = _mm_set1_epi8(42i8);
    let v_add64 = _mm_set1_epi8(64i8);
    let v_lookup = _mm_setr_epi8(-32, 0, 0, 19, 0, 0, 0, 0, 0, 0, -42, 0, 0, -29, 0, 0);
    let v_eq_const = _mm_set1_epi8(b'=' as i8);
    let mut i = 0usize;
    let mut col = 0usize;
    let out_base = out.as_mut_ptr();
    let mut out_ptr = out_base.add(out.len());
    while i < data.len() {
        if col == 0 {
            let _at_line_end = line_len == 1 || i == last;
            let b = data[i];
            let e = b.wrapping_add(42);
            let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
            let positional = e == 0x09 || e == 0x20 || e == 0x2E;
            if critical || positional {
                *out_ptr = b'=';
                *out_ptr.add(1) = e.wrapping_add(64);
                out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e;
                out_ptr = out_ptr.add(1);
            }
            col += 1;
            if col == line_len {
                std::ptr::copy_nonoverlapping(b"\r\n".as_ptr(), out_ptr, 2);
                out_ptr = out_ptr.add(2);
                col = 0;
            }
            i += 1;
            continue;
        }
        let safe = if line_len > 1 {
            let till_line_end = line_len - 1 - col;
            let till_data_end = last.saturating_sub(i);
            till_line_end.min(till_data_end)
        } else {
            0
        };
        let mut safe_rem = safe;
        while safe_rem >= 32 {
            let chunk_a = _mm_loadu_si128(data.as_ptr().add(i) as *const __m128i);
            let chunk_b = _mm_loadu_si128(data.as_ptr().add(i + 16) as *const __m128i);

            let any_a = _mm_cmpeq_epi8(_mm_shuffle_epi8(v_lookup, _mm_abs_epi8(chunk_a)), chunk_a);
            let any_b = _mm_cmpeq_epi8(_mm_shuffle_epi8(v_lookup, _mm_abs_epi8(chunk_b)), chunk_b);

            let mask_a = _mm_movemask_epi8(any_a) as u32;
            let mask_b = _mm_movemask_epi8(any_b) as u32;

            let shifted_a = _mm_add_epi8(chunk_a, add42);
            let shifted_b = _mm_add_epi8(chunk_b, add42);

            if (mask_a | mask_b) == 0 {
                _mm_storeu_si128(out_ptr as *mut __m128i, shifted_a);
                _mm_storeu_si128(out_ptr.add(16) as *mut __m128i, shifted_b);
                out_ptr = out_ptr.add(32);
            } else {
                if mask_a == 0 {
                    _mm_storeu_si128(out_ptr as *mut __m128i, shifted_a);
                    out_ptr = out_ptr.add(16);
                } else {
                    let escaped_a = _mm_add_epi8(shifted_a, _mm_and_si128(any_a, v_add64));
                    let m_lo = (mask_a & 0xFF) as usize;
                    let de_lo = _mm_unpacklo_epi64(escaped_a, v_eq_const);
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_shuffle_epi8(
                            de_lo,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                    let m_hi = ((mask_a >> 8) & 0xFF) as usize;
                    let de_hi = _mm_unpackhi_epi64(escaped_a, v_eq_const);
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_shuffle_epi8(
                            de_hi,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
                }
                if mask_b == 0 {
                    _mm_storeu_si128(out_ptr as *mut __m128i, shifted_b);
                    out_ptr = out_ptr.add(16);
                } else {
                    let escaped_b = _mm_add_epi8(shifted_b, _mm_and_si128(any_b, v_add64));
                    let m_lo = (mask_b & 0xFF) as usize;
                    let de_lo = _mm_unpacklo_epi64(escaped_b, v_eq_const);
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_shuffle_epi8(
                            de_lo,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                    let m_hi = ((mask_b >> 8) & 0xFF) as usize;
                    let de_hi = _mm_unpackhi_epi64(escaped_b, v_eq_const);
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_shuffle_epi8(
                            de_hi,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
                }
            }
            i += 32;
            col += 32;
            safe_rem -= 32;
        }
        while safe_rem >= 16 {
            let chunk = _mm_loadu_si128(data.as_ptr().add(i) as *const __m128i);
            let any = _mm_cmpeq_epi8(_mm_shuffle_epi8(v_lookup, _mm_abs_epi8(chunk)), chunk);
            let mask = _mm_movemask_epi8(any) as u32;
            let shifted = _mm_add_epi8(chunk, add42);
            if mask == 0 {
                _mm_storeu_si128(out_ptr as *mut __m128i, shifted);
                out_ptr = out_ptr.add(16);
            } else {
                let escaped = _mm_add_epi8(shifted, _mm_and_si128(any, v_add64));
                let m_lo = (mask & 0xFF) as usize;
                let de_lo = _mm_unpacklo_epi64(escaped, v_eq_const);
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_shuffle_epi8(
                        de_lo,
                        _mm_loadu_si128(
                            SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                        ),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                let m_hi = ((mask >> 8) & 0xFF) as usize;
                let de_hi = _mm_unpackhi_epi64(escaped, v_eq_const);
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_shuffle_epi8(
                        de_hi,
                        _mm_loadu_si128(
                            SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                        ),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
            }
            i += 16;
            col += 16;
            safe_rem -= 16;
        }
        while safe_rem > 0 {
            let e = data[i].wrapping_add(42);
            if matches!(e, 0x00 | 0x0A | 0x0D | 0x3D) {
                *out_ptr = b'=';
                *out_ptr.add(1) = e.wrapping_add(64);
                out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e;
                out_ptr = out_ptr.add(1);
            }
            i += 1;
            col += 1;
            safe_rem -= 1;
        }
        if i < data.len() {
            let at_line_end = col + 1 == line_len || i == last;
            let b = data[i];
            let e = b.wrapping_add(42);
            let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
            let positional =
                ((e == 0x09 || e == 0x20) && (col == 0 || at_line_end)) || (e == 0x2E && col == 0);
            if critical || positional {
                *out_ptr = b'=';
                *out_ptr.add(1) = e.wrapping_add(64);
                out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e;
                out_ptr = out_ptr.add(1);
            }
            col += 1;
            if col == line_len {
                std::ptr::copy_nonoverlapping(b"\r\n".as_ptr(), out_ptr, 2);
                out_ptr = out_ptr.add(2);
                col = 0;
            }
            i += 1;
        }
    }
    if col != 0 {
        std::ptr::copy_nonoverlapping(b"\r\n".as_ptr(), out_ptr, 2);
        out_ptr = out_ptr.add(2);
    }
    out.set_len(out_ptr.offset_from(out_base) as usize);
}

#[target_feature(enable = "avx2")]
unsafe fn encode_avx2_impl(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    use std::arch::x86_64::*;
    let line_len = line_len.max(1);
    if data.is_empty() {
        return;
    }
    out.reserve(data.len() * 2 + (data.len() / line_len + 1) * 2);
    let last = data.len() - 1;
    let add42 = _mm256_set1_epi8(42i8);
    let v_add64 = _mm256_set1_epi8(64i8);
    let v_lookup = _mm256_setr_epi8(
        -32, 0, 0, 19, 0, 0, 0, 0, 0, 0, -42, 0, 0, -29, 0, 0, -32, 0, 0, 19, 0, 0, 0, 0, 0, 0,
        -42, 0, 0, -29, 0, 0,
    );
    let v_eq_const = _mm_set1_epi8(b'=' as i8);
    let mut i = 0usize;
    let mut col = 0usize;
    let out_base = out.as_mut_ptr();
    let mut out_ptr = out_base.add(out.len());
    while i < data.len() {
        if col == 0 {
            let _at_line_end = line_len == 1 || i == last;
            let b = data[i];
            let e = b.wrapping_add(42);
            let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
            let positional = e == 0x09 || e == 0x20 || e == 0x2E;
            if critical || positional {
                *out_ptr = b'=';
                *out_ptr.add(1) = e.wrapping_add(64);
                out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e;
                out_ptr = out_ptr.add(1);
            }
            col += 1;
            if col == line_len {
                std::ptr::copy_nonoverlapping(b"\r\n".as_ptr(), out_ptr, 2);
                out_ptr = out_ptr.add(2);
                col = 0;
            }
            i += 1;
            continue;
        }
        let safe = if line_len > 1 {
            let till_line_end = line_len - 1 - col;
            let till_data_end = last.saturating_sub(i);
            till_line_end.min(till_data_end)
        } else {
            0
        };
        let mut safe_rem = safe;
        while safe_rem >= 64 {
            let chunk0 = _mm256_loadu_si256(data.as_ptr().add(i) as *const __m256i);
            let chunk1 = _mm256_loadu_si256(data.as_ptr().add(i + 32) as *const __m256i);

            let any0 = _mm256_cmpeq_epi8(
                _mm256_shuffle_epi8(v_lookup, _mm256_abs_epi8(chunk0)),
                chunk0,
            );
            let any1 = _mm256_cmpeq_epi8(
                _mm256_shuffle_epi8(v_lookup, _mm256_abs_epi8(chunk1)),
                chunk1,
            );

            let mask0 = _mm256_movemask_epi8(any0) as u32;
            let mask1 = _mm256_movemask_epi8(any1) as u32;

            let shifted0 = _mm256_add_epi8(chunk0, add42);
            let shifted1 = _mm256_add_epi8(chunk1, add42);

            if (mask0 | mask1) == 0 {
                _mm256_storeu_si256(out_ptr as *mut __m256i, shifted0);
                _mm256_storeu_si256(out_ptr.add(32) as *mut __m256i, shifted1);
                out_ptr = out_ptr.add(64);
            } else {
                for (_chunk, any, mask, shifted) in [
                    (chunk0, any0, mask0, shifted0),
                    (chunk1, any1, mask1, shifted1),
                ] {
                    if mask == 0 {
                        _mm256_storeu_si256(out_ptr as *mut __m256i, shifted);
                        out_ptr = out_ptr.add(32);
                    } else {
                        let escaped = _mm256_add_epi8(shifted, _mm256_and_si256(any, v_add64));
                        let s_lo = _mm256_extracti128_si256(escaped, 0);
                        let s_hi = _mm256_extracti128_si256(escaped, 1);
                        let m16_lo = mask & 0xFFFF;
                        if m16_lo == 0 {
                            _mm_storeu_si128(out_ptr as *mut __m128i, s_lo);
                            out_ptr = out_ptr.add(16);
                        } else {
                            let m_lo = (m16_lo & 0xFF) as usize;
                            _mm_storeu_si128(
                                out_ptr as *mut __m128i,
                                _mm_shuffle_epi8(
                                    _mm_unpacklo_epi64(s_lo, v_eq_const),
                                    _mm_loadu_si128(SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr()
                                        as *const __m128i),
                                ),
                            );
                            out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                            let m_hi = ((m16_lo >> 8) & 0xFF) as usize;
                            _mm_storeu_si128(
                                out_ptr as *mut __m128i,
                                _mm_shuffle_epi8(
                                    _mm_unpackhi_epi64(s_lo, v_eq_const),
                                    _mm_loadu_si128(SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr()
                                        as *const __m128i),
                                ),
                            );
                            out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
                        }
                        let m16_hi = (mask >> 16) & 0xFFFF;
                        if m16_hi == 0 {
                            _mm_storeu_si128(out_ptr as *mut __m128i, s_hi);
                            out_ptr = out_ptr.add(16);
                        } else {
                            let m_lo = (m16_hi & 0xFF) as usize;
                            _mm_storeu_si128(
                                out_ptr as *mut __m128i,
                                _mm_shuffle_epi8(
                                    _mm_unpacklo_epi64(s_hi, v_eq_const),
                                    _mm_loadu_si128(SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr()
                                        as *const __m128i),
                                ),
                            );
                            out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                            let m_hi = ((m16_hi >> 8) & 0xFF) as usize;
                            _mm_storeu_si128(
                                out_ptr as *mut __m128i,
                                _mm_shuffle_epi8(
                                    _mm_unpackhi_epi64(s_hi, v_eq_const),
                                    _mm_loadu_si128(SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr()
                                        as *const __m128i),
                                ),
                            );
                            out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
                        }
                    }
                }
            }
            i += 64;
            col += 64;
            safe_rem -= 64;
        }
        while safe_rem >= 32 {
            let chunk = _mm256_loadu_si256(data.as_ptr().add(i) as *const __m256i);
            let any =
                _mm256_cmpeq_epi8(_mm256_shuffle_epi8(v_lookup, _mm256_abs_epi8(chunk)), chunk);
            let mask = _mm256_movemask_epi8(any) as u32;
            let shifted = _mm256_add_epi8(chunk, add42);

            if mask == 0 {
                _mm256_storeu_si256(out_ptr as *mut __m256i, shifted);
                out_ptr = out_ptr.add(32);
            } else {
                let escaped = _mm256_add_epi8(shifted, _mm256_and_si256(any, v_add64));
                let s_lo = _mm256_extracti128_si256(escaped, 0);
                let s_hi = _mm256_extracti128_si256(escaped, 1);
                let m16_lo = mask & 0xFFFF;
                if m16_lo == 0 {
                    _mm_storeu_si128(out_ptr as *mut __m128i, s_lo);
                    out_ptr = out_ptr.add(16);
                } else {
                    let m_lo = (m16_lo & 0xFF) as usize;
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_shuffle_epi8(
                            _mm_unpacklo_epi64(s_lo, v_eq_const),
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                    let m_hi = ((m16_lo >> 8) & 0xFF) as usize;
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_shuffle_epi8(
                            _mm_unpackhi_epi64(s_lo, v_eq_const),
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
                }
                let m16_hi = (mask >> 16) & 0xFFFF;
                if m16_hi == 0 {
                    _mm_storeu_si128(out_ptr as *mut __m128i, s_hi);
                    out_ptr = out_ptr.add(16);
                } else {
                    let m_lo = (m16_hi & 0xFF) as usize;
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_shuffle_epi8(
                            _mm_unpacklo_epi64(s_hi, v_eq_const),
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                    let m_hi = ((m16_hi >> 8) & 0xFF) as usize;
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_shuffle_epi8(
                            _mm_unpackhi_epi64(s_hi, v_eq_const),
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
                }
            }
            i += 32;
            col += 32;
            safe_rem -= 32;
        }
        let add42_128 = _mm_set1_epi8(42i8);
        let v_add64_128 = _mm_set1_epi8(64i8);
        let v_lookup_128 = _mm_setr_epi8(-32, 0, 0, 19, 0, 0, 0, 0, 0, 0, -42, 0, 0, -29, 0, 0);
        while safe_rem >= 16 {
            let chunk = _mm_loadu_si128(data.as_ptr().add(i) as *const __m128i);
            let any = _mm_cmpeq_epi8(_mm_shuffle_epi8(v_lookup_128, _mm_abs_epi8(chunk)), chunk);
            let mask = _mm_movemask_epi8(any) as u32;
            let shifted = _mm_add_epi8(chunk, add42_128);
            if mask == 0 {
                _mm_storeu_si128(out_ptr as *mut __m128i, shifted);
                out_ptr = out_ptr.add(16);
            } else {
                let escaped = _mm_add_epi8(shifted, _mm_and_si128(any, v_add64_128));
                let m_lo = (mask & 0xFF) as usize;
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_shuffle_epi8(
                        _mm_unpacklo_epi64(escaped, v_eq_const),
                        _mm_loadu_si128(
                            SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                        ),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                let m_hi = ((mask >> 8) & 0xFF) as usize;
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_shuffle_epi8(
                        _mm_unpackhi_epi64(escaped, v_eq_const),
                        _mm_loadu_si128(
                            SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                        ),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
            }
            i += 16;
            col += 16;
            safe_rem -= 16;
        }
        while safe_rem > 0 {
            let e = data[i].wrapping_add(42);
            if matches!(e, 0x00 | 0x0A | 0x0D | 0x3D) {
                *out_ptr = b'=';
                *out_ptr.add(1) = e.wrapping_add(64);
                out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e;
                out_ptr = out_ptr.add(1);
            }
            i += 1;
            col += 1;
            safe_rem -= 1;
        }
        if i < data.len() {
            let at_line_end = col + 1 == line_len || i == last;
            let b = data[i];
            let e = b.wrapping_add(42);
            let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
            let positional =
                ((e == 0x09 || e == 0x20) && (col == 0 || at_line_end)) || (e == 0x2E && col == 0);
            if critical || positional {
                *out_ptr = b'=';
                *out_ptr.add(1) = e.wrapping_add(64);
                out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e;
                out_ptr = out_ptr.add(1);
            }
            col += 1;
            if col == line_len {
                std::ptr::copy_nonoverlapping(b"\r\n".as_ptr(), out_ptr, 2);
                out_ptr = out_ptr.add(2);
                col = 0;
            }
            i += 1;
        }
    }
    if col != 0 {
        std::ptr::copy_nonoverlapping(b"\r\n".as_ptr(), out_ptr, 2);
        out_ptr = out_ptr.add(2);
    }
    out.set_len(out_ptr.offset_from(out_base) as usize);
}
