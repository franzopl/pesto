//! yEnc encoder.
//!
//! Encodes raw file bytes into yEnc article bodies, including the `=ybegin`,
//! `=ypart` and `=yend` control lines, and splits files into multi-part
//! segments. This is the hot path — the inner [`encode_into`] loop allocates
//! nothing beyond the output buffer.
//!
//! Reference: the yEnc draft specification, <http://www.yenc.org/yenc-draft.1.3.txt>.

/// Default yEnc line length, in source bytes per line.
pub const DEFAULT_LINE_LENGTH: usize = 128;

// --- CRC-32 (IEEE polynomial, reflected — the variant used by yEnc) ---

/// Incremental CRC-32 calculator (IEEE polynomial, as used by yEnc).
#[derive(Debug, Clone)]
pub struct Crc32 {
    hasher: crc32fast::Hasher,
}

impl Crc32 {
    /// Start a fresh CRC-32 computation.
    pub fn new() -> Self {
        Self {
            hasher: crc32fast::Hasher::new(),
        }
    }

    /// Feed more bytes into the running checksum.
    pub fn update(&mut self, data: &[u8]) {
        self.hasher.update(data);
    }

    /// Produce the final CRC-32 value.
    pub fn finalize(&self) -> u32 {
        self.hasher.clone().finalize()
    }
}

impl Default for Crc32 {
    fn default() -> Self {
        Self::new()
    }
}

/// CRC-32 of a complete byte slice.
pub fn crc32(data: &[u8]) -> u32 {
    crc32fast::hash(data)
}

// --- Segmentation ---

/// Split a file of `file_size` bytes into `(offset, len)` segments of at most
/// `article_size` bytes each. An empty file yields a single empty segment so
/// it still produces one article.
pub fn segments(file_size: u64, article_size: usize) -> Vec<(u64, usize)> {
    let article_size = article_size.max(1) as u64;
    if file_size == 0 {
        return vec![(0, 0)];
    }
    let mut out = Vec::new();
    let mut offset = 0u64;
    while offset < file_size {
        let len = article_size.min(file_size - offset);
        out.push((offset, len as usize));
        offset += len;
    }
    out
}

// --- Encoding ---

/// Identifies one segment of a file to be encoded.
#[derive(Debug, Clone, Copy)]
pub struct PartSpec {
    /// 1-based part number.
    pub number: u32,
    /// Total number of parts for the file.
    pub total: u32,
    /// 0-based byte offset of this part within the file.
    pub offset: u64,
}

/// A yEnc-encoded segment, ready to be used as an NNTP article body.
#[derive(Debug)]
pub struct EncodedPart {
    /// 1-based part number.
    pub number: u32,
    /// Total number of parts for the file.
    pub total: u32,
    /// 1-based offset of the first byte of this part within the file.
    pub begin: u64,
    /// 1-based offset of the last byte of this part within the file.
    pub end: u64,
    /// CRC-32 of this part's raw bytes.
    pub crc32: u32,
    /// Full yEnc body: `=ybegin` / `=ypart` / data lines / `=yend`.
    pub body: Vec<u8>,
}

/// Encode one segment of a file into a complete yEnc article body.
///
/// `data` is the raw byte slice for this part. `file_size` is the size of the
/// whole file. When `spec.total > 1` a multi-part body is produced (with a
/// `=ypart` line); otherwise a single-part body is produced. `file_crc32`, if
/// supplied, is appended to the `=yend` line of multi-part articles.
pub fn encode_part(
    name: &str,
    file_size: u64,
    spec: PartSpec,
    data: &[u8],
    line_len: usize,
    file_crc32: Option<u32>,
) -> EncodedPart {
    let multipart = spec.total > 1;
    let part_crc = crc32(data);
    let begin = spec.offset + 1;
    let end = spec.offset + data.len() as u64;

    let ybegin = if multipart {
        format!(
            "=ybegin part={} total={} line={} size={} name={}\r\n",
            spec.number, spec.total, line_len, file_size, name
        )
    } else {
        format!(
            "=ybegin line={} size={} name={}\r\n",
            line_len, file_size, name
        )
    };

    let ypart = if multipart {
        Some(format!("=ypart begin={} end={}\r\n", begin, end))
    } else {
        None
    };

    let yend = if multipart {
        let mut s = format!(
            "=yend size={} part={} pcrc32={:08x}",
            data.len(),
            spec.number,
            part_crc
        );
        if let Some(file_crc) = file_crc32 {
            s.push_str(&format!(" crc32={:08x}", file_crc));
        }
        s.push_str("\r\n");
        s
    } else {
        format!("=yend size={} crc32={:08x}\r\n", data.len(), part_crc)
    };

    let data_encoded_size = encoded_size(data, line_len);
    let total_capacity = ybegin.len() + ypart.as_ref().map_or(0, |p| p.len()) + data_encoded_size + yend.len();

    let mut body = Vec::with_capacity(total_capacity);

    body.extend_from_slice(ybegin.as_bytes());
    if let Some(p) = ypart {
        body.extend_from_slice(p.as_bytes());
    }

    encode(&mut body, data, line_len);
    body.extend_from_slice(yend.as_bytes());

    EncodedPart {
        number: spec.number,
        total: spec.total,
        begin,
        end,
        crc32: part_crc,
        body,
    }
}

/// Compute the exact number of bytes that encoding `data` with `line_len` will
/// produce, including escape sequences and `\r\n` line terminators.
///
/// Callers can use this to pre-reserve output buffer capacity before encoding,
/// eliminating all reallocation and per-chunk `reserve` calls inside the hot loop.
pub fn encoded_size(data: &[u8], line_len: usize) -> usize {
    let line_len = line_len.max(1);
    if data.is_empty() {
        return 0;
    }

    #[cfg(target_arch = "aarch64")]
    {
        return unsafe { encoded_size_neon(data, line_len) };
    }

    #[cfg(not(target_arch = "aarch64"))]
    {
        let last = data.len() - 1;
        let mut escapes = 0usize;
        let mut col = 0usize;

        for (i, &b) in data.iter().enumerate() {
            let e = b.wrapping_add(42);
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
        }

        // One CRLF per complete line, plus one trailing CRLF for a partial final line.
        let lines = data.len().div_ceil(line_len);
        data.len() + escapes + lines * 2
    }
}

#[cfg(target_arch = "aarch64")]
unsafe fn encoded_size_neon(data: &[u8], line_len: usize) -> usize {
    use std::arch::aarch64::*;
    let last = data.len() - 1;
    let mut escapes = 0usize;
    let mut i = 0usize;
    let mut col = 0usize;

    let v_add42 = vdupq_n_u8(42);
    let v_nul = vdupq_n_u8(0x00);
    let v_lf = vdupq_n_u8(0x0A);
    let v_cr = vdupq_n_u8(0x0D);
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
            let mask = vorrq_u8(
                vorrq_u8(vceqq_u8(shifted, v_nul), vceqq_u8(shifted, v_lf)),
                vorrq_u8(vceqq_u8(shifted, v_cr), vceqq_u8(shifted, v_eq)),
            );

            let bits = vcntq_u8(mask);
            let sum = vaddvq_u8(bits) as usize;
            escapes += sum / 8;

            i += 16; col += 16; safe_rem -= 16;
        }

        while safe_rem > 0 {
            let e = data[i].wrapping_add(42);
            if matches!(e, 0x00 | 0x0A | 0x0D | 0x3D) {
                escapes += 1;
            }
            i += 1; col += 1; safe_rem -= 1;
        }
    }

    let lines = data.len().div_ceil(line_len);
    data.len() + escapes + lines * 2
}

/// Core yEnc encoder: append the encoded form of `data` to `out`, wrapping
/// every `line_len` source bytes.
///
/// Each byte is shifted by 42 (mod 256). The four critical output values —
/// NUL, LF, CR and `=` — are always escaped as `=` followed by the value
/// shifted by a further 64. TAB and space are escaped only at the start or end
/// of a line (where transports may strip them), and `.` is escaped at the
/// start of a line (to keep clear of NNTP dot-stuffing).
pub fn encode_scalar(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    let line_len = line_len.max(1);
    // Upper bound: every byte could escape (×2) + one CRLF per line.
    out.reserve(data.len() * 2 + (data.len() / line_len + 1) * 2);
    let last = data.len().saturating_sub(1);
    let mut col = 0usize;

    unsafe {
        let out_base = out.as_mut_ptr();
        let mut out_ptr = out_base.add(out.len());

        for (i, &b) in data.iter().enumerate() {
            let e = b.wrapping_add(42);
            let at_line_start = col == 0;
            let at_line_end = col + 1 == line_len || i == last;

            let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
            let positional = ((e == 0x09 || e == 0x20) && (at_line_start || at_line_end))
                || (e == 0x2E && at_line_start);

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
        }

        if col != 0 {
            *out_ptr = b'\r';
            *out_ptr.add(1) = b'\n';
            out_ptr = out_ptr.add(2);
        }

        out.set_len(out_ptr.offset_from(out_base) as usize);
    }
}

// --- SSSE3 path (x86-64 only) ---

/// SSSE3-accelerated yEnc encoder. Falls back to [`encode_scalar`] when the
/// CPU does not support SSSE3 (detected at runtime via `is_x86_feature_detected!`).
///
/// Produces identical output to [`encode_scalar`] for all inputs.
#[cfg(target_arch = "x86_64")]
#[cfg(target_arch = "x86_64")]
pub fn encode_ssse3(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    if is_x86_feature_detected!("ssse3") {
        unsafe { encode_ssse3_impl(out, data, line_len) }
    } else {
        encode_scalar(out, data, line_len)
    }
}

#[cfg(target_arch = "x86_64")]
pub fn encode_avx2(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    if is_x86_feature_detected!("avx2") {
        unsafe { encode_avx2_impl(out, data, line_len) }
    } else {
        encode_ssse3(out, data, line_len)
    }
}

#[cfg(target_arch = "x86_64")]
pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    if is_x86_feature_detected!("avx2") {
        unsafe { encode_avx2_impl(out, data, line_len) }
    } else if is_x86_feature_detected!("ssse3") {
        unsafe { encode_ssse3_impl(out, data, line_len) }
    } else {
        encode_scalar(out, data, line_len)
    }
}

/// NEON-accelerated yEnc encoder for ARM.
#[cfg(target_arch = "aarch64")]
pub fn encode_neon(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    unsafe { encode_neon_impl(out, data, line_len) }
}

#[cfg(target_arch = "aarch64")]
pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    unsafe { encode_neon_impl(out, data, line_len) }
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    encode_scalar(out, data, line_len)
}

#[cfg(target_arch = "x86_64")]
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
    let v_nul = _mm_setzero_si128();
    let v_lf = _mm_set1_epi8(0x0Au8 as i8);
    let v_cr = _mm_set1_epi8(0x0Du8 as i8);
    let v_eq = _mm_set1_epi8(0x3Du8 as i8);
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
            let shifted_a = _mm_add_epi8(chunk_a, add42);
            let shifted_b = _mm_add_epi8(chunk_b, add42);
            let mask_a = _mm_movemask_epi8(_mm_or_si128(
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted_a, v_nul),
                    _mm_cmpeq_epi8(shifted_a, v_lf),
                ),
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted_a, v_cr),
                    _mm_cmpeq_epi8(shifted_a, v_eq),
                ),
            ));
            let mask_b = _mm_movemask_epi8(_mm_or_si128(
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted_b, v_nul),
                    _mm_cmpeq_epi8(shifted_b, v_lf),
                ),
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted_b, v_cr),
                    _mm_cmpeq_epi8(shifted_b, v_eq),
                ),
            ));
            if mask_a == 0 {
                _mm_storeu_si128(out_ptr as *mut __m128i, shifted_a);
                out_ptr = out_ptr.add(16);
            } else {
                let m_lo = (mask_a & 0xFF) as usize;
                let de_lo = _mm_unpacklo_epi64(shifted_a, v_eq_const);
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_add_epi8(
                        _mm_shuffle_epi8(
                            de_lo,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                        _mm_loadu_si128(ADD_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                let m_hi = ((mask_a >> 8) & 0xFF) as usize;
                let de_hi = _mm_unpackhi_epi64(shifted_a, v_eq_const);
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_add_epi8(
                        _mm_shuffle_epi8(
                            de_hi,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                            ),
                        ),
                        _mm_loadu_si128(ADD_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
            }
            if mask_b == 0 {
                _mm_storeu_si128(out_ptr as *mut __m128i, shifted_b);
                out_ptr = out_ptr.add(16);
            } else {
                let m_lo = (mask_b & 0xFF) as usize;
                let de_lo = _mm_unpacklo_epi64(shifted_b, v_eq_const);
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_add_epi8(
                        _mm_shuffle_epi8(
                            de_lo,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                        _mm_loadu_si128(ADD_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                let m_hi = ((mask_b >> 8) & 0xFF) as usize;
                let de_hi = _mm_unpackhi_epi64(shifted_b, v_eq_const);
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_add_epi8(
                        _mm_shuffle_epi8(
                            de_hi,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                            ),
                        ),
                        _mm_loadu_si128(ADD_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_hi) as usize);
            }
            i += 32;
            col += 32;
            safe_rem -= 32;
        }
        while safe_rem >= 16 {
            let chunk = _mm_loadu_si128(data.as_ptr().add(i) as *const __m128i);
            let shifted = _mm_add_epi8(chunk, add42);
            let mask = _mm_movemask_epi8(_mm_or_si128(
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted, v_nul),
                    _mm_cmpeq_epi8(shifted, v_lf),
                ),
                _mm_or_si128(_mm_cmpeq_epi8(shifted, v_cr), _mm_cmpeq_epi8(shifted, v_eq)),
            ));
            if mask == 0 {
                _mm_storeu_si128(out_ptr as *mut __m128i, shifted);
                out_ptr = out_ptr.add(16);
            } else {
                let m_lo = (mask & 0xFF) as usize;
                let de_lo = _mm_unpacklo_epi64(shifted, v_eq_const);
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_add_epi8(
                        _mm_shuffle_epi8(
                            de_lo,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                        _mm_loadu_si128(ADD_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                let m_hi = ((mask >> 8) & 0xFF) as usize;
                let de_hi = _mm_unpackhi_epi64(shifted, v_eq_const);
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_add_epi8(
                        _mm_shuffle_epi8(
                            de_hi,
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                            ),
                        ),
                        _mm_loadu_si128(ADD_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i),
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

#[cfg(target_arch = "x86_64")]
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
    let v_nul = _mm256_setzero_si256();
    let v_lf = _mm256_set1_epi8(0x0Au8 as i8);
    let v_cr = _mm256_set1_epi8(0x0Du8 as i8);
    let v_eq = _mm256_set1_epi8(0x3Du8 as i8);
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
            let chunk = _mm256_loadu_si256(data.as_ptr().add(i) as *const __m256i);
            let shifted = _mm256_add_epi8(chunk, add42);
            let any = _mm256_or_si256(
                _mm256_or_si256(
                    _mm256_cmpeq_epi8(shifted, v_nul),
                    _mm256_cmpeq_epi8(shifted, v_lf),
                ),
                _mm256_or_si256(
                    _mm256_cmpeq_epi8(shifted, v_cr),
                    _mm256_cmpeq_epi8(shifted, v_eq),
                ),
            );
            let mask = _mm256_movemask_epi8(any) as u32;
            if mask == 0 {
                _mm256_storeu_si256(out_ptr as *mut __m256i, shifted);
                out_ptr = out_ptr.add(32);
            } else {
                let s_lo = _mm256_extracti128_si256(shifted, 0);
                let s_hi = _mm256_extracti128_si256(shifted, 1);
                let m16_lo = mask & 0xFFFF;
                if m16_lo == 0 {
                    _mm_storeu_si128(out_ptr as *mut __m128i, s_lo);
                    out_ptr = out_ptr.add(16);
                } else {
                    let m_lo = (m16_lo & 0xFF) as usize;
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_add_epi8(
                            _mm_shuffle_epi8(
                                _mm_unpacklo_epi64(s_lo, v_eq_const),
                                _mm_loadu_si128(
                                    SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                                ),
                            ),
                            _mm_loadu_si128(
                                ADD_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                    let m_hi = ((m16_lo >> 8) & 0xFF) as usize;
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_add_epi8(
                            _mm_shuffle_epi8(
                                _mm_unpackhi_epi64(s_lo, v_eq_const),
                                _mm_loadu_si128(
                                    SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                                ),
                            ),
                            _mm_loadu_si128(
                                ADD_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
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
                        _mm_add_epi8(
                            _mm_shuffle_epi8(
                                _mm_unpacklo_epi64(s_hi, v_eq_const),
                                _mm_loadu_si128(
                                    SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                                ),
                            ),
                            _mm_loadu_si128(
                                ADD_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                    );
                    out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                    let m_hi = ((m16_hi >> 8) & 0xFF) as usize;
                    _mm_storeu_si128(
                        out_ptr as *mut __m128i,
                        _mm_add_epi8(
                            _mm_shuffle_epi8(
                                _mm_unpackhi_epi64(s_hi, v_eq_const),
                                _mm_loadu_si128(
                                    SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                                ),
                            ),
                            _mm_loadu_si128(
                                ADD_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
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
        let v_nul_128 = _mm_setzero_si128();
        let v_lf_128 = _mm_set1_epi8(0x0Au8 as i8);
        let v_cr_128 = _mm_set1_epi8(0x0Du8 as i8);
        let v_eq_128 = _mm_set1_epi8(0x3Du8 as i8);
        while safe_rem >= 16 {
            let chunk = _mm_loadu_si128(data.as_ptr().add(i) as *const __m128i);
            let shifted = _mm_add_epi8(chunk, add42_128);
            let mask = _mm_movemask_epi8(_mm_or_si128(
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted, v_nul_128),
                    _mm_cmpeq_epi8(shifted, v_lf_128),
                ),
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted, v_cr_128),
                    _mm_cmpeq_epi8(shifted, v_eq_128),
                ),
            ));
            if mask == 0 {
                _mm_storeu_si128(out_ptr as *mut __m128i, shifted);
                out_ptr = out_ptr.add(16);
            } else {
                let m_lo = (mask & 0xFF) as usize;
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_add_epi8(
                        _mm_shuffle_epi8(
                            _mm_unpacklo_epi64(shifted, v_eq_const),
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i
                            ),
                        ),
                        _mm_loadu_si128(ADD_TABLE.get_unchecked(m_lo).as_ptr() as *const __m128i),
                    ),
                );
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(m_lo) as usize);
                let m_hi = ((mask >> 8) & 0xFF) as usize;
                _mm_storeu_si128(
                    out_ptr as *mut __m128i,
                    _mm_add_epi8(
                        _mm_shuffle_epi8(
                            _mm_unpackhi_epi64(shifted, v_eq_const),
                            _mm_loadu_si128(
                                SHUFFLE_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i
                            ),
                        ),
                        _mm_loadu_si128(ADD_TABLE.get_unchecked(m_hi).as_ptr() as *const __m128i),
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

#[cfg(target_arch = "aarch64")]
unsafe fn encode_neon_impl(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    use std::arch::aarch64::*;
    let line_len = line_len.max(1);
    if data.is_empty() { return; }
    // Reservation is now handled by encode_part, but we keep a local
    // reserve for direct calls.
    out.reserve(data.len() * 2 + (data.len() / line_len + 1) * 2);
    let last = data.len() - 1;
    let v_add42 = vdupq_n_u8(42);
    let v_nul = vdupq_n_u8(0x00);
    let v_lf = vdupq_n_u8(0x0A);
    let v_cr = vdupq_n_u8(0x0D);
    let v_eq = vdupq_n_u8(0x3D);
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
                *out_ptr = b'='; *out_ptr.add(1) = e.wrapping_add(64); out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e; out_ptr = out_ptr.add(1);
            }
            col += 1;
            if col == line_len {
                *out_ptr = b'\r'; *out_ptr.add(1) = b'\n'; out_ptr = out_ptr.add(2);
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
            let m0 = vorrq_u8(vorrq_u8(vceqq_u8(s0, v_nul), vceqq_u8(s0, v_lf)), vorrq_u8(vceqq_u8(s0, v_cr), vceqq_u8(s0, v_eq)));
            let m1 = vorrq_u8(vorrq_u8(vceqq_u8(s1, v_nul), vceqq_u8(s1, v_lf)), vorrq_u8(vceqq_u8(s1, v_cr), vceqq_u8(s1, v_eq)));
            let m2 = vorrq_u8(vorrq_u8(vceqq_u8(s2, v_nul), vceqq_u8(s2, v_lf)), vorrq_u8(vceqq_u8(s2, v_cr), vceqq_u8(s2, v_eq)));
            let m3 = vorrq_u8(vorrq_u8(vceqq_u8(s3, v_nul), vceqq_u8(s3, v_lf)), vorrq_u8(vceqq_u8(s3, v_cr), vceqq_u8(s3, v_eq)));

            if vmaxvq_u8(vorrq_u8(vorrq_u8(m0, m1), vorrq_u8(m2, m3))) == 0 {
                vst1q_u8(out_ptr, s0);
                vst1q_u8(out_ptr.add(16), s1);
                vst1q_u8(out_ptr.add(32), s2);
                vst1q_u8(out_ptr.add(48), s3);
                out_ptr = out_ptr.add(64);
                i += 64; col += 64; safe_rem -= 64;
            } else {
                for (chunk_s, chunk_m) in [(s0, m0), (s1, m1), (s2, m2), (s3, m3)] {
                    if vmaxvq_u8(chunk_m) == 0 {
                        vst1q_u8(out_ptr, chunk_s);
                        out_ptr = out_ptr.add(16);
                    } else {
                        let weighted = vandq_u8(chunk_m, v_weights);
                        let sum_lo = vaddv_u8(vget_low_u8(weighted)) as usize;
                        let sum_hi = vaddv_u8(vget_high_u8(weighted)) as usize;
                        let de_lo = vcombine_u8(vget_low_u8(chunk_s), vget_low_u8(v_eq_const));
                        let de_hi = vcombine_u8(vget_high_u8(chunk_s), vget_low_u8(v_eq_const));
                        let res_lo = vaddq_u8(vqtbl1q_u8(de_lo, vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_lo).as_ptr())), vld1q_u8(ADD_TABLE.get_unchecked(sum_lo).as_ptr()));
                        vst1q_u8(out_ptr, res_lo);
                        out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_lo) as usize);
                        let res_hi = vaddq_u8(vqtbl1q_u8(de_hi, vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_hi).as_ptr())), vld1q_u8(ADD_TABLE.get_unchecked(sum_hi).as_ptr()));
                        vst1q_u8(out_ptr, res_hi);
                        out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_hi) as usize);
                    }
                    i += 16; col += 16;
                }
                safe_rem -= 64;
            }
        }

        while safe_rem >= 32 {
            let chunk0 = vld1q_u8(data.as_ptr().add(i));
            let chunk1 = vld1q_u8(data.as_ptr().add(i + 16));
            let s0 = vaddq_u8(chunk0, v_add42);
            let s1 = vaddq_u8(chunk1, v_add42);
            let m0 = vorrq_u8(
                vorrq_u8(vceqq_u8(s0, v_nul), vceqq_u8(s0, v_lf)),
                vorrq_u8(vceqq_u8(s0, v_cr), vceqq_u8(s0, v_eq)),
            );
            let m1 = vorrq_u8(
                vorrq_u8(vceqq_u8(s1, v_nul), vceqq_u8(s1, v_lf)),
                vorrq_u8(vceqq_u8(s1, v_cr), vceqq_u8(s1, v_eq)),
            );

            if vmaxvq_u8(vorrq_u8(m0, m1)) == 0 {
                vst1q_u8(out_ptr, s0);
                vst1q_u8(out_ptr.add(16), s1);
                out_ptr = out_ptr.add(32);
                i += 32; col += 32; safe_rem -= 32;
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
                        let de_lo = vcombine_u8(vget_low_u8(chunk_s), vget_low_u8(v_eq_const));
                        let de_hi = vcombine_u8(vget_high_u8(chunk_s), vget_low_u8(v_eq_const));
                        let res_lo = vaddq_u8(vqtbl1q_u8(de_lo, vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_lo).as_ptr())), vld1q_u8(ADD_TABLE.get_unchecked(sum_lo).as_ptr()));
                        vst1q_u8(out_ptr, res_lo);
                        out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_lo) as usize);
                        let res_hi = vaddq_u8(vqtbl1q_u8(de_hi, vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_hi).as_ptr())), vld1q_u8(ADD_TABLE.get_unchecked(sum_hi).as_ptr()));
                        vst1q_u8(out_ptr, res_hi);
                        out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_hi) as usize);
                    }
                    i += 16; col += 16;
                }
                safe_rem -= 32;
            }
        }

        while safe_rem >= 16 {
            let chunk = vld1q_u8(data.as_ptr().add(i));
            let shifted = vaddq_u8(chunk, v_add42);
            let mask = vorrq_u8(
                vorrq_u8(vceqq_u8(shifted, v_nul), vceqq_u8(shifted, v_lf)),
                vorrq_u8(vceqq_u8(shifted, v_cr), vceqq_u8(shifted, v_eq)),
            );

            if vmaxvq_u8(mask) == 0 {
                vst1q_u8(out_ptr, shifted);
                out_ptr = out_ptr.add(16);
            } else {
                let weighted = vandq_u8(mask, v_weights);
                let sum_low = vaddv_u8(vget_low_u8(weighted)) as usize;
                let sum_high = vaddv_u8(vget_high_u8(weighted)) as usize;
                let de_lo = vcombine_u8(vget_low_u8(shifted), vget_low_u8(v_eq_const));
                let de_hi = vcombine_u8(vget_high_u8(shifted), vget_low_u8(v_eq_const));
                let res_lo = vaddq_u8(vqtbl1q_u8(de_lo, vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_low).as_ptr())), vld1q_u8(ADD_TABLE.get_unchecked(sum_low).as_ptr()));
                vst1q_u8(out_ptr, res_lo);
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_low) as usize);
                let res_hi = vaddq_u8(vqtbl1q_u8(de_hi, vld1q_u8(SHUFFLE_TABLE.get_unchecked(sum_high).as_ptr())), vld1q_u8(ADD_TABLE.get_unchecked(sum_high).as_ptr()));
                vst1q_u8(out_ptr, res_hi);
                out_ptr = out_ptr.add(*LEN_TABLE.get_unchecked(sum_high) as usize);
            }
            i += 16; col += 16; safe_rem -= 16;
        }

        while safe_rem > 0 {
            let e = data[i].wrapping_add(42);
            if matches!(e, 0x00 | 0x0A | 0x0D | 0x3D) {
                *out_ptr = b'='; *out_ptr.add(1) = e.wrapping_add(64); out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e; out_ptr = out_ptr.add(1);
            }
            i += 1; col += 1; safe_rem -= 1;
        }

        if i < data.len() {
            let at_line_end = col + 1 == line_len || i == last;
            let b = data[i];
            let e = b.wrapping_add(42);
            let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
            let positional = (e == 0x09 || e == 0x20) && at_line_end;
            if critical || positional {
                *out_ptr = b'='; *out_ptr.add(1) = e.wrapping_add(64); out_ptr = out_ptr.add(2);
            } else {
                *out_ptr = e; out_ptr = out_ptr.add(1);
            }
            col += 1;
            if col == line_len {
                *out_ptr = b'\r'; *out_ptr.add(1) = b'\n'; out_ptr = out_ptr.add(2);
                col = 0;
            }
            i += 1;
        }
    }

    if col != 0 {
        *out_ptr = b'\r'; *out_ptr.add(1) = b'\n'; out_ptr = out_ptr.add(2);
    }
    out.set_len(out_ptr.offset_from(out_base) as usize);
}
pub static SHUFFLE_TABLE: [[u8; 16]; 256] = [
    [
        0, 1, 2, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255, 255, 255,
    ],
    [8, 0, 1, 2, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 1, 2, 8, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 4, 5, 6, 7, 255, 255, 255, 255],
    [0, 1, 2, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255],
    [0, 1, 2, 8, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 8, 4, 5, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 8, 4, 5, 6, 7, 255, 255, 255],
    [0, 1, 2, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [0, 1, 2, 8, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 4, 8, 5, 6, 7, 255, 255, 255],
    [0, 1, 2, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255],
    [0, 1, 2, 8, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 8, 4, 8, 5, 6, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 8, 4, 8, 5, 6, 7, 255, 255],
    [0, 1, 2, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 1, 2, 8, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 4, 5, 8, 6, 7, 255, 255, 255],
    [0, 1, 2, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255],
    [0, 1, 2, 8, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 8, 4, 5, 8, 6, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 8, 4, 5, 8, 6, 7, 255, 255],
    [0, 1, 2, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [0, 1, 2, 8, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 4, 8, 5, 8, 6, 7, 255, 255],
    [0, 1, 2, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [0, 1, 8, 2, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255],
    [0, 1, 2, 8, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255],
    [0, 1, 8, 2, 8, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 8, 4, 8, 5, 8, 6, 7, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 8, 4, 8, 5, 8, 6, 7, 255],
    [0, 1, 2, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 1, 2, 8, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 4, 5, 6, 8, 7, 255, 255, 255],
    [0, 1, 2, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255],
    [0, 1, 2, 8, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 8, 4, 5, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 8, 4, 5, 6, 8, 7, 255, 255],
    [0, 1, 2, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [0, 1, 2, 8, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 4, 8, 5, 6, 8, 7, 255, 255],
    [0, 1, 2, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [0, 1, 8, 2, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255],
    [0, 1, 2, 8, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255],
    [0, 1, 8, 2, 8, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 8, 4, 8, 5, 6, 8, 7, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 8, 4, 8, 5, 6, 8, 7, 255],
    [0, 1, 2, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [0, 1, 8, 2, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 1, 2, 8, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 1, 8, 2, 8, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 4, 5, 8, 6, 8, 7, 255, 255],
    [0, 1, 2, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 1, 8, 2, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255],
    [0, 1, 2, 8, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255],
    [0, 1, 8, 2, 8, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 8, 4, 5, 8, 6, 8, 7, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 8, 4, 5, 8, 6, 8, 7, 255],
    [0, 1, 2, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [0, 8, 1, 2, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 1, 8, 2, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 8, 2, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [0, 1, 2, 8, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 2, 8, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [0, 1, 8, 2, 8, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [0, 8, 1, 8, 2, 8, 3, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 4, 8, 5, 8, 6, 8, 7, 255],
    [0, 1, 2, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255, 255],
    [8, 0, 1, 2, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [0, 8, 1, 2, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 8, 1, 2, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [0, 1, 8, 2, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 1, 8, 2, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [0, 8, 1, 8, 2, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [8, 0, 8, 1, 8, 2, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255],
    [0, 1, 2, 8, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255, 255],
    [8, 0, 1, 2, 8, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [0, 8, 1, 2, 8, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [8, 0, 8, 1, 2, 8, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255],
    [0, 1, 8, 2, 8, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255, 255],
    [8, 0, 1, 8, 2, 8, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255],
    [0, 8, 1, 8, 2, 8, 3, 8, 4, 8, 5, 8, 6, 8, 7, 255],
    [8, 0, 8, 1, 8, 2, 8, 3, 8, 4, 8, 5, 8, 6, 8, 7],
];
pub static ADD_TABLE: [[u8; 16]; 256] = [
    [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 0, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 0, 0],
    [0, 0, 0, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0],
    [0, 0, 0, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 0],
    [0, 0, 0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 0],
    [0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0],
    [0, 0, 0, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 0],
    [0, 0, 0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 0],
    [0, 0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0],
    [0, 0, 0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0],
    [0, 0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0, 0],
    [0, 64, 0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 64, 0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0],
    [0, 0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0, 0],
    [0, 64, 0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 64, 0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0],
    [0, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 0],
    [0, 64, 0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0],
    [0, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0],
    [0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64, 0, 64],
];
pub static LEN_TABLE: [u8; 256] = [
    8, 9, 9, 10, 9, 10, 10, 11, 9, 10, 10, 11, 10, 11, 11, 12, 9, 10, 10, 11, 10, 11, 11, 12, 10,
    11, 11, 12, 11, 12, 12, 13, 9, 10, 10, 11, 10, 11, 11, 12, 10, 11, 11, 12, 11, 12, 12, 13, 10,
    11, 11, 12, 11, 12, 12, 13, 11, 12, 12, 13, 12, 13, 13, 14, 9, 10, 10, 11, 10, 11, 11, 12, 10,
    11, 11, 12, 11, 12, 12, 13, 10, 11, 11, 12, 11, 12, 12, 13, 11, 12, 12, 13, 12, 13, 13, 14, 10,
    11, 11, 12, 11, 12, 12, 13, 11, 12, 12, 13, 12, 13, 13, 14, 11, 12, 12, 13, 12, 13, 13, 14, 12,
    13, 13, 14, 13, 14, 14, 15, 9, 10, 10, 11, 10, 11, 11, 12, 10, 11, 11, 12, 11, 12, 12, 13, 10,
    11, 11, 12, 11, 12, 12, 13, 11, 12, 12, 13, 12, 13, 13, 14, 10, 11, 11, 12, 11, 12, 12, 13, 11,
    12, 12, 13, 12, 13, 13, 14, 11, 12, 12, 13, 12, 13, 13, 14, 12, 13, 13, 14, 13, 14, 14, 15, 10,
    11, 11, 12, 11, 12, 12, 13, 11, 12, 12, 13, 12, 13, 13, 14, 11, 12, 12, 13, 12, 13, 13, 14, 12,
    13, 13, 14, 13, 14, 14, 15, 11, 12, 12, 13, 12, 13, 13, 14, 12, 13, 13, 14, 13, 14, 14, 15, 12,
    13, 13, 14, 13, 14, 14, 15, 13, 14, 14, 15, 14, 15, 15, 16,
];

#[allow(dead_code)]
mod tests {
    use super::*;

    /// Minimal yEnc decoder used to verify that encoding is reversible.
    fn decode(encoded: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut escape = false;
        for &c in encoded {
            if escape {
                out.push(c.wrapping_sub(64).wrapping_sub(42));
                escape = false;
            } else if c == b'=' {
                escape = true;
            } else if c != b'\r' && c != b'\n' {
                out.push(c.wrapping_sub(42));
            }
        }
        out
    }

    fn encode_s(data: &[u8], line_len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        encode_scalar(&mut out, data, line_len);
        out
    }

    #[test]
    fn crc32_standard_check_value() {
        // The well-known CRC-32 check value for the ASCII string "123456789".
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
        assert_eq!(crc32(b""), 0);
    }

    #[test]
    fn crc32_incremental_matches_oneshot() {
        let data: Vec<u8> = (0..=255u8).collect();
        let mut crc = Crc32::new();
        crc.update(&data[..100]);
        crc.update(&data[100..]);
        assert_eq!(crc.finalize(), crc32(&data));
    }

    #[test]
    fn escapes_critical_characters() {
        // 0xD6 + 42 == 0x00 (NUL) -> escaped as '=' + (0x00 + 64).
        assert_eq!(encode_s(&[0xD6], 128), vec![b'=', 0x40, b'\r', b'\n']);
        // 0xE3 + 42 == 0x0D (CR) -> escaped.
        assert_eq!(encode_s(&[0xE3], 128), vec![b'=', 0x4D, b'\r', b'\n']);
        // 0x13 + 42 == 0x3D ('=') -> escaped.
        assert_eq!(encode_s(&[0x13], 128), vec![b'=', 0x7D, b'\r', b'\n']);
    }

    #[test]
    fn escapes_dot_at_line_start() {
        // 0x04 + 42 == 0x2E ('.'); at the start of a line it must be escaped.
        assert_eq!(encode_s(&[0x04], 128), vec![b'=', 0x6E, b'\r', b'\n']);
        // Mid-line, the same byte is left untouched.
        let line = encode_s(&[0x00, 0x04], 128);
        assert_eq!(line, vec![0x2A, 0x2E, b'\r', b'\n']);
    }

    #[test]
    fn non_critical_byte_is_shifted() {
        // 0x00 + 42 == 0x2A ('*'), not a critical character.
        assert_eq!(encode_s(&[0x00], 128), vec![0x2A, b'\r', b'\n']);
    }

    #[test]
    fn wraps_lines_at_line_length() {
        let encoded = encode_s(&[0u8; 5], 2);
        // Three lines: 2 + 2 + 1 bytes, each terminated by CRLF.
        assert_eq!(encoded, b"\x2a\x2a\r\n\x2a\x2a\r\n\x2a\r\n");
    }

    #[test]
    fn encoding_is_reversible_for_all_byte_values() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        assert_eq!(decode(&encode_s(&data, 128)), data);
    }

    #[test]
    fn segments_split_evenly_and_with_remainder() {
        assert_eq!(segments(250, 100), vec![(0, 100), (100, 100), (200, 50)]);
        assert_eq!(segments(100, 100), vec![(0, 100)]);
        assert_eq!(segments(0, 100), vec![(0, 0)]);
    }

    #[test]
    fn single_part_body_has_expected_control_lines() {
        let part = encode_part(
            "test.bin",
            3,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            &[1, 2, 3],
            128,
            None,
        );
        let body = String::from_utf8(part.body).unwrap();
        assert!(body.starts_with("=ybegin line=128 size=3 name=test.bin\r\n"));
        assert!(body.contains("=yend size=3 crc32="));
        assert_eq!(part.begin, 1);
        assert_eq!(part.end, 3);
    }

    #[test]
    fn multi_part_body_has_ypart_line() {
        let part = encode_part(
            "movie.mkv",
            10_000,
            PartSpec {
                number: 2,
                total: 3,
                offset: 100,
            },
            &[0u8; 50],
            128,
            Some(0xDEAD_BEEF),
        );
        let body = String::from_utf8(part.body).unwrap();
        assert!(body.starts_with("=ybegin part=2 total=3 line=128 size=10000 name=movie.mkv\r\n"));
        assert!(body.contains("=ypart begin=101 end=150\r\n"));
        assert!(body.contains("=yend size=50 part=2 pcrc32="));
        assert!(body.contains("crc32=deadbeef"));
        assert_eq!(part.begin, 101);
        assert_eq!(part.end, 150);
    }

    #[test]
    fn encode_part_empty_data_zero_length_segment() {
        // An empty file produces one segment with no data bytes.
        let part = encode_part(
            "empty.bin",
            0,
            PartSpec {
                number: 1,
                total: 1,
                offset: 0,
            },
            &[],
            128,
            None,
        );
        let body = String::from_utf8(part.body).unwrap();
        // Control lines still present and consistent.
        assert!(body.contains("size=0"));
        assert!(body.contains("=yend size=0 crc32="));
        // begin=1, end=0 for an empty part (offset + len = 0 + 0).
        assert_eq!(part.begin, 1);
        assert_eq!(part.end, 0);
        assert_eq!(part.crc32, crc32(&[]));
    }

    #[test]
    fn segments_file_exactly_article_size_yields_one_segment() {
        assert_eq!(segments(128, 128), vec![(0, 128)]);
    }

    #[test]
    fn segments_file_one_byte_over_article_size_yields_two_segments() {
        assert_eq!(segments(129, 128), vec![(0, 128), (128, 1)]);
    }

    #[test]
    fn body_length_matches_line_length_exactly() {
        // A single segment whose data is exactly line_len bytes should produce
        // a single encoded line (no premature wrap, no missing trailing CRLF).
        let data: Vec<u8> = vec![0x00; 128]; // 128 bytes, none critical
        let encoded = encode_s(&data, 128);
        // Exactly 128 encoded bytes + CRLF; no mid-line CRLF.
        let newline_count = encoded.windows(2).filter(|w| w == b"\r\n").count();
        assert_eq!(
            newline_count, 1,
            "expected exactly one CRLF for a full line"
        );
        // Round-trip.
        assert_eq!(decode(&encoded), data);
    }

    // --- 26a: comprehensive coverage of all four critical bytes ---

    // Input bytes whose encoded value (b+42 mod 256) hits each critical value.
    const NUL_IN: u8 = 0xD6; // (0xD6+42)%256=0x00 NUL
    const LF_IN: u8 = 0xE0; // (0xE0+42)%256=0x0A LF
    const CR_IN: u8 = 0xE3; // (0xE3+42)%256=0x0D CR
    const EQ_IN: u8 = 0x13; // (0x13+42)%256=0x3D '='
                            // Input bytes whose encoded value hits positional specials.
    const TAB_IN: u8 = 0xDF; // (0xDF+42)%256=0x09 TAB
    const SP_IN: u8 = 0xF6; // (0xF6+42)%256=0x20 SPACE
    const DOT_IN: u8 = 0x04; // (0x04+42)%256=0x2E '.'

    #[test]
    fn all_critical_bytes_escaped_at_first_position() {
        for raw in [NUL_IN, LF_IN, CR_IN, EQ_IN] {
            let enc = encode_s(&[raw], 128);
            assert_eq!(enc[0], b'=', "byte {raw:#04x} not escaped at position 0");
        }
    }

    #[test]
    fn all_critical_bytes_escaped_in_middle() {
        // Surround with a neutral byte (0x00 encodes to 0x2A, never critical).
        for raw in [NUL_IN, LF_IN, CR_IN, EQ_IN] {
            let data = [0x00, raw, 0x00];
            let enc = encode_s(&data, 128);
            // First byte is neutral (0x2A), second must be escape '='.
            assert_eq!(enc[0], 0x2A);
            assert_eq!(enc[1], b'=', "byte {raw:#04x} not escaped in middle");
        }
    }

    #[test]
    fn all_critical_bytes_escaped_at_last_position() {
        for raw in [NUL_IN, LF_IN, CR_IN, EQ_IN] {
            let data = [0x00, raw];
            let enc = encode_s(&data, 128);
            assert_eq!(enc[0], 0x2A);
            assert_eq!(enc[1], b'=', "byte {raw:#04x} not escaped at last position");
        }
    }

    #[test]
    fn consecutive_critical_bytes_all_escaped() {
        let data = [NUL_IN, LF_IN, CR_IN, EQ_IN];
        let enc = encode_s(&data, 128);
        // 4 escaped bytes = 8 encoded bytes + CRLF.
        assert_eq!(enc.len(), 10);
        for i in (0..8).step_by(2) {
            assert_eq!(enc[i], b'=', "escape marker missing at index {i}");
        }
        assert_eq!(&enc[8..], b"\r\n");
    }

    #[test]
    fn critical_escape_payload_is_value_plus_64() {
        // NUL (0x00): escape payload = 0x00 + 64 = 0x40.
        assert_eq!(encode_s(&[NUL_IN], 128)[1], 0x40);
        // LF  (0x0A): payload = 0x4A.
        assert_eq!(encode_s(&[LF_IN], 128)[1], 0x4A);
        // CR  (0x0D): payload = 0x4D.
        assert_eq!(encode_s(&[CR_IN], 128)[1], 0x4D);
        // '=' (0x3D): payload = 0x7D.
        assert_eq!(encode_s(&[EQ_IN], 128)[1], 0x7D);
    }

    // --- 26a: positional escapes ---

    #[test]
    fn tab_escaped_at_line_start() {
        let enc = encode_s(&[TAB_IN], 128);
        assert_eq!(enc[0], b'=', "TAB not escaped at line start");
        assert_eq!(enc[1], 0x09u8.wrapping_add(64));
    }

    #[test]
    fn tab_not_escaped_mid_line() {
        // 0x00 is a safe neutral byte (encodes to 0x2A).
        let data = [0x00, TAB_IN, 0x00];
        let enc = encode_s(&data, 128);
        assert_eq!(enc[1], 0x09, "TAB incorrectly escaped mid-line");
    }

    #[test]
    fn tab_escapedat_line_end() {
        // line_len=4: positions 0,1,2,3 → position 3 is the last in the line.
        let data = [0x00, 0x00, 0x00, TAB_IN];
        let enc = encode_s(&data, 4);
        // First three neutral bytes produce 0x2A each.
        assert_eq!(&enc[..3], &[0x2A, 0x2A, 0x2A]);
        assert_eq!(enc[3], b'=', "TAB not escaped at line end (col=line_len-1)");
    }

    #[test]
    fn space_escaped_at_line_start() {
        let enc = encode_s(&[SP_IN], 128);
        assert_eq!(enc[0], b'=', "SPACE not escaped at line start");
    }

    #[test]
    fn space_not_escaped_mid_line() {
        let data = [0x00, SP_IN, 0x00];
        let enc = encode_s(&data, 128);
        assert_eq!(enc[1], 0x20, "SPACE incorrectly escaped mid-line");
    }

    #[test]
    fn space_escapedat_line_end() {
        let data = [0x00, 0x00, 0x00, SP_IN];
        let enc = encode_s(&data, 4);
        assert_eq!(&enc[..3], &[0x2A, 0x2A, 0x2A]);
        assert_eq!(enc[3], b'=', "SPACE not escaped at line end");
    }

    #[test]
    fn dot_escaped_at_start_of_second_line() {
        // line_len=2: first line = [0x00, 0x00], second line starts with DOT_IN.
        let data = [0x00, 0x00, DOT_IN];
        let enc = encode_s(&data, 2);
        // First line: 0x2A 0x2A \r\n
        assert_eq!(&enc[..4], b"\x2a\x2a\r\n");
        // Second line must begin with '=' (dot at line start, NNTP dot-stuffing).
        assert_eq!(enc[4], b'=', "dot not escaped at start of second line");
    }

    #[test]
    fn dot_not_escaped_mid_line_or_line_end() {
        // DOT_IN mid-line and at end of line — neither position requires escape.
        let data = [0x00, DOT_IN, DOT_IN];
        let enc = encode_s(&data, 128);
        // Position 1 and 2 are mid-line and last — no escape needed.
        assert_eq!(enc[1], 0x2E, "dot incorrectly escaped mid-line");
        assert_eq!(enc[2], 0x2E, "dot incorrectly escaped at last position");
    }

    // --- 26a: wrap-around at exactly line_len ---

    #[test]
    fn line_wrap_inserts_crlf_at_boundary() {
        // line_len=3: every 3 input bytes produce one CRLF.
        let data = [0x00u8; 6];
        let enc = encode_s(&data, 3);
        assert_eq!(&enc[3..5], b"\r\n", "CRLF missing after first full line");
        assert_eq!(&enc[8..10], b"\r\n", "CRLF missing after second full line");
        assert_eq!(enc.len(), 10); // 3+CRLF + 3+CRLF
    }

    #[test]
    fn trailing_crlf_added_for_partial_line() {
        let data = [0x00u8; 5];
        let enc = encode_s(&data, 3);
        // Lines: 3 bytes + CRLF, then 2 bytes + CRLF.
        assert_eq!(enc.len(), 9);
        assert_eq!(&enc[7..], b"\r\n");
    }

    // --- 26a: full 256-byte round-trip ---

    #[test]
    fn full_256_byte_round_trip() {
        let data: Vec<u8> = (0u8..=255).collect();
        assert_eq!(decode(&encode_s(&data, 128)), data);
    }

    // --- 26b: SSSE3 path produces identical output to scalar ---

    #[cfg(target_arch = "x86_64")]
    fn encode_ssse3_vec(data: &[u8], line_len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        encode_ssse3(&mut out, data, line_len);
        out
    }

    /// Macro: assert SSSE3 output equals scalar output for `data` and `line_len`.
    #[allow(unused_macros)]
    macro_rules! assert_ssse3_eq {
        ($data:expr, $line_len:expr) => {{
            #[cfg(target_arch = "x86_64")]
            {
                let scalar = encode_s($data, $line_len);
                let simd = encode_ssse3_vec($data, $line_len);
                assert_eq!(
                    simd, scalar,
                    "SSSE3 diverges from scalar (line_len={})",
                    $line_len
                );
            }
        }};
    }

    #[test]
    fn ssse3_matches_scalar_all_256_byte_values() {
        #[cfg(target_arch = "x86_64")]
        {
            let data: Vec<u8> = (0u8..=255).collect();
            assert_ssse3_eq!(&data, 128);
        }
    }

    #[test]
    fn ssse3_matches_scalar_all_critical_bytes() {
        #[cfg(target_arch = "x86_64")]
        {
            // All four critical raw inputs, repeated to span many lines.
            let data: Vec<u8> = [NUL_IN, LF_IN, CR_IN, EQ_IN]
                .iter()
                .cycle()
                .copied()
                .take(512)
                .collect();
            assert_ssse3_eq!(&data, 128);
        }
    }

    #[test]
    fn ssse3_matches_scalar_positional_bytes_at_boundaries() {
        #[cfg(target_arch = "x86_64")]
        {
            // Dot/space/tab at line start and end with line_len=4.
            let data: Vec<u8> = [DOT_IN, SP_IN, TAB_IN, 0x00]
                .iter()
                .cycle()
                .copied()
                .take(256)
                .collect();
            assert_ssse3_eq!(&data, 4);
        }
    }

    #[test]
    fn ssse3_matches_scalar_large_random_like_payload() {
        #[cfg(target_arch = "x86_64")]
        {
            // 750 KB of pseudo-random data (covers the typical article size).
            let data: Vec<u8> = (0u8..=255)
                .cycle()
                .enumerate()
                .map(|(i, b): (usize, u8)| b.wrapping_add((i.wrapping_mul(7).wrapping_add(13)) as u8))
                .take(750 * 1024)
                .collect();
            assert_ssse3_eq!(&data, 128);
        }
    }

    #[test]
    fn ssse3_matches_scalar_empty() {
        #[cfg(target_arch = "x86_64")]
        assert_ssse3_eq!(&[], 128);
    }

    #[test]
    fn ssse3_matches_scalar_single_byte() {
        #[cfg(target_arch = "x86_64")]
        for b in 0u8..=255 {
            let data = [b];
            assert_ssse3_eq!(&data, 128);
        }
    }

    #[test]
    fn ssse3_matches_scalar_short_line_len() {
        #[cfg(target_arch = "x86_64")]
        {
            // Stress the boundary logic with very short lines.
            let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();
            for ll in [1, 2, 3, 4, 7, 16, 17] {
                assert_ssse3_eq!(&data, ll);
            }
        }
    }

    #[test]
    fn ssse3_round_trip() {
        #[cfg(target_arch = "x86_64")]
        {
            let data: Vec<u8> = (0u8..=255).cycle().take(750 * 1024).collect();
            let mut encoded = Vec::new();
            encode_ssse3(&mut encoded, &data, 128);
            assert_eq!(decode(&encoded), data);
        }
    }

    // --- 26c: AVX2 path produces identical output to scalar ---

    #[cfg(target_arch = "x86_64")]
    fn encode_avx2_vec(data: &[u8], line_len: usize) -> Vec<u8> {
        let mut out = Vec::new();
        encode_avx2(&mut out, data, line_len);
        out
    }

    #[allow(unused_macros)]
    macro_rules! assert_avx2_eq {
        ($data:expr, $line_len:expr) => {{
            #[cfg(target_arch = "x86_64")]
            {
                let scalar = encode_s($data, $line_len);
                let simd = encode_avx2_vec($data, $line_len);
                assert_eq!(
                    simd, scalar,
                    "AVX2 diverges from scalar (line_len={})",
                    $line_len
                );
            }
        }};
    }

    #[test]
    fn avx2_matches_scalar_all_256_byte_values() {
        #[cfg(target_arch = "x86_64")]
        {
            let data: Vec<u8> = (0u8..=255).collect();
            assert_avx2_eq!(&data, 128);
        }
    }

    #[test]
    fn avx2_matches_scalar_all_critical_bytes() {
        #[cfg(target_arch = "x86_64")]
        {
            let data: Vec<u8> = [NUL_IN, LF_IN, CR_IN, EQ_IN]
                .iter()
                .cycle()
                .copied()
                .take(512)
                .collect();
            assert_avx2_eq!(&data, 128);
        }
    }

    #[test]
    fn avx2_matches_scalar_positional_bytes_at_boundaries() {
        #[cfg(target_arch = "x86_64")]
        {
            let data: Vec<u8> = [DOT_IN, SP_IN, TAB_IN, 0x00]
                .iter()
                .cycle()
                .copied()
                .take(256)
                .collect();
            assert_avx2_eq!(&data, 4);
        }
    }

    #[test]
    fn avx2_matches_scalar_large_random_like_payload() {
        #[cfg(target_arch = "x86_64")]
        {
            let data: Vec<u8> = (0u8..=255)
                .cycle()
                .enumerate()
                .map(|(i, b): (usize, u8)| b.wrapping_add((i.wrapping_mul(7).wrapping_add(13)) as u8))
                .take(750 * 1024)
                .collect();
            assert_avx2_eq!(&data, 128);
        }
    }

    #[test]
    fn avx2_matches_scalar_empty() {
        #[cfg(target_arch = "x86_64")]
        assert_avx2_eq!(&[], 128);
    }

    #[test]
    fn avx2_matches_scalar_single_byte() {
        #[cfg(target_arch = "x86_64")]
        for b in 0u8..=255 {
            let data = [b];
            assert_avx2_eq!(&data, 128);
        }
    }

    #[test]
    fn avx2_matches_scalar_short_line_len() {
        #[cfg(target_arch = "x86_64")]
        {
            let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();
            for ll in [1, 2, 3, 4, 7, 16, 17, 32, 33] {
                assert_avx2_eq!(&data, ll);
            }
        }
    }

    #[test]
    fn avx2_round_trip() {
        #[cfg(target_arch = "x86_64")]
        {
            let data: Vec<u8> = (0u8..=255).cycle().take(750 * 1024).collect();
            let mut encoded = Vec::new();
            encode_avx2(&mut encoded, &data, 128);
            assert_eq!(decode(&encoded), data);
        }
    }

    // --- dispatcher always matches scalar ---

    #[test]
    fn dispatcher_matches_scalar_large_payload() {
        let data: Vec<u8> = (0u8..=255).cycle().take(750 * 1024).collect();
        let mut a = Vec::new();
        let mut b = Vec::new();
        super::encode(&mut a, &data, 128);
        encode_scalar(&mut b, &data, 128);
        assert_eq!(a, b);
    }

    // --- 26d: encoded_size matches actual output length ---

    fn check_encoded_size(data: &[u8], line_len: usize) {
        let predicted = encoded_size(data, line_len);
        let actual = encode_s(data, line_len).len();
        assert_eq!(
            predicted,
            actual,
            "encoded_size mismatch: predicted={predicted} actual={actual} \
             (data.len()={}, line_len={line_len})",
            data.len()
        );
    }

    #[test]
    fn encoded_size_matches_empty() {
        assert_eq!(encoded_size(&[], 128), 0);
    }

    #[test]
    fn encoded_size_matches_all_256_byte_values() {
        let data: Vec<u8> = (0u8..=255).collect();
        check_encoded_size(&data, 128);
    }

    #[test]
    fn encoded_size_matches_all_critical_bytes() {
        let data: Vec<u8> = [NUL_IN, LF_IN, CR_IN, EQ_IN]
            .iter()
            .cycle()
            .copied()
            .take(512)
            .collect();
        check_encoded_size(&data, 128);
    }

    #[test]
    fn encoded_size_matches_positional_bytes_at_boundaries() {
        let data: Vec<u8> = [DOT_IN, SP_IN, TAB_IN, 0x00]
            .iter()
            .cycle()
            .copied()
            .take(256)
            .collect();
        for ll in [1, 2, 3, 4, 7, 16, 17, 128] {
            check_encoded_size(&data, ll);
        }
    }

    #[test]
    fn encoded_size_matches_all_single_bytes() {
        for b in 0u8..=255 {
            check_encoded_size(&[b], 128);
            check_encoded_size(&[b], 1);
        }
    }

    #[test]
    fn encoded_size_matches_large_payload() {
        let data: Vec<u8> = (0u8..=255)
            .cycle()
            .enumerate()
            .map(|(i, b): (usize, u8)| b.wrapping_add((i.wrapping_mul(7).wrapping_add(13)) as u8))
            .take(750 * 1024)
            .collect();
        check_encoded_size(&data, 128);
    }
}
