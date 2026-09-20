use super::super::*;

#[test]
fn file_hasher_16k_equals_full_for_small_files() {
    let mut hasher = FileHasher::new();
    hasher.update(b"hello ");
    hasher.update(b"world");
    let hashes = hasher.finish();
    assert_eq!(hashes.length, 11);
    assert_eq!(hashes.md5_full, crate::packet::md5(b"hello world"));
    assert_eq!(hashes.md5_16k, crate::packet::md5(b"hello world"));
}

#[test]
fn file_hasher_16k_covers_only_the_first_16k() {
    let data = vec![0x5Au8; HEAD_LEN + 5000];
    let mut hasher = FileHasher::new();
    hasher.update(&data[..10_000]);
    hasher.update(&data[10_000..]);
    let hashes = hasher.finish();
    assert_eq!(hashes.length as usize, data.len());
    assert_eq!(hashes.md5_full, crate::packet::md5(&data));
    assert_eq!(hashes.md5_16k, crate::packet::md5(&data[..HEAD_LEN]));
}

#[test]
fn slice_checksum_matches_md5_and_crc32() {
    let slice = [1u8, 2, 3, 4, 5, 6, 7, 8];
    let checksum = slice_checksum(&slice);
    assert_eq!(checksum.md5, crate::packet::md5(&slice));
    assert_eq!(checksum.crc32, crate::yenc::crc32(&slice));
}

#[test]
#[cfg(target_arch = "x86_64")]
fn batched_slice_checksums_match_individual_checksums() {
    let slices: Vec<Vec<u8>> = (0..17)
        .map(|i| {
            (0..4096)
                .map(|offset| (offset as u8).wrapping_mul(31).wrapping_add(i))
                .collect()
        })
        .collect();
    let expected: Vec<_> = slices.iter().map(|slice| slice_checksum(slice)).collect();

    let actual = slice_checksums_batch(&slices);
    for (actual, expected) in actual.iter().zip(&expected) {
        assert_eq!(actual.md5, expected.md5);
        assert_eq!(actual.crc32, expected.crc32);
    }
}

#[test]
#[cfg(target_arch = "x86_64")]
fn dep_tables_correctness_and_timing() {
    use std::time::Instant;

    let t0 = Instant::now();
    let enc = RecoveryEncoder::new(4, 1, 0, 1);
    let elapsed = t0.elapsed();

    let Some(ref tables) = enc.dep_tables else {
        // GFNI hardware or non-AVX2: table is not allocated; skip.
        return;
    };

    // index 0 must be all-zero (multiply by 0 always yields 0).
    assert_eq!(tables[0], [0u16; 16]);

    // index 1 must be the identity (multiply by 1 is a no-op).
    let identity: [u16; 16] = std::array::from_fn(|k| 1 << k);
    assert_eq!(tables[1], identity);

    // Spot-check: table[n] must equal xor_dep_matrix(n) for representative n.
    for &n in &[2u16, 3, 7, 256, 1000, 0x1234, 0xABCD, 65534] {
        assert_eq!(
            tables[n as usize],
            xor_dep_matrix(n),
            "dep_tables mismatch at n={n}"
        );
    }

    // Release target: < 5 ms on i5-10400. Debug builds are much slower due
    // to the absence of optimizations; allow up to 5 s there.
    let limit_ms = if cfg!(debug_assertions) { 5_000 } else { 50 };
    assert!(
        elapsed.as_millis() < limit_ms,
        "dep_tables construction took {}ms, expected < {limit_ms}ms",
        elapsed.as_millis()
    );
}
