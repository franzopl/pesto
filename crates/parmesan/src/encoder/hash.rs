use super::*;

/// MD5 and length of a whole file, plus the MD5 of its first 16 KiB.
#[derive(Debug, Clone)]
pub struct FileHashes {
    pub md5_full: [u8; 16],
    pub md5_16k: [u8; 16],
    pub length: u64,
}

/// Computes [`FileHashes`] from a file's real bytes, fed incrementally,
/// and computes per-slice checksums simultaneously.
pub struct FileHasher {
    full: md5_many::Md5State,
    head: Md5,
    head_consumed: usize,
    length: u64,
    many: md5_many::Md5Many,
}

impl FileHasher {
    /// Start hashing a new file.
    pub fn new() -> Self {
        Self {
            full: md5_many::Md5State::new(),
            head: Md5::new(),
            head_consumed: 0,
            length: 0,
            many: md5_many::Md5Many::new(),
        }
    }

    /// Feed more of the file's real (unpadded) bytes.
    pub fn update(&mut self, data: &[u8]) {
        self.full.update(data);
        self.length += data.len() as u64;
        let room = HEAD_LEN - self.head_consumed;
        if room > 0 {
            let take = room.min(data.len());
            self.head.update(&data[..take]);
            self.head_consumed += take;
        }
    }

    /// Update the file hash with the unpadded portion of the slice,
    /// and simultaneously compute the MD5 and CRC32 of the fully padded slice.
    pub fn update_and_hash_slice(
        &mut self,
        padded_slice: &[u8],
        actual_len: usize,
    ) -> SliceChecksum {
        let mut slice_state = md5_many::Md5State::new();
        let mut crc = crc32fast::Hasher::new();

        // Feed the 16k head if still needed
        let unpadded = &padded_slice[..actual_len];
        self.length += actual_len as u64;
        let room = HEAD_LEN - self.head_consumed;
        if room > 0 {
            let take = room.min(actual_len);
            self.head.update(&unpadded[..take]);
            self.head_consumed += take;
        }

        // Process chunk by chunk to maintain L1 cache locality for CRC
        for chunk_start in (0..padded_slice.len()).step_by(64 * 1024) {
            let chunk_end = (chunk_start + 64 * 1024).min(padded_slice.len());
            let chunk = &padded_slice[chunk_start..chunk_end];
            crc.update(chunk);

            // Compute how much of this chunk is actual unpadded data
            let chunk_actual_len = if chunk_start >= actual_len {
                0
            } else {
                (actual_len - chunk_start).min(chunk.len())
            };

            if chunk_actual_len == chunk.len() {
                // Entire chunk is unpadded data: hash simultaneously
                let mut states = [self.full, slice_state];
                self.many.update_many(&mut states, &[chunk, chunk]);
                self.full = states[0];
                slice_state = states[1];
            } else if chunk_actual_len > 0 {
                // Partial chunk: simultaneously hash the unpadded part, then individually hash the rest of the padding for the slice
                let unpadded_part = &chunk[..chunk_actual_len];
                let padded_part = &chunk[chunk_actual_len..];

                let mut states = [self.full, slice_state];
                self.many
                    .update_many(&mut states, &[unpadded_part, unpadded_part]);
                self.full = states[0];
                slice_state = states[1];

                slice_state.update(padded_part);
            } else {
                // Completely padding
                slice_state.update(chunk);
            }
        }

        SliceChecksum {
            md5: slice_state.finalize(),
            crc32: crc.finalize(),
        }
    }

    /// Finish and return the hashes.
    pub fn finish(self) -> FileHashes {
        let mut md5_16k = [0u8; 16];
        md5_16k.copy_from_slice(&self.head.finalize());
        FileHashes {
            md5_full: self.full.finalize(),
            md5_16k,
            length: self.length,
        }
    }
}

impl Default for FileHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Barrett polynomial reduction for GF(2^16)/0x1100B.
///
/// On entry the six `poly16x8_t` arguments hold the XOR-accumulated outputs of
/// `pmull_lo`/`pmull_hi` for a Karatsuba product:
///   `low1, low2`  — lo_byte(input) × lo_byte(coeff)  (lower and upper 8 lanes)
///   `mid1, mid2`  — (lo^hi)(input) × (lo^hi)(coeff)
///   `high1, high2`— hi_byte(input) × hi_byte(coeff)
///
/// On return the result lives in "split" format:
///   lo_bytes_of_result = `vreinterpretq_u8_p16(*low1 ^ *low2)`
///   hi_bytes_of_result = `vreinterpretq_u8_p16(*high1 ^ *high2)`
///
/// Ported from ParPar `gf16_clmul_neon.h` (MIT licence, © animetosho).
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
pub(super) unsafe fn gf16_clmul_reduce_neon(
    low1: &mut poly16x8_t,
    low2: &mut poly16x8_t,
    mid1: poly16x8_t,
    mid2: poly16x8_t,
    high1: &mut poly16x8_t,
    high2: &mut poly16x8_t,
) {
    use std::arch::aarch64::*;

    // Deinterleave the 16-bit poly results into even/odd byte planes.
    // After vuzpq_u8(a16, b16):  val[0] = even bytes of a16 ++ even bytes of b16
    //                             val[1] = odd  bytes of a16 ++ odd  bytes of b16
    let hib = vuzpq_u8(vreinterpretq_u8_p16(*high1), vreinterpretq_u8_p16(*high2));
    let lob = vuzpq_u8(vreinterpretq_u8_p16(*low1), vreinterpretq_u8_p16(*low2));
    let mib = vuzpq_u8(vreinterpretq_u8_p16(mid1), vreinterpretq_u8_p16(mid2));
    // hib.val[0] = bits 16-23 of unreduced product (per element)
    // hib.val[1] = bits 24-30 of unreduced product
    // lob.val[0] = bits  0- 7
    // lob.val[1] = bits  8-14 (bit 15 is always 0: 8b×8b → 15-bit product)
    // mib.val[0/1] = low/high bytes of Karatsuba middle

    // Merge the middle Karatsuba term to assemble the 31-bit product bytes.
    let lib = veorq_u8(hib.0, lob.1); // cross-overlap
    let lob1 = veorq_u8(veorq_u8(lib, lob.0), mib.0); // bits  8-15
    let hib0 = veorq_u8(veorq_u8(lib, hib.1), mib.1); // bits 16-23

    // Barrett reduction.  Polynomial: 0x1100B = x^16 + x^12 + x^3 + x + 1.
    // The high word (15 bits) lives in (hib0 | hib.val[1]<<8).
    // Step 1: quotient approximation th0 = bits 20-27 of the product.
    let th0_a = vsriq_n_u8::<4>(vshlq_n_u8::<4>(hib.1), hib0);
    let th1_a = veorq_u8(hib.1, vshrq_n_u8::<4>(hib.1));
    let mut th0 = veorq_u8(veorq_u8(th0_a, th1_a), hib0);

    // Step 2: extract top 3 bits of th0, then XOR-fold (th0_hi3 ^= th0_hi3 >> 2).
    // Implemented via vqtbl1q_u8 lookup (no SHA3 EOR3 needed).
    // Table encodes n ^ (n >> 2) for n ∈ 0..8; indices 8-15 are unused (→ 0).
    let th0_hi3 = vshrq_n_u8::<5>(th0);
    const TBL: [u8; 16] = [0, 1, 2, 3, 5, 4, 7, 6, 0, 0, 0, 0, 0, 0, 0, 0];
    let tbl_v = vld1q_u8(TBL.as_ptr());
    let th0_hi3r = vqtbl1q_u8(tbl_v, th0_hi3);

    // Fold the high-byte contribution (shift-5 term).
    th0 = veorq_u8(th0, vshrq_n_u8::<5>(hib.1));

    // Step 3: multiply by 0x0b = x^3 + x + 1 (low coefficient of 0x100B).
    // vmulq_p8: polynomial multiply truncated to 8 bits (PMUL.16B instruction).
    let red_l = vdupq_n_p8(0x0b);
    let hib1_new = vsliq_n_u8::<4>(th0_hi3r, th0);
    let th1_new = vreinterpretq_u8_p8(vmulq_p8(vreinterpretq_p8_u8(th1_a), red_l));
    let hib0_new = vreinterpretq_u8_p8(vmulq_p8(vreinterpretq_p8_u8(th0), red_l));

    // Pack into split format (caller XORs low1^low2 → lo lane, high1^high2 → hi lane).
    *low1 = vreinterpretq_p16_u8(lob.0);
    *low2 = vreinterpretq_p16_u8(hib0_new);
    *high1 = vreinterpretq_p16_u8(veorq_u8(hib1_new, th1_new));
    *high2 = vreinterpretq_p16_u8(lob1);
}

/// MD5 + CRC32 checksum of one zero-padded input slice (for the IFSC packet).
///
/// One walk of the buffer: the previous implementation hashed twice
/// (separate `md5` and `crc32` passes). Parpar fuses both on the input
/// stream; this is the portable equivalent until a SIMD MD5×2 lands.
pub fn slice_checksum(padded_slice: &[u8]) -> SliceChecksum {
    let mut digest = Md5::new();
    let mut crc = crc32fast::Hasher::new();
    for chunk in padded_slice.chunks(64 * 1024) {
        digest.update(chunk);
        crc.update(chunk);
    }
    let mut md5_out = [0u8; 16];
    md5_out.copy_from_slice(&digest.finalize());
    SliceChecksum {
        md5: md5_out,
        crc32: crc.finalize(),
    }
}

/// Compute checksums for an equal-sized encoder batch, filling independent
/// MD5 lanes while CRC32 runs on the remaining Rayon workers.
#[cfg(target_arch = "x86_64")]
pub(super) fn slice_checksums_batch(padded_slices: &[Vec<u8>]) -> Vec<SliceChecksum> {
    if padded_slices.is_empty() {
        return Vec::new();
    }

    let inputs: Vec<&[u8]> = padded_slices.iter().map(Vec::as_slice).collect();
    let (md5s, crc32s) = rayon::join(
        || {
            let mut outputs = vec![[0u8; 16]; inputs.len()];
            md5_many::Md5Many::new().hash_many(&inputs, &mut outputs);
            outputs
        },
        || {
            padded_slices
                .par_iter()
                .map(|slice| crc32fast::hash(slice))
                .collect::<Vec<_>>()
        },
    );

    md5s.into_iter()
        .zip(crc32s)
        .map(|(md5, crc32)| SliceChecksum { md5, crc32 })
        .collect()
}
