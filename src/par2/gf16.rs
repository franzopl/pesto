//! GF(2^16) arithmetic and the PAR2 Reed-Solomon matrix.
//!
//! PAR2 computes recovery data over the Galois field GF(2^16) with the
//! primitive polynomial `0x1100B` (x^16 + x^12 + x^3 + x + 1) and the
//! primitive element 2. Every detail here is matched bit-for-bit with
//! `par2cmdline` so the output is repairable by standard tools.
//!
//! ## The Reed-Solomon matrix
//!
//! Each recovery block has an integer **exponent** (0, 1, 2, …). Each input
//! block `i` is assigned a **base** constant `antilog(logbase_i)`, where
//! `logbase_i` is the i-th non-negative integer coprime with 65535 — the
//! multiplicative group order. Since `65535 = 3·5·17·257`, "coprime with
//! 65535" means "not divisible by 3, 5, 17 or 257". The matrix entry for
//! input block `i` and recovery exponent `e` is `base_i` raised to `e`.

/// Primitive polynomial of the field (x^16 + x^12 + x^3 + x + 1).
const POLYNOMIAL: u32 = 0x1_100B;

/// Order of the multiplicative group: `2^16 - 1`.
pub const ORDER: u32 = 65_535;

/// Maximum number of input blocks: the count of integers coprime with 65535,
/// i.e. `φ(65535) = φ(3)·φ(5)·φ(17)·φ(257) = 2·4·16·256`.
pub const MAX_INPUT_BLOCKS: usize = 32_768;

/// Maximum number of recovery blocks par2cmdline supports.
pub const MAX_RECOVERY_BLOCKS: usize = 65_535;

/// Greatest common divisor.
fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = b;
        b = a % b;
        a = t;
    }
    a
}

/// The `logbase` exponents assigned to the first `count` input blocks: the
/// non-negative integers coprime with 65535, in order.
///
/// # Panics
///
/// Panics if `count` exceeds [`MAX_INPUT_BLOCKS`].
pub fn input_logbases(count: usize) -> Vec<u32> {
    assert!(
        count <= MAX_INPUT_BLOCKS,
        "PAR2 supports at most {MAX_INPUT_BLOCKS} input blocks, got {count}"
    );
    let mut out = Vec::with_capacity(count);
    let mut logbase = 0u32;
    while out.len() < count {
        if gcd(ORDER, logbase) == 1 {
            out.push(logbase);
        }
        logbase += 1;
    }
    out
}

/// The GF(2^16) field: the antilog (`2^i`) and log lookup tables.
#[derive(Debug, Clone)]
pub struct Gf16 {
    /// `antilog[i] = 2^i` for `i` in `0..65535`.
    antilog: Vec<u16>,
    /// `log[v]` is the exponent `i` such that `antilog[i] == v`; `log[0]`
    /// is unused.
    log: Vec<u16>,
}

impl Gf16 {
    /// Build the field lookup tables.
    pub fn new() -> Self {
        let mut antilog = vec![0u16; ORDER as usize];
        let mut log = vec![0u16; ORDER as usize + 1];
        let mut value: u32 = 1;
        for (i, slot) in antilog.iter_mut().enumerate() {
            *slot = value as u16;
            log[value as usize] = i as u16;
            value <<= 1;
            if value & 0x1_0000 != 0 {
                value ^= POLYNOMIAL;
            }
        }
        Self { antilog, log }
    }

    /// Multiply two field elements.
    pub fn mul(&self, a: u16, b: u16) -> u16 {
        if a == 0 || b == 0 {
            return 0;
        }
        let exponent = self.log[a as usize] as u32 + self.log[b as usize] as u32;
        self.antilog[(exponent % ORDER) as usize]
    }

    /// Raise a field element to an integer power.
    pub fn pow(&self, base: u16, exponent: u32) -> u16 {
        if base == 0 {
            return u16::from(exponent == 0);
        }
        let log = self.log[base as usize] as u64;
        self.antilog[((log * exponent as u64) % ORDER as u64) as usize]
    }

    /// `2^exponent` in the field, with the exponent reduced modulo the group
    /// order. Note `discrete_log(exp(e)) == e % ORDER`.
    pub fn exp(&self, exponent: u32) -> u16 {
        self.antilog[(exponent % ORDER) as usize]
    }

    /// Discrete logarithm: the exponent `e` such that `exp(e) == value`.
    ///
    /// # Panics
    ///
    /// Panics if `value` is zero.
    pub fn discrete_log(&self, value: u16) -> u16 {
        assert!(value != 0, "zero has no discrete logarithm");
        self.log[value as usize]
    }

    /// The base constants assigned to the first `count` input blocks.
    pub fn input_bases(&self, count: usize) -> Vec<u16> {
        input_logbases(count)
            .into_iter()
            .map(|logbase| self.antilog[logbase as usize])
            .collect()
    }

    /// The Reed-Solomon coefficient applied to input block `input_index` when
    /// forming the recovery block with the given `exponent`.
    pub fn recovery_coefficient(&self, input_index: usize, exponent: u32) -> u16 {
        let logbase = input_logbases(input_index + 1)[input_index] as u64;
        self.antilog[((logbase * exponent as u64) % ORDER as u64) as usize]
    }
}

impl Default for Gf16 {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_sizes_and_first_powers_of_two() {
        let gf = Gf16::new();
        assert_eq!(gf.antilog.len(), 65_535);
        assert_eq!(gf.log.len(), 65_536);
        assert_eq!(gf.antilog[0], 1);
        assert_eq!(gf.antilog[1], 2);
        assert_eq!(gf.antilog[2], 4);
        // 2^16 wraps via the polynomial: 0x10000 ^ 0x1100B == 0x100B.
        assert_eq!(gf.antilog[16], 0x100B);
    }

    #[test]
    fn log_and_antilog_are_inverses() {
        let gf = Gf16::new();
        for exponent in 0..65_535u32 {
            let value = gf.antilog[exponent as usize];
            assert_eq!(gf.log[value as usize] as u32, exponent);
        }
        for value in 1..=65_535u32 {
            let exponent = gf.log[value as usize];
            assert_eq!(gf.antilog[exponent as usize] as u32, value);
        }
    }

    #[test]
    fn multiplication_basics_and_log_homomorphism() {
        let gf = Gf16::new();
        assert_eq!(gf.mul(0, 1234), 0);
        assert_eq!(gf.mul(1, 1234), 1234);
        assert_eq!(gf.mul(2, 2), 4);
        // mul(2^i, 2^j) == 2^((i + j) mod 65535)
        for &(i, j) in &[(3u32, 5u32), (100, 200), (60_000, 60_000)] {
            let product = gf.mul(gf.antilog[i as usize], gf.antilog[j as usize]);
            assert_eq!(product, gf.antilog[((i + j) % 65_535) as usize]);
        }
    }

    #[test]
    fn pow_matches_repeated_multiplication() {
        let gf = Gf16::new();
        assert_eq!(gf.pow(7, 0), 1);
        assert_eq!(gf.pow(7, 1), 7);
        for base in [2u16, 7, 12_345, 60_000] {
            let mut acc = 1u16;
            for exponent in 0..20u32 {
                assert_eq!(gf.pow(base, exponent), acc);
                acc = gf.mul(acc, base);
            }
        }
    }

    #[test]
    fn input_logbases_skip_factors_of_65535() {
        // 65535 = 3 * 5 * 17 * 257.
        let logbases = input_logbases(64);
        assert_eq!(&logbases[..4], &[1, 2, 4, 7]);
        for &logbase in &logbases {
            assert_ne!(logbase % 3, 0);
            assert_ne!(logbase % 5, 0);
            assert_ne!(logbase % 17, 0);
            assert_ne!(logbase % 257, 0);
        }
        // The sequence is strictly increasing.
        assert!(logbases.windows(2).all(|w| w[0] < w[1]));
    }

    #[test]
    fn input_bases_are_antilogs_of_the_logbases() {
        let gf = Gf16::new();
        // logbases 1, 2, 4, 7 -> antilog 2, 4, 16, 128.
        assert_eq!(gf.input_bases(4), vec![2, 4, 16, 128]);
    }

    #[test]
    fn recovery_coefficient_exponent_zero_is_one() {
        let gf = Gf16::new();
        for input_index in [0usize, 1, 5, 100] {
            assert_eq!(gf.recovery_coefficient(input_index, 0), 1);
        }
        // Exponent 1 yields the input block's own base constant.
        assert_eq!(gf.recovery_coefficient(0, 1), 2);
        assert_eq!(gf.recovery_coefficient(1, 1), 4);
        // base_0 = 2, so exponent 2 gives 2^2 = 4.
        assert_eq!(gf.recovery_coefficient(0, 2), 4);
    }

    #[test]
    fn full_multiplicative_group_is_covered() {
        // Every non-zero value appears exactly once in the antilog table.
        let gf = Gf16::new();
        let mut seen = vec![false; 65_536];
        for &value in &gf.antilog {
            assert!(!seen[value as usize], "duplicate value {value}");
            assert_ne!(value, 0);
            seen[value as usize] = true;
        }
    }
}
