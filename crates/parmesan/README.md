# parmesan

Fast, standalone PAR2 (Reed-Solomon) creation tool.

`parmesan` encodes input files into PAR2 recovery archives using a
multi-threaded Reed-Solomon implementation with SIMD acceleration (Scalar,
SSSE3, AVX2, AVX2+GFNI, AVX-512+GFNI, ARM NEON).

It is designed to be used both as a CLI binary and as a library embedded in
[pesto](https://github.com/franzopl/pesto), a fast Usenet poster.

## Usage (CLI)

```
parmesan [OPTIONS] <FILES>...
```

| Flag | Default | Description |
|------|---------|-------------|
| `-r`, `--recovery-pct` | `10` | Percentage of recovery data to generate |
| `-s`, `--slice-size` | auto | Manual PAR2 slice size, e.g. `"1 MiB"` |
| `-n`, `--num-slices` | auto | Target number of input slices |
| `--simd` | `auto` | SIMD path: `auto`, `scalar`, `ssse3`, `avx2`, `avx2-gfni`, `avx512-gfni`, `neon` |

## Building

```bash
cargo build --release -p parmesan
```

The optimised binary lands at `target/release/parmesan`.

## License

MIT
