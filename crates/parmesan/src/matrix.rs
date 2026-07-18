//! GF(2¹⁶) linear algebra for PAR2 repair.
//!
//! Repair needs one capability the encoder never does: inverting a small
//! square matrix over GF(2¹⁶). Given `m` missing input blocks and `m`
//! available recovery blocks, the reduced Reed-Solomon matrix `A` (row `r`
//! = a chosen recovery exponent, column `c` = a missing input block's base
//! constant) is a submatrix of a Vandermonde-like matrix and is always
//! invertible over the field — that's the maximum-distance-separable (MDS)
//! property that makes Reed-Solomon repair work at all. This module builds
//! that matrix and inverts it via Gauss-Jordan elimination; see
//! [`crate::decoder`] for how the inverse is turned into reconstructed slice
//! data.

use crate::gf16::Gf16;

/// A square matrix over GF(2¹⁶), stored row-major.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Gf16Matrix {
    n: usize,
    data: Vec<u16>,
}

/// The reduced matrix turned out to be singular.
///
/// For a matrix built by [`Gf16Matrix::build_reduced`] from valid PAR2 data
/// this should be unreachable — it is a real GF(2¹⁶) Vandermonde-style
/// submatrix, which is always invertible. Seeing this error means the
/// caller fed in something that isn't actually such a matrix (e.g.
/// duplicate bases or duplicate exponents), which is a logic bug in the
/// caller, not a property of the input data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SingularMatrix;

impl std::fmt::Display for SingularMatrix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "GF(2^16) matrix is singular: no non-zero pivot found; \
             this indicates a bad block selection, not damaged input data"
        )
    }
}

impl std::error::Error for SingularMatrix {}

impl Gf16Matrix {
    /// An `n×n` matrix of zeros.
    pub fn zero(n: usize) -> Self {
        Self {
            n,
            data: vec![0u16; n * n],
        }
    }

    /// The `n×n` identity matrix.
    pub fn identity(n: usize) -> Self {
        let mut m = Self::zero(n);
        for i in 0..n {
            m.set(i, i, 1);
        }
        m
    }

    /// Build the `m×m` reduced Reed-Solomon matrix relating `m` missing
    /// input blocks to `m` selected recovery blocks.
    ///
    /// Row `r` corresponds to the recovery block with exponent
    /// `exponents[r]`; column `c` corresponds to the missing input block
    /// with base constant `bases[c]`. Entry `A[r][c] = bases[c] ^
    /// exponents[r]` (GF(2¹⁶) exponentiation) — exactly the coefficient the
    /// encoder applies to that input block when forming that recovery
    /// block (see [`crate::gf16::Gf16::recovery_coefficient`]).
    ///
    /// # Panics
    ///
    /// Panics if `bases.len() != exponents.len()`.
    pub fn build_reduced(gf: &Gf16, bases: &[u16], exponents: &[u32]) -> Self {
        assert_eq!(
            bases.len(),
            exponents.len(),
            "the reduced matrix must be square: got {} bases and {} exponents",
            bases.len(),
            exponents.len()
        );
        let n = bases.len();
        let mut m = Self::zero(n);
        for (r, &e) in exponents.iter().enumerate() {
            for (c, &b) in bases.iter().enumerate() {
                m.set(r, c, gf.pow(b, e));
            }
        }
        m
    }

    /// Matrix dimension (`n` for an `n×n` matrix).
    pub fn n(&self) -> usize {
        self.n
    }

    /// Entry at row `r`, column `c`.
    pub fn get(&self, r: usize, c: usize) -> u16 {
        self.data[r * self.n + c]
    }

    /// Set the entry at row `r`, column `c`.
    pub fn set(&mut self, r: usize, c: usize, v: u16) {
        self.data[r * self.n + c] = v;
    }

    /// Row `r` as a slice of `n` coefficients.
    pub fn row(&self, r: usize) -> &[u16] {
        &self.data[r * self.n..(r + 1) * self.n]
    }

    fn swap_rows(&mut self, a: usize, b: usize) {
        if a == b {
            return;
        }
        let n = self.n;
        let (lo, hi) = if a < b { (a, b) } else { (b, a) };
        let (head, tail) = self.data.split_at_mut(hi * n);
        head[lo * n..lo * n + n].swap_with_slice(&mut tail[..n]);
    }

    /// Invert this matrix via Gauss-Jordan elimination over GF(2¹⁶).
    ///
    /// GF(2¹⁶) arithmetic is exact (no floating-point rounding), so the only
    /// failure mode is a genuinely singular matrix — see [`SingularMatrix`].
    pub fn invert(&self, gf: &Gf16) -> Result<Self, SingularMatrix> {
        let n = self.n;
        let mut a = self.clone();
        let mut inv = Self::identity(n);

        for col in 0..n {
            let pivot_row = (col..n)
                .find(|&r| a.get(r, col) != 0)
                .ok_or(SingularMatrix)?;
            a.swap_rows(col, pivot_row);
            inv.swap_rows(col, pivot_row);

            let pivot_inv = gf.inverse(a.get(col, col));
            if pivot_inv != 1 {
                for c in 0..n {
                    let v = a.get(col, c);
                    if v != 0 {
                        a.set(col, c, gf.mul(v, pivot_inv));
                    }
                    let v = inv.get(col, c);
                    if v != 0 {
                        inv.set(col, c, gf.mul(v, pivot_inv));
                    }
                }
            }

            for r in 0..n {
                if r == col {
                    continue;
                }
                let factor = a.get(r, col);
                if factor == 0 {
                    continue;
                }
                for c in 0..n {
                    let a_val = a.get(col, c);
                    if a_val != 0 {
                        let new = a.get(r, c) ^ gf.mul(factor, a_val);
                        a.set(r, c, new);
                    }
                    let inv_val = inv.get(col, c);
                    if inv_val != 0 {
                        let new = inv.get(r, c) ^ gf.mul(factor, inv_val);
                        inv.set(r, c, new);
                    }
                }
            }
        }

        Ok(inv)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mat_mul(gf: &Gf16, a: &Gf16Matrix, b: &Gf16Matrix) -> Gf16Matrix {
        assert_eq!(a.n(), b.n());
        let n = a.n();
        let mut out = Gf16Matrix::zero(n);
        for r in 0..n {
            for c in 0..n {
                let mut acc = 0u16;
                for k in 0..n {
                    acc ^= gf.mul(a.get(r, k), b.get(k, c));
                }
                out.set(r, c, acc);
            }
        }
        out
    }

    #[test]
    fn identity_inverts_to_itself() {
        let gf = Gf16::new();
        let id = Gf16Matrix::identity(5);
        let inv = id.invert(&gf).unwrap();
        assert_eq!(inv, id);
    }

    #[test]
    fn reduced_matrix_inverts_and_satisfies_a_times_a_inv_is_identity() {
        let gf = Gf16::new();
        for &m in &[1usize, 2, 5, 16, 37] {
            let bases = gf.input_bases(m);
            // Use m distinct, arbitrary recovery exponents.
            let exponents: Vec<u32> = (0..m as u32).map(|i| i * 3 + 1).collect();
            let a = Gf16Matrix::build_reduced(&gf, &bases, &exponents);
            let inv = a.invert(&gf).expect("RS submatrix must be invertible");
            let product = mat_mul(&gf, &a, &inv);
            assert_eq!(product, Gf16Matrix::identity(m), "m={m}");
        }
    }

    #[test]
    fn reduced_matrix_is_invertible_for_arbitrary_exponent_choices() {
        // The MDS property: any m distinct recovery exponents work, not just
        // consecutive ones starting at zero.
        let gf = Gf16::new();
        let bases = gf.input_bases(20);
        let exponents: Vec<u32> = vec![
            0, 1, 4, 9, 16, 25, 36, 49, 64, 81, 100, 121, 144, 169, 196, 225, 256, 289, 324, 361,
        ];
        let a = Gf16Matrix::build_reduced(&gf, &bases, &exponents);
        let inv = a.invert(&gf).expect("RS submatrix must be invertible");
        let product = mat_mul(&gf, &a, &inv);
        assert_eq!(product, Gf16Matrix::identity(20));
    }

    #[test]
    fn a_genuinely_singular_matrix_returns_an_error_not_a_panic() {
        let gf = Gf16::new();
        // Two identical rows -> singular.
        let mut m = Gf16Matrix::zero(3);
        m.set(0, 0, 1);
        m.set(0, 1, 2);
        m.set(0, 2, 3);
        m.set(1, 0, 1);
        m.set(1, 1, 2);
        m.set(1, 2, 3);
        m.set(2, 0, 5);
        m.set(2, 1, 6);
        m.set(2, 2, 7);
        assert_eq!(m.invert(&gf), Err(SingularMatrix));
    }

    #[test]
    fn an_all_zero_matrix_is_singular() {
        let gf = Gf16::new();
        let m = Gf16Matrix::zero(4);
        assert_eq!(m.invert(&gf), Err(SingularMatrix));
    }

    #[test]
    #[should_panic(expected = "must be square")]
    fn build_reduced_panics_on_mismatched_lengths() {
        let gf = Gf16::new();
        let bases = gf.input_bases(3);
        let exponents = vec![0u32, 1];
        Gf16Matrix::build_reduced(&gf, &bases, &exponents);
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        /// `m` distinct GF(2^16) exponents in `0..ORDER`, deterministically
        /// derived from `seed` — a cheap stand-in for
        /// `prop::collection::btree_set` with a size that depends on another
        /// strategy's output.
        fn distinct_exponents(seed: u64, m: usize) -> Vec<u32> {
            let mut lcg = seed | 1;
            let mut set = std::collections::BTreeSet::new();
            while set.len() < m {
                lcg = lcg
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1442695040888963407);
                set.insert((lcg >> 32) as u32 % crate::gf16::ORDER);
            }
            set.into_iter().collect()
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(200))]

            /// The MDS property, exercised randomly instead of only at the
            /// fixed sizes/exponents the unit tests above use: any `m`
            /// distinct input blocks and `m` distinct recovery exponents
            /// must yield an invertible reduced matrix.
            #[test]
            fn reduced_matrix_always_inverts(m in 1usize..40, seed in any::<u64>()) {
                let gf = Gf16::new();
                let bases = gf.input_bases(m);
                let exponents = distinct_exponents(seed, m);
                let a = Gf16Matrix::build_reduced(&gf, &bases, &exponents);
                let inv = a.invert(&gf);
                prop_assert!(inv.is_ok(), "singular for m={m}, seed={seed:#x}");
                let product = mat_mul(&gf, &a, &inv.unwrap());
                prop_assert_eq!(product, Gf16Matrix::identity(m));
            }
        }
    }
}
