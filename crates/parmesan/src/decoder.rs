//! Reconstructs missing PAR2 input slices from available recovery blocks.
//!
//! [`RecoveryDecoder`] implements the algorithm described in `ROADMAP.md`
//! Phase 22: subtract every known input slice's contribution from a chosen
//! set of recovery blocks (one per missing slice), invert the resulting
//! reduced matrix, and multiply through to recover the missing data. See
//! [`crate::matrix`] for the matrix algebra and [`crate::gf16_mac`] for the
//! multiply-accumulate primitive both steps share.
//!
//! This decoder is independent of file layout: it works entirely in terms
//! of global input-slice indices (the same indexing `RecoveryEncoder` uses)
//! and asks its caller for bytes via a callback, so it never has to know
//! about paths, `RecoverySet`, or on-disk formats. [`crate::repair`] is
//! where those pieces meet.

use crate::gf16::{input_logbases, Gf16};
use crate::gf16_mac::mac;
use crate::matrix::Gf16Matrix;
use anyhow::{bail, Context, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Reconstructs a set of missing PAR2 input slices.
pub struct RecoveryDecoder {
    gf: Gf16,
    slice_size: usize,
    logbases: Vec<u32>,
    total_input_slices: usize,
    missing: Vec<usize>,
}

impl RecoveryDecoder {
    /// Build a decoder for `total_input_slices` input slices of `slice_size`
    /// bytes each, where the global indices listed in `missing` need
    /// reconstruction.
    ///
    /// # Panics
    ///
    /// Panics if `missing` is empty or any index in it is
    /// `>= total_input_slices`.
    pub fn new(slice_size: usize, total_input_slices: usize, mut missing: Vec<usize>) -> Self {
        assert!(
            !missing.is_empty(),
            "RecoveryDecoder: nothing to reconstruct"
        );
        missing.sort_unstable();
        missing.dedup();
        assert!(
            *missing.last().unwrap() < total_input_slices,
            "RecoveryDecoder: missing index {} is out of range for {total_input_slices} slices",
            missing.last().unwrap()
        );
        Self {
            gf: Gf16::new(),
            slice_size,
            logbases: input_logbases(total_input_slices),
            total_input_slices,
            missing,
        }
    }

    /// Global indices this decoder will reconstruct, ascending.
    pub fn missing(&self) -> &[usize] {
        &self.missing
    }

    /// Reconstruct every missing slice.
    ///
    /// `known_slice(i)` must return the padded, slice-sized bytes of known
    /// (non-missing) global input slice `i`. It is called exactly once per
    /// known slice, in ascending order — a caller reading from disk pays for
    /// one sequential pass over the surviving data, not one pass per missing
    /// slice.
    ///
    /// `recovery_blocks` must hold at least `self.missing().len()` entries;
    /// the lowest-exponent blocks available are used first (an arbitrary but
    /// deterministic choice — any `m` distinct recovery blocks work, per the
    /// MDS property documented on [`Gf16Matrix::build_reduced`]).
    ///
    /// Returns `(global_index, slice_bytes)` pairs, one per entry of
    /// [`missing`](Self::missing), in the same order.
    pub fn reconstruct(
        &self,
        mut known_slice: impl FnMut(usize) -> Result<Vec<u8>>,
        recovery_blocks: &BTreeMap<u32, Vec<u8>>,
    ) -> Result<Vec<(usize, Vec<u8>)>> {
        let m = self.missing.len();
        if recovery_blocks.len() < m {
            bail!(
                "not enough recovery blocks to repair {m} missing slice(s): only {} available",
                recovery_blocks.len()
            );
        }

        let exponents: Vec<u32> = recovery_blocks.keys().take(m).copied().collect();
        let bases: Vec<u16> = self
            .missing
            .iter()
            .map(|&i| self.gf.exp(self.logbases[i]))
            .collect();

        let mut adjusted: Vec<Vec<u8>> = Vec::with_capacity(m);
        for e in &exponents {
            let block = &recovery_blocks[e];
            anyhow::ensure!(
                block.len() == self.slice_size,
                "recovery block (exponent {e}) has the wrong length: expected {}, got {}",
                self.slice_size,
                block.len()
            );
            adjusted.push(block.clone());
        }

        // Subtract each known slice's contribution from every selected
        // recovery block in one pass, so the caller's `known_slice` reads
        // each surviving slice from disk exactly once regardless of how
        // many blocks are being reconstructed.
        let missing_set: BTreeSet<usize> = self.missing.iter().copied().collect();
        for j in 0..self.total_input_slices {
            if missing_set.contains(&j) {
                continue;
            }
            let known_bytes =
                known_slice(j).with_context(|| format!("reading known input slice {j}"))?;
            anyhow::ensure!(
                known_bytes.len() == self.slice_size,
                "known slice {j} has the wrong length: expected {}, got {}",
                self.slice_size,
                known_bytes.len()
            );
            let base_j = self.gf.exp(self.logbases[j]);
            for (row, &e) in exponents.iter().enumerate() {
                let coeff = self.gf.pow(base_j, e);
                mac(&self.gf, &mut adjusted[row], &known_bytes, coeff);
            }
        }

        let a = Gf16Matrix::build_reduced(&self.gf, &bases, &exponents);
        let inv = a
            .invert(&self.gf)
            .map_err(|e| anyhow::anyhow!("{e}"))
            .context(
                "the selected recovery/missing blocks produced a singular matrix \
                 (this indicates a decoder bug, not damaged input data)",
            )?;

        let mut results = Vec::with_capacity(m);
        for (c, &global_index) in self.missing.iter().enumerate() {
            let mut out = vec![0u8; self.slice_size];
            for (r, adjusted_row) in adjusted.iter().enumerate() {
                let coeff = inv.get(c, r);
                if coeff != 0 {
                    mac(&self.gf, &mut out, adjusted_row, coeff);
                }
            }
            results.push((global_index, out));
        }

        Ok(results)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encoder::RecoveryEncoder;

    fn make_slice(slice_size: usize, seed: u8) -> Vec<u8> {
        (0..slice_size)
            .map(|i| seed.wrapping_add(i as u8))
            .collect()
    }

    fn encode(
        slices: &[Vec<u8>],
        slice_size: usize,
        recovery_count: usize,
    ) -> BTreeMap<u32, Vec<u8>> {
        let mut enc = RecoveryEncoder::new(slice_size, slices.len(), 0, recovery_count);
        for s in slices {
            enc.add_slice(s.clone());
        }
        let (recovery_slices, _checksums) = enc.finish();
        recovery_slices
            .into_iter()
            .map(|s| (s.exponent, s.data))
            .collect()
    }

    #[test]
    fn reconstructs_a_single_missing_slice() {
        let slice_size = 64;
        let n = 5;
        let slices: Vec<Vec<u8>> = (0..n)
            .map(|i| make_slice(slice_size, (i as u8).wrapping_mul(17).wrapping_add(3)))
            .collect();
        let recovery_blocks = encode(&slices, slice_size, n);

        let missing_index = 2;
        let dec = RecoveryDecoder::new(slice_size, n, vec![missing_index]);
        let result = dec
            .reconstruct(|j| Ok(slices[j].clone()), &recovery_blocks)
            .unwrap();

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].0, missing_index);
        assert_eq!(result[0].1, slices[missing_index]);
    }

    #[test]
    fn reconstructs_multiple_missing_slices_using_exactly_enough_recovery_blocks() {
        let slice_size = 64;
        let n = 10;
        let slices: Vec<Vec<u8>> = (0..n)
            .map(|i| make_slice(slice_size, (i as u8).wrapping_mul(31).wrapping_add(7)))
            .collect();
        let recovery_count = 4;
        let recovery_blocks = encode(&slices, slice_size, recovery_count);

        let missing = vec![1usize, 4, 7, 9];
        let dec = RecoveryDecoder::new(slice_size, n, missing.clone());
        let result = dec
            .reconstruct(
                |j| {
                    assert!(
                        !missing.contains(&j),
                        "decoder must never ask for a missing slice's bytes"
                    );
                    Ok(slices[j].clone())
                },
                &recovery_blocks,
            )
            .unwrap();

        let got: BTreeMap<usize, Vec<u8>> = result.into_iter().collect();
        for &idx in &missing {
            assert_eq!(got[&idx], slices[idx], "slice {idx} mismatch");
        }
    }

    #[test]
    fn reconstructs_correctly_with_surplus_recovery_blocks() {
        // More recovery blocks are available than are strictly needed; the
        // decoder should still pick exactly `m` of them and succeed.
        let slice_size = 32;
        let n = 8;
        let slices: Vec<Vec<u8>> = (0..n)
            .map(|i| make_slice(slice_size, (i as u8).wrapping_mul(5).wrapping_add(1)))
            .collect();
        let recovery_blocks = encode(&slices, slice_size, 6); // n=8 missing=1, way more than needed

        let missing_index = 3;
        let dec = RecoveryDecoder::new(slice_size, n, vec![missing_index]);
        let result = dec
            .reconstruct(|j| Ok(slices[j].clone()), &recovery_blocks)
            .unwrap();
        assert_eq!(result[0].1, slices[missing_index]);
    }

    #[test]
    fn errors_cleanly_when_not_enough_recovery_blocks() {
        let slice_size = 64;
        let n = 5;
        let slices: Vec<Vec<u8>> = (0..n).map(|i| make_slice(slice_size, i as u8)).collect();
        let recovery_blocks = encode(&slices, slice_size, 1);

        let dec = RecoveryDecoder::new(slice_size, n, vec![0, 1]);
        let result = dec.reconstruct(|j| Ok(slices[j].clone()), &recovery_blocks);
        assert!(result.is_err());
    }

    #[test]
    fn reconstructing_every_slice_at_once_still_works() {
        let slice_size = 32;
        let n = 6;
        let slices: Vec<Vec<u8>> = (0..n)
            .map(|i| make_slice(slice_size, (i as u8).wrapping_mul(41).wrapping_add(2)))
            .collect();
        let recovery_blocks = encode(&slices, slice_size, n);

        let missing: Vec<usize> = (0..n).collect();
        let dec = RecoveryDecoder::new(slice_size, n, missing.clone());
        let result = dec
            .reconstruct(
                |_| unreachable!("no known slices exist in this test"),
                &recovery_blocks,
            )
            .unwrap();

        let got: BTreeMap<usize, Vec<u8>> = result.into_iter().collect();
        for i in 0..n {
            assert_eq!(got[&i], slices[i]);
        }
    }
}
