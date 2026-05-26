//! aarch64 yEnc encoders: NEON.

use super::tables::{LEN_TABLE, SHUFFLE_TABLE};

pub fn encode_neon(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    unsafe { encode_neon_impl(out, data, line_len) }
}

pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    unsafe { encode_neon_impl(out, data, line_len) }
}

/// # Safety
/// Caller must ensure `data` is a valid slice. This function uses NEON
/// intrinsics which are always available on aarch64.
pub unsafe fn encoded_size_neon(data: &[u8], line_len: usize) -> usize {
    use std::arch::aarch64::*;
    let last = data.len() - 1;
    let mut escapes = 0usize;
    let mut i = 0usize;
    let mut col = 0usize;

    let v_add42 = vdupq_n_u8(42);
    let v_lookup = vld1q_u8([255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 255, 0, 0].as_ptr());
    let v_eq = vdupq_n_u8(0x3D);

    while i < data.len() {
        if col == 0 || col + 16 >= line_len || i + 16 >= data.len() {
            let e = data[i].wrapping_add(42);
            let at_line_start = col == 0;
            let at_line_end = col + 1 == line_len || i == last;
            let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
            let positional = ((e == 0x09 || e == 0x20) && (at_line_start || at_line_end))
                || (e == 0x2E && at_line_start);
            if critical || positional {
                escapes += 1;
            }
            col += 1;
            if col == line_len {
                col = 0;
            }
            i += 1;
            continue;
        }

        let till_line_end = line_len - 1 - col;
        let till_data_end = last.saturating_sub(i);
        let mut safe_rem = till_line_end.min(till_data_end);

        while safe_rem >= 16 {
            let chunk = vld1q_u8(data.as_ptr().add(i));
            let shifted = vaddq_u8(chunk, v_add42);
            let mask = vorrq_u8(vqtbl1q_u8(v_lookup, shifted), vceqq_u8(shifted, v_eq));

            let count = vaddvq_u8(vshrq_n_u8(mask, 7)) as usize;
            escapes += count;

            i += 16;
            col += 16;
            safe_rem -= 16;
        }

        while safe_rem > 0 {
            let e = data[i].wrapping_add(42);
            if matches!(e, 0x00 | 0x0A | 0x0D | 0x3D) {
                escapes += 1;
            }
            i += 1;
            col += 1;
            safe_rem -= 1;
        }
    }

    let lines = data.len().div_ceil(line_len);
    data.len() + escapes + lines * 2
}

unsafe fn encode_neon_impl(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    use std::arch::aarch64::*;
    let line_len = line_len.max(1);
    if data.is_empty() {
        return;
    }
    // Reservation is now handled by encode_part, but we keep a local
    // reserve for direct calls.
    out.reserve(data.len() * 2 + (data.len() / line_len + 1) * 2);
    let last = data.len() - 1;
    let v_add42 = vdupq_n_u8(42);
    let v_lookup = vld1q_u8([255, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 0, 0, 255, 0, 0].as_ptr());
    let v_eq = vdupq_n_u8(0x3D);
    let v_add64 = vdupq_n_u8(64);
    let v_eq_const = vdupq_n_u8(b'=');
    let v_weights = vld1q_u8([1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128].as_ptr());

    let mut i = 0usize;
    let mut col = 0usize;
    let out_base = out.as_mut_ptr();
    let mut out_ptr = out_base.add(out.len());

    while i < data.len() {
        if col == 0 {
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
                *out_ptr = b'\r';
                *out_ptr.add(1) = b'\n';
                out_ptr = out_ptr.add(2);
                col = 0;
            }
            i += 1;
            continue;
        }

        let till_line_end = line_len - 1 - col;
        let till_data_end = last.saturating_sub(i);
        let mut safe_rem = till_line_end.min(till_data_end);

        while safe_rem >= 64 {
            let chunk0 = vld1q_u8(data.as_ptr().add(i));
            let chunk1 = vld1q_u8(data.as_ptr().add(i + 16));
            let chunk2 = vld1q_u8(data.as_ptr().add(i + 32));
            let chunk3 = vld1q_u8(data.as_ptr().add(i + 48));
            let s0 = vaddq_u8(chunk0, v_add42);
            let s1 = vaddq_u8(chunk1, v_add42);
            let s2 = vaddq_u8(chunk2, v_add42);
            let s3 = vaddq_u8(chunk3, v_add42);

            let m0 = vorrq_u8(vqtbl1q_u8(v_lookup, s0), vceqq_u8(s0, v_eq));
            let m1 = vorrq_u8(vqtbl1q_u8(v_lookup, s1), vceqq_u8(s1, v_eq));
            let m2 = vorrq_u8(vqtbl1q_u8(v_lookup, s2), vceqq_u8(s2, v_eq));
            let m3 = vorrq_u8(vqtbl1q_u8(v_lookup, s3), vceqq_u8(s3, v_eq));

            if vmaxvq_u8(vorrq_u8(vorrq_u8(m0, m1), vorrq_u8(m2, m3))) == 0 {
                vst1q_u8_x4(out_ptr, uint8x16x4_t(s0, s1, s2, s3));
                out_ptr = out_ptr.add(64);
                i += 64;
                col += 64;
                safe_rem -= 64;
            } else {
                for (chunk_s, chunk_m) in [(s0, m0), (s1, m1), (s2, m2), (s3, m3)] {
                    if vmaxvq_u8(chunk_m) == 0 {
                        vst1q_u8(out_ptr, chunk_s);
                        out_ptr = out_ptr.add(16);
                    } else {
                        let weighted = vandq_u8(chunk_m, v_weights);
                        let sum_lo = vaddv_u8(vget_low_u8(weighted)) as usize;
                        let sum_hi = vaddv_u8(vget_high_u8(weighted)) as usize;

                        // Pre-add 64 to escaped characters
                        let escaped_chunk_s = vaddq_u8(chunk_s, vandq_u8(chunk_m, v_add64));

                        let de_lo =
                            vcombine_u8(vget_low_u8(escaped_chunk_s), vget_low_u8(v_eq_const));
                        let res_lo = vqtbl1q_u8(
                            de_lo,
                            vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_lo).as_ptr()),
                        );
                        vst1q_u8(out_ptr, res_lo);
                        out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_lo) as usize);

                        let de_hi =
                            vcombine_u8(vget_high_u8(escaped_chunk_s), vget_low_u8(v_eq_const));
                        let res_hi = vqtbl1q_u8(
                            de_hi,
                            vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_hi).as_ptr()),
                        );
                        vst1q_u8(out_ptr, res_hi);
                        out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_hi) as usize);
                    }
                    i += 16;
                    col += 16;
                }
                safe_rem -= 64;
            }
        }

        while safe_rem >= 32 {
            let chunk0 = vld1q_u8(data.as_ptr().add(i));
            let chunk1 = vld1q_u8(data.as_ptr().add(i + 16));
            let s0 = vaddq_u8(chunk0, v_add42);
            let s1 = vaddq_u8(chunk1, v_add42);
            let m0 = vorrq_u8(vqtbl1q_u8(v_lookup, s0), vceqq_u8(s0, v_eq));
            let m1 = vorrq_u8(vqtbl1q_u8(v_lookup, s1), vceqq_u8(s1, v_eq));

            if vmaxvq_u8(vorrq_u8(m0, m1)) == 0 {
                vst1q_u8_x2(out_ptr, uint8x16x2_t(s0, s1));
                out_ptr = out_ptr.add(32);
                i += 32;
                col += 32;
                safe_rem -= 32;
            } else {
                // If any escapes in 32B, handle 16B chunks individually with shuffle expansion
                for (chunk_s, chunk_m) in [(s0, m0), (s1, m1)] {
                    if vmaxvq_u8(chunk_m) == 0 {
                        vst1q_u8(out_ptr, chunk_s);
                        out_ptr = out_ptr.add(16);
                    } else {
                        let weighted = vandq_u8(chunk_m, v_weights);
                        let sum_lo = vaddv_u8(vget_low_u8(weighted)) as usize;
                        let sum_hi = vaddv_u8(vget_high_u8(weighted)) as usize;

                        // Pre-add 64 to escaped characters
                        let escaped_chunk_s = vaddq_u8(chunk_s, vandq_u8(chunk_m, v_add64));

                        let de_lo =
                            vcombine_u8(vget_low_u8(escaped_chunk_s), vget_low_u8(v_eq_const));
                        let res_lo = vqtbl1q_u8(
                            de_lo,
                            vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_lo).as_ptr()),
                        );
                        vst1q_u8(out_ptr, res_lo);
                        out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_lo) as usize);

                        let de_hi =
                            vcombine_u8(vget_high_u8(escaped_chunk_s), vget_low_u8(v_eq_const));
                        let res_hi = vqtbl1q_u8(
                            de_hi,
                            vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_hi).as_ptr()),
                        );
                        vst1q_u8(out_ptr, res_hi);
                        out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_hi) as usize);
                    }
                    i += 16;
                    col += 16;
                }
                safe_rem -= 32;
            }
        }

        while safe_rem >= 16 {
            let chunk = vld1q_u8(data.as_ptr().add(i));
            let shifted = vaddq_u8(chunk, v_add42);
            let mask = vorrq_u8(vqtbl1q_u8(v_lookup, shifted), vceqq_u8(shifted, v_eq));

            if vmaxvq_u8(mask) == 0 {
                vst1q_u8(out_ptr, shifted);
                out_ptr = out_ptr.add(16);
            } else {
                let weighted = vandq_u8(mask, v_weights);
                let sum_low = vaddv_u8(vget_low_u8(weighted)) as usize;
                let sum_high = vaddv_u8(vget_high_u8(weighted)) as usize;

                // Pre-add 64 to escaped characters
                let escaped_shifted = vaddq_u8(shifted, vandq_u8(mask, v_add64));

                let de_lo = vcombine_u8(vget_low_u8(escaped_shifted), vget_low_u8(v_eq_const));
                let res_lo = vqtbl1q_u8(
                    de_lo,
                    vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_low).as_ptr()),
                );
                vst1q_u8(out_ptr, res_lo);
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_low) as usize);

                let de_hi = vcombine_u8(vget_high_u8(escaped_shifted), vget_low_u8(v_eq_const));
                let res_hi = vqtbl1q_u8(
                    de_hi,
                    vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_high).as_ptr()),
                );
                vst1q_u8(out_ptr, res_hi);
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_high) as usize);
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
            let positional = (e == 0x09 || e == 0x20) && at_line_end;
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
                *out_ptr = b'\r';
                *out_ptr.add(1) = b'\n';
                out_ptr = out_ptr.add(2);
                col = 0;
            }
            i += 1;
        }
    }

    if col != 0 {
        *out_ptr = b'\r';
        *out_ptr.add(1) = b'\n';
        out_ptr = out_ptr.add(2);
    }
    out.set_len(out_ptr.offset_from(out_base) as usize);
}
