//! Criterion benchmarks for the decode (repair) path.
//!
//! Run with:
//!
//! ```text
//! cargo bench -p parmesan-par2
//! ```
//!
//! Formalizes the throughput claims in `ROADMAP.md`/`CHANGELOG.md` (which
//! were first measured with an ad hoc `#[ignore]`d timing test in
//! `gf16_mac.rs`) and covers the specific question `ROADMAP.md` Phase 22h
//! calls out: how matrix inversion cost scales with the number of missing
//! blocks, to calibrate the point where algebra cost starts to dominate the
//! multiply-accumulate cost.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use parmesan::decoder::RecoveryDecoder;
use parmesan::encoder::RecoveryEncoder;
use parmesan::gf16::Gf16;
use parmesan::gf16_mac::mac;
use parmesan::matrix::Gf16Matrix;
use std::collections::BTreeMap;
use std::hint::black_box;

fn bench_mac(c: &mut Criterion) {
    let gf = Gf16::new();
    let mut group = c.benchmark_group("gf16_mac_mac");
    for &len in &[64 * 1024usize, 1024 * 1024] {
        group.throughput(Throughput::Bytes(len as u64));
        let src = vec![0xABu8; len];
        let mut dst = vec![0u8; len];
        group.bench_with_input(BenchmarkId::from_parameter(len), &len, |b, _| {
            b.iter(|| {
                mac(&gf, black_box(&mut dst), black_box(&src), black_box(0x1234));
            });
        });
    }
    group.finish();
}

fn bench_matrix_invert(c: &mut Criterion) {
    let gf = Gf16::new();
    let mut group = c.benchmark_group("matrix_invert");
    group.sample_size(20);
    // Per ROADMAP.md Phase 22h: calibrate the practical limit before
    // algebra cost (O(m^3), naive Gauss-Jordan) dominates the MAC cost
    // (O(m) per missing block, linear in slice size). Capped at 1000 rather
    // than the aspirational 5000 in that roadmap note: m=5000 means ~1.25e11
    // field multiplications, tens of minutes per single `invert()` call —
    // itself a useful data point about where this naive implementation
    // stops being practical, and exactly the kind of number that motivates
    // the "incremental Gauss-Jordan" optimisation `decoder.rs` defers.
    for &m in &[10usize, 50, 100, 250, 500, 1000] {
        group.bench_with_input(BenchmarkId::from_parameter(m), &m, |b, &m| {
            let bases = gf.input_bases(m);
            let exponents: Vec<u32> = (0..m as u32).collect();
            b.iter(|| {
                let a = Gf16Matrix::build_reduced(&gf, black_box(&bases), black_box(&exponents));
                black_box(a.invert(&gf).unwrap());
            });
        });
    }
    group.finish();
}

fn bench_decoder_reconstruct(c: &mut Criterion) {
    let slice_size = 64 * 1024; // 64 KiB, a realistic PAR2 slice size
    let total_input_slices = 600;
    let mut group = c.benchmark_group("decoder_reconstruct");
    group.sample_size(30);

    for &missing_count in &[1usize, 10, 50, 100] {
        let slices: Vec<Vec<u8>> = (0..total_input_slices)
            .map(|i| vec![(i % 256) as u8; slice_size])
            .collect();

        let mut enc =
            RecoveryEncoder::new(slice_size, total_input_slices, 0, missing_count).with_checksums();
        for s in &slices {
            enc.add_slice(s.clone());
        }
        let (recovery_slices, _checksums) = enc.finish();
        let recovery_blocks: BTreeMap<u32, Vec<u8>> = recovery_slices
            .into_iter()
            .map(|s| (s.exponent, s.data))
            .collect();

        let missing: Vec<usize> = (0..missing_count).collect();

        group.throughput(Throughput::Bytes((slice_size * missing_count) as u64));
        group.bench_with_input(
            BenchmarkId::from_parameter(missing_count),
            &missing_count,
            |b, _| {
                b.iter(|| {
                    let dec = RecoveryDecoder::new(slice_size, total_input_slices, missing.clone());
                    let result = dec
                        .reconstruct(|j| Ok(slices[j].clone()), black_box(&recovery_blocks))
                        .unwrap();
                    black_box(result);
                });
            },
        );
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_mac,
    bench_matrix_invert,
    bench_decoder_reconstruct
);
criterion_main!(benches);
