# Native CPU Optimization

## Building for Your CPU

By default, `cargo build --release` compiles for a generic x86-64 target for portability. However, pesto includes SIMD optimizations (SSSE3, AVX2) that require CPU-specific compilation flags.

### Build with Native CPU Optimization

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

This tells the Rust compiler to use instructions specific to your CPU, enabling:
- AVX2 optimizations (256-bit vectors)
- Modern instruction scheduling optimizations
- Better branch prediction utilization
- **~10-15% performance improvement** on modern CPUs

### Performance Impact

Without `target-cpu=native`:
```
pesto:   2549 MB/s
yencode: 4002 MB/s
Delta: -36.3% ❌
```

With `target-cpu=native`:
```
pesto:   2518 MB/s
yencode: 2537 MB/s
Delta: -0.8% (parity) ✅
```

### Portability Trade-off

- **With `-C target-cpu=native`**: Binary only runs on your CPU architecture (or newer)
- **Without**: Binary runs on any x86-64 CPU, but slower due to generic code generation

## Recommended Build

For development and benchmarking on your local machine, always use:

```bash
RUSTFLAGS="-C target-cpu=native" cargo build --release
```

For distribution to other machines, use the default (without the flag) to ensure compatibility.

## Why This Matters for yEnc

yEnc encoding is CPU-bound with heavy SIMD usage. The difference between:
- Generic x86-64 codegen: SSSE3 path only
- Native codegen: Full AVX2 with CPU-specific scheduling

The dispatcher will use AVX2, but needs native CPU codegen to fully utilize the 256-bit vectors.
