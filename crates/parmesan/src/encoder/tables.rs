use super::*;

/// Pre-computed AVX-512/GFNI coefficient table for one (recovery_block, input_slice) pair.
/// Two 512-bit matrix registers (mat_lo, mat_hi) plus 256-entry scalar lookup tables.
#[cfg(target_arch = "x86_64")]
pub(super) type Avx512GfniTable = (__m512i, __m512i, [u16; 256], [u16; 256]);

#[cfg(target_arch = "x86_64")]
pub(super) type Avx512ShuffleTable = (
    __m512i,
    __m512i,
    __m512i,
    __m512i,
    __m512i,
    __m512i,
    __m512i,
    __m512i,
    [u16; 256],
    [u16; 256],
);

/// Pre-computed AVX2/GFNI coefficient table for one (recovery_block, input_slice) pair.
/// Two 256-bit matrix registers (mat_lo, mat_hi) plus 256-entry scalar lookup tables.
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
pub(super) type Avx2GfniTable = (__m256i, __m256i, [u16; 256], [u16; 256]);

/// Pre-computed SSSE3 coefficient table for one (recovery_block, input_slice) pair.
/// Eight 128-bit shuffle vectors plus 256-entry scalar lookup tables.
#[cfg(target_arch = "x86_64")]
pub(super) type Ssse3Table = (
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    __m128i,
    [u16; 256],
    [u16; 256],
);

/// Pre-computed AVX2/Shuffle2x coefficient table for one (recovery_block, input_slice) pair.
/// Four 256-bit shuffle vectors where each `__m256i` packs two 16-entry nibble tables
/// into its two 128-bit lanes, enabling the Shuffle2x kernel to use 4 PSHUFB instead of 8.
///
/// Layout (where loNk[n] = (gf.mul(n<<4k, c) & 0xFF), hiNk[n] = (gf.mul(n<<4k, c) >> 8)):
///   tNormA: lane0 = loN0, lane1 = hiN2
///   tNormB: lane0 = loN1, lane1 = hiN3
///   tSwapA: lane0 = loN2, lane1 = hiN0
///   tSwapB: lane0 = loN3, lane1 = hiN1
#[cfg(target_arch = "x86_64")]
#[allow(dead_code)]
pub(super) type Avx2Shuffle2xTable = (
    __m256i,    // tNormA
    __m256i,    // tNormB
    __m256i,    // tSwapA
    __m256i,    // tSwapB
    [u16; 256], // scalar table_low  (fallback / for the test harness)
    [u16; 256], // scalar table_high
);

/// Pre-computed AVX2 coefficient table for one (recovery_block, input_slice) pair.
/// Eight 256-bit shuffle vectors covering the four nibble × two byte-half combinations,
/// plus full 256-entry lookup tables for the scalar tail handler.
#[cfg(target_arch = "x86_64")]
pub(super) type Avx2Table = (
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    __m256i,
    [u16; 256],
    [u16; 256],
);

/// One ALTMAP output plane's decoded dependency list: `plane_out` is XORed
/// from `plane_ins[..n_ins]` (indices into the 16 input planes).
#[cfg(target_arch = "x86_64")]
#[derive(Clone, Copy)]
pub(super) struct PlaneOutDeps {
    pub(super) plane_out: u8,
    pub(super) plane_ins: [u8; 16],
    pub(super) n_ins: u8,
}

/// Decode a coefficient's 16-bit-mask dependency matrix (`xor_dep_matrix`'s
/// output) into a flat, branch-free-to-walk list of `(plane_out, plane_ins)`
/// pairs, skipping any `plane_out` with an all-zero mask.
///
/// §148: `flush_avx2_altmap_work` used to re-test all 256 `(plane_out,
/// plane_in)` bit positions on *every* 32-byte output vector (`n_vec` of them
/// per recovery-chunk × input-slice pair — in the thousands for a large
/// file). The mask only depends on the coefficient, which is fixed for the
/// whole `vi` loop, so decoding it once up front and walking a plain index
/// list inside the hot loop removes that redundant re-decoding without
/// changing which XORs happen or their order (XOR is commutative/
/// associative, so reordering by plane_out is safe).
#[cfg(target_arch = "x86_64")]
pub(super) fn decode_plane_deps(deps: &[u16; 16]) -> ([PlaneOutDeps; 16], usize) {
    let mut out = [PlaneOutDeps {
        plane_out: 0,
        plane_ins: [0; 16],
        n_ins: 0,
    }; 16];
    let mut count = 0;
    for (plane_out, &mask) in deps.iter().enumerate() {
        if mask == 0 {
            continue;
        }
        let mut plane_ins = [0u8; 16];
        let mut n_ins = 0u8;
        for plane_in in 0..16u8 {
            if (mask >> plane_in) & 1 == 1 {
                plane_ins[n_ins as usize] = plane_in;
                n_ins += 1;
            }
        }
        out[count] = PlaneOutDeps {
            plane_out: plane_out as u8,
            plane_ins,
            n_ins,
        };
        count += 1;
    }
    (out, count)
}

/// Four 8×8 GF(2) matrices for Affine GFNI (`ll`, `lh`, `hl`, `hh`).
/// Byte 7 of each u64 is row 0 of `gf2p8affine` (Intel SDM).
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
pub(super) fn gfni_affine_u64_mats(gf: &Gf16, coeff: u16) -> (u64, u64, u64, u64) {
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
    (m_ll, m_lh, m_hl, m_hh)
}

/// Parpar Affine nibble scratch: 16×4 matrices (`gf16_affine_init_avx2` /
/// `gf16_bitdep_init256` with `genAffine=1`). A coefficient's four 8×8
/// matrices are the XOR of the four nibble contributions instead of an 8×8
/// rebuild per (recovery, source) pair.
#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
#[repr(align(32))]
pub(super) struct AffineNibbleScratch {
    /// `mats[nibble][slot]` for `coeff` nibble `slot` (weight `1<<(4*slot)`).
    /// The physical qword order is `ll`, `hh`, `hl`, `lh`, matching ParPar's
    /// `gf16_bitdep256_swap(..., genAffine=1)` and its AVX-512 lane shuffles.
    pub(super) mats: [[(u64, u64, u64, u64); 4]; 16],
}

#[cfg_attr(not(target_arch = "x86_64"), allow(dead_code))]
impl AffineNibbleScratch {
    pub(super) fn new(gf: &Gf16) -> Self {
        let mut mats = [[(0u64, 0u64, 0u64, 0u64); 4]; 16];
        for n in 0..16u16 {
            for slot in 0..4u16 {
                let (ll, lh, hl, hh) = gfni_affine_u64_mats(gf, n << (4 * slot));
                mats[n as usize][slot as usize] = (ll, hh, hl, lh);
            }
        }
        Self { mats }
    }

    pub(super) fn load(&self, coeff: u16) -> (u64, u64, u64, u64) {
        let xor = |a: (u64, u64, u64, u64), b: (u64, u64, u64, u64)| {
            (a.0 ^ b.0, a.1 ^ b.1, a.2 ^ b.2, a.3 ^ b.3)
        };
        let mut m = self.mats[(coeff & 0xf) as usize][0];
        m = xor(m, self.mats[((coeff >> 4) & 0xf) as usize][1]);
        m = xor(m, self.mats[((coeff >> 8) & 0xf) as usize][2]);
        m = xor(m, self.mats[((coeff >> 12) & 0xf) as usize][3]);
        (m.0, m.3, m.2, m.1)
    }
}
