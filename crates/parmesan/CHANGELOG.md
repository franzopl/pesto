# Changelog — parmesan

All notable changes to `parmesan` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased]

## [0.1.0] — 2026-05-23

### Added
- Multi-threaded Reed-Solomon PAR2 encoder with SIMD dispatch:
  - Scalar (pure Rust, no SIMD)
  - SSSE3 (128-bit shuffles, x86/x86_64)
  - AVX2 (256-bit shuffles, x86_64)
  - AVX2+GFNI (Intel Ice Lake+)
  - AVX-512+GFNI (Intel Ice Lake+, production-enabled after Ice Lake Xeon
    validation on AWS m6i)
  - ARM NEON (AArch64)
- Automatic SIMD path selection at runtime (`--simd auto`); override via
  `--simd <path>` flag.
- PAR2 geometry calculation: auto slice-size from file set, configurable via
  `--slice-size` and `--num-slices`.
- Full PAR2 v2 packet generation: `FileDesc`, `IFSC`, `Main`, `RecvSlic`,
  `Creator`.
- `walkdir`-based directory ingestion: pass files or whole directories.
- Progress bar via `indicatif`.
- `tracing` + `tracing-subscriber` logging with `RUST_LOG` override.
- Library API (`lib.rs`) exposing encoder, layout, ops and packet modules for
  embedding in `pesto`.
- `bench-internals` feature to expose per-path flush functions for
  micro-benchmarking.
- `par2-avx2-gfni-unsafe` feature to expose AVX2+GFNI and AVX-512+GFNI paths
  for explicit testing.
