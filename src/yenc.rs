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

    // Encoded data is roughly the same size as the input, plus escapes and
    // line endings; reserve generously to avoid reallocations on the hot path.
    let mut body = Vec::with_capacity(data.len() + data.len() / 32 + 128);

    if multipart {
        body.extend_from_slice(
            format!(
                "=ybegin part={} total={} line={} size={} name={}\r\n",
                spec.number, spec.total, line_len, file_size, name
            )
            .as_bytes(),
        );
        body.extend_from_slice(format!("=ypart begin={} end={}\r\n", begin, end).as_bytes());
    } else {
        body.extend_from_slice(
            format!(
                "=ybegin line={} size={} name={}\r\n",
                line_len, file_size, name
            )
            .as_bytes(),
        );
    }

    encode(&mut body, data, line_len);

    if multipart {
        body.extend_from_slice(
            format!(
                "=yend size={} part={} pcrc32={:08x}",
                data.len(),
                spec.number,
                part_crc
            )
            .as_bytes(),
        );
        if let Some(file_crc) = file_crc32 {
            body.extend_from_slice(format!(" crc32={:08x}", file_crc).as_bytes());
        }
        body.extend_from_slice(b"\r\n");
    } else {
        body.extend_from_slice(
            format!("=yend size={} crc32={:08x}\r\n", data.len(), part_crc).as_bytes(),
        );
    }

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

    for (i, &b) in data.iter().enumerate() {
        let at_line_start = col == 0;
        let at_line_end = col + 1 == line_len || i == last;
        let e = b.wrapping_add(42);

        let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
        let positional = ((e == 0x09 || e == 0x20) && (at_line_start || at_line_end))
            || (e == 0x2E && at_line_start);

        if critical || positional {
            out.push(b'=');
            out.push(e.wrapping_add(64));
        } else {
            out.push(e);
        }

        col += 1;
        if col == line_len {
            out.extend_from_slice(b"\r\n");
            col = 0;
        }
    }

    if col != 0 {
        out.extend_from_slice(b"\r\n");
    }
}

// --- SSSE3 path (x86-64 only) ---

/// SSSE3-accelerated yEnc encoder. Falls back to [`encode_scalar`] when the
/// CPU does not support SSSE3 (detected at runtime via `is_x86_feature_detected!`).
///
/// Produces identical output to [`encode_scalar`] for all inputs.
#[cfg(target_arch = "x86_64")]
pub fn encode_ssse3(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    if is_x86_feature_detected!("ssse3") {
        // SAFETY: we just confirmed the CPU supports SSSE3.
        unsafe { encode_ssse3_impl(out, data, line_len) }
    } else {
        encode_scalar(out, data, line_len)
    }
}

/// Encode one byte using full positional-escape rules and append to `out`.
/// `col` is advanced; if it reaches `line_len` a CRLF is emitted and `col`
/// is reset to 0.
#[inline(always)]
fn emit_scalar(out: &mut Vec<u8>, b: u8, col: &mut usize, line_len: usize, at_line_end: bool) {
    let e = b.wrapping_add(42);
    let at_line_start = *col == 0;
    let critical = matches!(e, 0x00 | 0x0A | 0x0D | 0x3D);
    let positional = ((e == 0x09 || e == 0x20) && (at_line_start || at_line_end))
        || (e == 0x2E && at_line_start);
    if critical || positional {
        out.push(b'=');
        out.push(e.wrapping_add(64));
    } else {
        out.push(e);
    }
    *col += 1;
    if *col == line_len {
        out.extend_from_slice(b"\r\n");
        *col = 0;
    }
}

/// Encode one shifted byte (already `b+42`) that is guaranteed to be in the
/// middle of a line — no positional escapes apply, only critical ones.
#[inline(always)]
fn emit_critical_only(out: &mut Vec<u8>, e: u8) {
    if matches!(e, 0x00 | 0x0A | 0x0D | 0x3D) {
        out.push(b'=');
        out.push(e.wrapping_add(64));
    } else {
        out.push(e);
    }
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "ssse3")]
unsafe fn encode_ssse3_impl(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    use std::arch::x86_64::*;

    let line_len = line_len.max(1);
    if data.is_empty() {
        return;
    }

    // Upper bound reserve: eliminates all per-chunk reserve() calls.
    out.reserve(data.len() * 2 + (data.len() / line_len + 1) * 2);

    let last = data.len() - 1;
    let add42 = _mm_set1_epi8(42i8);
    let v_nul = _mm_setzero_si128();
    let v_lf = _mm_set1_epi8(0x0Au8 as i8);
    let v_cr = _mm_set1_epi8(0x0Du8 as i8);
    let v_eq = _mm_set1_epi8(0x3Du8 as i8);

    let mut i = 0usize;
    let mut col = 0usize;

    while i < data.len() {
        // -- Line-start byte: always scalar (dot/space/tab positional escapes) --
        if col == 0 {
            let at_line_end = line_len == 1 || i == last;
            emit_scalar(out, data[i], &mut col, line_len, at_line_end);
            i += 1;
            continue;
        }

        // -- Middle zone: col in [1, line_len-2] and not the last data byte --
        // No positional escapes apply here; only critical bytes need escaping.
        let safe = if line_len > 1 {
            // bytes until we'd hit the last position of this line
            let till_line_end = line_len - 1 - col;
            // bytes until we'd hit the last byte of the data (needs scalar)
            let till_data_end = last.saturating_sub(i);
            till_line_end.min(till_data_end)
        } else {
            0
        };

        // SIMD: 2×16-byte unrolled loop — halves branch overhead for lines ≥ 32B safe zone.
        // The two loads are independent, giving the CPU better ILP than the 1×16 loop.
        let mut safe_rem = safe;
        while safe_rem >= 32 {
            let p_a = data.as_ptr().add(i) as *const __m128i;
            let p_b = data.as_ptr().add(i + 16) as *const __m128i;
            let chunk_a = _mm_loadu_si128(p_a);
            let chunk_b = _mm_loadu_si128(p_b);
            let shifted_a = _mm_add_epi8(chunk_a, add42);
            let shifted_b = _mm_add_epi8(chunk_b, add42);

            let any_a = _mm_or_si128(
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted_a, v_nul),
                    _mm_cmpeq_epi8(shifted_a, v_lf),
                ),
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted_a, v_cr),
                    _mm_cmpeq_epi8(shifted_a, v_eq),
                ),
            );
            let any_b = _mm_or_si128(
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted_b, v_nul),
                    _mm_cmpeq_epi8(shifted_b, v_lf),
                ),
                _mm_or_si128(
                    _mm_cmpeq_epi8(shifted_b, v_cr),
                    _mm_cmpeq_epi8(shifted_b, v_eq),
                ),
            );
            let mask_a = _mm_movemask_epi8(any_a);
            let mask_b = _mm_movemask_epi8(any_b);

            if mask_a | mask_b == 0 {
                // Fast path: both chunks clean — two consecutive stores.
                let old_len = out.len();
                let dst = out.as_mut_ptr().add(old_len);
                _mm_storeu_si128(dst as *mut __m128i, shifted_a);
                _mm_storeu_si128(dst.add(16) as *mut __m128i, shifted_b);
                out.set_len(old_len + 32);
            } else {
                // Slow path: at least one critical byte somewhere in the 32B block.
                if mask_a == 0 {
                    let old_len = out.len();
                    _mm_storeu_si128(out.as_mut_ptr().add(old_len) as *mut __m128i, shifted_a);
                    out.set_len(old_len + 16);
                } else {
                    let mut tmp = [0u8; 16];
                    _mm_storeu_si128(tmp.as_mut_ptr() as *mut __m128i, shifted_a);
                    for &e in &tmp {
                        emit_critical_only(out, e);
                    }
                }
                if mask_b == 0 {
                    let old_len = out.len();
                    _mm_storeu_si128(out.as_mut_ptr().add(old_len) as *mut __m128i, shifted_b);
                    out.set_len(old_len + 16);
                } else {
                    let mut tmp = [0u8; 16];
                    _mm_storeu_si128(tmp.as_mut_ptr() as *mut __m128i, shifted_b);
                    for &e in &tmp {
                        emit_critical_only(out, e);
                    }
                }
            }

            i += 32;
            col += 32;
            safe_rem -= 32;
        }

        // Single 16-byte chunk for the remainder (safe_rem in [16, 31]).
        while safe_rem >= 16 {
            let chunk = _mm_loadu_si128(data.as_ptr().add(i) as *const __m128i);
            let shifted = _mm_add_epi8(chunk, add42);

            // Build escape mask for the four critical values.
            let m0 = _mm_cmpeq_epi8(shifted, v_nul);
            let m1 = _mm_cmpeq_epi8(shifted, v_lf);
            let m2 = _mm_cmpeq_epi8(shifted, v_cr);
            let m3 = _mm_cmpeq_epi8(shifted, v_eq);
            let any = _mm_or_si128(_mm_or_si128(m0, m1), _mm_or_si128(m2, m3));
            let needs_escape = _mm_movemask_epi8(any);

            if needs_escape == 0 {
                // Fast path: capacity guaranteed by pre-reserve; write directly.
                let old_len = out.len();
                _mm_storeu_si128(out.as_mut_ptr().add(old_len) as *mut __m128i, shifted);
                out.set_len(old_len + 16);
            } else {
                // Slow path: one or more critical bytes in this chunk.
                let mut tmp = [0u8; 16];
                _mm_storeu_si128(tmp.as_mut_ptr() as *mut __m128i, shifted);
                for &e in &tmp {
                    emit_critical_only(out, e);
                }
            }

            i += 16;
            col += 16;
            safe_rem -= 16;
        }

        // Scalar tail of the safe zone (< 16 bytes, no positional escapes).
        while safe_rem > 0 {
            emit_critical_only(out, data[i].wrapping_add(42));
            i += 1;
            col += 1;
            safe_rem -= 1;
        }

        // -- Line-end byte OR last data byte: scalar --
        // (positional escapes for space/tab at line end; also handles i==last)
        if i < data.len() {
            let at_line_end = col + 1 == line_len || i == last;
            emit_scalar(out, data[i], &mut col, line_len, at_line_end);
            i += 1;
        }
    }

    // Trailing CRLF for a partial line.
    if col != 0 {
        out.extend_from_slice(b"\r\n");
    }
}

// --- AVX2 path (x86-64 only) ---

/// AVX2-accelerated yEnc encoder. Falls back to SSSE3 or scalar when the
/// required CPU features are absent (detected at runtime).
///
/// **Note:** for standard line lengths (128–256), SSSE3 is measurably faster
/// because the safe-zone per line (`line_len - 2` bytes) does not fill an
/// integer number of 32-byte AVX2 chunks, leaving a larger scalar tail than
/// SSSE3 would. [`encode`] therefore dispatches to SSSE3 in practice; this
/// function exists for benchmarking and for future multi-line processing.
///
/// Produces identical output to [`encode_scalar`] for all inputs.
#[cfg(target_arch = "x86_64")]
pub fn encode_avx2(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    if is_x86_feature_detected!("avx2") {
        // SAFETY: we just confirmed the CPU supports AVX2.
        unsafe { encode_avx2_impl(out, data, line_len) }
    } else {
        encode_ssse3(out, data, line_len)
    }
}

/// Runtime-dispatched encoder: picks the fastest path available on this CPU.
///
/// For `line_len` values used in practice (128–256), SSSE3 (16-byte chunks)
/// consistently outperforms AVX2 (32-byte chunks) because the safe zone per
/// line — `line_len - 2` bytes — does not divide evenly into 32-byte chunks,
/// leaving too large a scalar tail. AVX2 is kept for benchmarking and future
/// multi-line processing (phase 27d).
///
/// Priority: SSSE3 > scalar. (AVX2 reserved for multi-line phase.)
#[cfg(target_arch = "x86_64")]
pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    if is_x86_feature_detected!("ssse3") {
        unsafe { encode_ssse3_impl(out, data, line_len) }
    } else {
        encode_scalar(out, data, line_len)
    }
}

/// Runtime-dispatched encoder for non-x86 targets (scalar only).
#[cfg(not(target_arch = "x86_64"))]
pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    encode_scalar(out, data, line_len)
}

#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn encode_avx2_impl(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    use std::arch::x86_64::*;

    let line_len = line_len.max(1);
    if data.is_empty() {
        return;
    }

    // Upper bound reserve: eliminates all per-chunk reserve() calls.
    out.reserve(data.len() * 2 + (data.len() / line_len + 1) * 2);

    let last = data.len() - 1;
    let add42 = _mm256_set1_epi8(42i8);
    let v_nul = _mm256_setzero_si256();
    let v_lf = _mm256_set1_epi8(0x0Au8 as i8);
    let v_cr = _mm256_set1_epi8(0x0Du8 as i8);
    let v_eq = _mm256_set1_epi8(0x3Du8 as i8);

    let mut i = 0usize;
    let mut col = 0usize;

    while i < data.len() {
        // -- Line-start byte: always scalar (dot/space/tab positional escapes) --
        if col == 0 {
            let at_line_end = line_len == 1 || i == last;
            emit_scalar(out, data[i], &mut col, line_len, at_line_end);
            i += 1;
            continue;
        }

        // -- Middle zone: col in [1, line_len-2] and not the last data byte --
        let safe = if line_len > 1 {
            let till_line_end = line_len - 1 - col;
            let till_data_end = last.saturating_sub(i);
            till_line_end.min(till_data_end)
        } else {
            0
        };

        // AVX2: 32-byte chunks
        let mut safe_rem = safe;
        while safe_rem >= 32 {
            let chunk = _mm256_loadu_si256(data.as_ptr().add(i) as *const __m256i);
            let shifted = _mm256_add_epi8(chunk, add42);

            // Build escape mask for the four critical values.
            let m0 = _mm256_cmpeq_epi8(shifted, v_nul);
            let m1 = _mm256_cmpeq_epi8(shifted, v_lf);
            let m2 = _mm256_cmpeq_epi8(shifted, v_cr);
            let m3 = _mm256_cmpeq_epi8(shifted, v_eq);
            let any = _mm256_or_si256(_mm256_or_si256(m0, m1), _mm256_or_si256(m2, m3));
            let needs_escape = _mm256_movemask_epi8(any);

            if needs_escape == 0 {
                // Fast path: capacity guaranteed by pre-reserve; write directly.
                let old_len = out.len();
                _mm256_storeu_si256(out.as_mut_ptr().add(old_len) as *mut __m256i, shifted);
                out.set_len(old_len + 32);
            } else {
                // Slow path: one or more critical bytes in this chunk.
                let mut tmp = [0u8; 32];
                _mm256_storeu_si256(tmp.as_mut_ptr() as *mut __m256i, shifted);
                for &e in &tmp {
                    emit_critical_only(out, e);
                }
            }

            i += 32;
            col += 32;
            safe_rem -= 32;
        }

        // SSSE3 remainder: 16-byte chunks from what AVX2 couldn't cover.
        // Reuse the 128-bit registers rather than calling encode_ssse3_impl to
        // avoid re-entering the outer boundary logic.
        let add42_128 = _mm_set1_epi8(42i8);
        let v_nul_128 = _mm_setzero_si128();
        let v_lf_128 = _mm_set1_epi8(0x0Au8 as i8);
        let v_cr_128 = _mm_set1_epi8(0x0Du8 as i8);
        let v_eq_128 = _mm_set1_epi8(0x3Du8 as i8);
        while safe_rem >= 16 {
            let chunk = _mm_loadu_si128(data.as_ptr().add(i) as *const __m128i);
            let shifted = _mm_add_epi8(chunk, add42_128);
            let m0 = _mm_cmpeq_epi8(shifted, v_nul_128);
            let m1 = _mm_cmpeq_epi8(shifted, v_lf_128);
            let m2 = _mm_cmpeq_epi8(shifted, v_cr_128);
            let m3 = _mm_cmpeq_epi8(shifted, v_eq_128);
            let any = _mm_or_si128(_mm_or_si128(m0, m1), _mm_or_si128(m2, m3));
            if _mm_movemask_epi8(any) == 0 {
                let old_len = out.len();
                _mm_storeu_si128(out.as_mut_ptr().add(old_len) as *mut __m128i, shifted);
                out.set_len(old_len + 16);
            } else {
                let mut tmp = [0u8; 16];
                _mm_storeu_si128(tmp.as_mut_ptr() as *mut __m128i, shifted);
                for &e in &tmp {
                    emit_critical_only(out, e);
                }
            }
            i += 16;
            col += 16;
            safe_rem -= 16;
        }

        // Scalar tail of the safe zone (< 16 bytes, no positional escapes).
        while safe_rem > 0 {
            emit_critical_only(out, data[i].wrapping_add(42));
            i += 1;
            col += 1;
            safe_rem -= 1;
        }

        // -- Line-end byte OR last data byte: scalar --
        if i < data.len() {
            let at_line_end = col + 1 == line_len || i == last;
            emit_scalar(out, data[i], &mut col, line_len, at_line_end);
            i += 1;
        }
    }

    // Trailing CRLF for a partial line.
    if col != 0 {
        out.extend_from_slice(b"\r\n");
    }
}

#[cfg(test)]
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
    fn tab_escaped_at_line_end() {
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
    fn space_escaped_at_line_end() {
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
        let data: Vec<u8> = (0u8..=255).collect();
        assert_ssse3_eq!(&data, 128);
    }

    #[test]
    fn ssse3_matches_scalar_all_critical_bytes() {
        // All four critical raw inputs, repeated to span many lines.
        let data: Vec<u8> = [NUL_IN, LF_IN, CR_IN, EQ_IN]
            .iter()
            .cycle()
            .copied()
            .take(512)
            .collect();
        assert_ssse3_eq!(&data, 128);
    }

    #[test]
    fn ssse3_matches_scalar_positional_bytes_at_boundaries() {
        // Dot/space/tab at line start and end with line_len=4.
        let data: Vec<u8> = [DOT_IN, SP_IN, TAB_IN, 0x00]
            .iter()
            .cycle()
            .copied()
            .take(256)
            .collect();
        assert_ssse3_eq!(&data, 4);
    }

    #[test]
    fn ssse3_matches_scalar_large_random_like_payload() {
        // 750 KB of pseudo-random data (covers the typical article size).
        let data: Vec<u8> = (0u8..=255)
            .cycle()
            .enumerate()
            .map(|(i, b): (usize, u8)| b.wrapping_add((i.wrapping_mul(7).wrapping_add(13)) as u8))
            .take(750 * 1024)
            .collect();
        assert_ssse3_eq!(&data, 128);
    }

    #[test]
    fn ssse3_matches_scalar_empty() {
        assert_ssse3_eq!(&[], 128);
    }

    #[test]
    fn ssse3_matches_scalar_single_byte() {
        for b in 0u8..=255 {
            let data = [b];
            assert_ssse3_eq!(&data, 128);
        }
    }

    #[test]
    fn ssse3_matches_scalar_short_line_len() {
        // Stress the boundary logic with very short lines.
        let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        for ll in [1, 2, 3, 4, 7, 16, 17] {
            assert_ssse3_eq!(&data, ll);
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
        let data: Vec<u8> = (0u8..=255).collect();
        assert_avx2_eq!(&data, 128);
    }

    #[test]
    fn avx2_matches_scalar_all_critical_bytes() {
        let data: Vec<u8> = [NUL_IN, LF_IN, CR_IN, EQ_IN]
            .iter()
            .cycle()
            .copied()
            .take(512)
            .collect();
        assert_avx2_eq!(&data, 128);
    }

    #[test]
    fn avx2_matches_scalar_positional_bytes_at_boundaries() {
        let data: Vec<u8> = [DOT_IN, SP_IN, TAB_IN, 0x00]
            .iter()
            .cycle()
            .copied()
            .take(256)
            .collect();
        assert_avx2_eq!(&data, 4);
    }

    #[test]
    fn avx2_matches_scalar_large_random_like_payload() {
        let data: Vec<u8> = (0u8..=255)
            .cycle()
            .enumerate()
            .map(|(i, b): (usize, u8)| b.wrapping_add((i.wrapping_mul(7).wrapping_add(13)) as u8))
            .take(750 * 1024)
            .collect();
        assert_avx2_eq!(&data, 128);
    }

    #[test]
    fn avx2_matches_scalar_empty() {
        assert_avx2_eq!(&[], 128);
    }

    #[test]
    fn avx2_matches_scalar_single_byte() {
        for b in 0u8..=255 {
            let data = [b];
            assert_avx2_eq!(&data, 128);
        }
    }

    #[test]
    fn avx2_matches_scalar_short_line_len() {
        let data: Vec<u8> = (0u8..=255).cycle().take(512).collect();
        for ll in [1, 2, 3, 4, 7, 16, 17, 32, 33] {
            assert_avx2_eq!(&data, ll);
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
