//! yEnc encoder.
//!
//! Encodes raw file bytes into yEnc article bodies, including the `=ybegin`,
//! `=ypart` and `=yend` control lines, and splits files into multi-part
//! segments. This is the hot path — the inner [`encode_into`] loop allocates
//! nothing beyond the output buffer.
//!
//! Reference: the yEnc draft specification, <http://www.yenc.org/yenc-draft.1.3.txt>.

mod tables;

#[cfg(target_arch = "aarch64")]
pub mod aarch64;
pub mod scalar;
#[cfg(target_arch = "x86_64")]
pub mod x86;

#[cfg(test)]
mod tests;

#[cfg(target_arch = "aarch64")]
pub use aarch64::encode_neon;
pub use scalar::encode_scalar;
#[cfg(target_arch = "x86_64")]
pub use x86::{encode_avx2, encode_ssse3};

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
    let total_capacity =
        ybegin.len() + ypart.as_ref().map_or(0, |p| p.len()) + data_encoded_size + yend.len();

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
        unsafe { aarch64::encoded_size_neon(data, line_len) }
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

/// Architecture-dispatched yEnc encoder. Selects the best available backend at
/// runtime: AVX2 > SSSE3 > NEON > scalar.
#[cfg(target_arch = "x86_64")]
pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    x86::encode(out, data, line_len)
}

#[cfg(target_arch = "aarch64")]
pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    aarch64::encode(out, data, line_len)
}

#[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
pub fn encode(out: &mut Vec<u8>, data: &[u8], line_len: usize) {
    scalar::encode_scalar(out, data, line_len)
}
