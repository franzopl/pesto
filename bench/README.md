# pesto benchmarks

Portable benchmark suite for comparing `pesto` and `parmesan` against
established Usenet tools. Each script is self-contained: it builds what it
needs, generates sparse test files, runs the comparison, and prints a
copy-paste–ready Markdown table.

---

## Quick start

```bash
# Build release binaries first
cargo build --release

# yEnc encoding throughput (default: 100 MB file)
./bench/yenc.sh

# PAR2 creation throughput (default: 1 GB and 5 GB files)
./bench/par2.sh

# Custom sizes
./bench/yenc.sh 50 200 500
./bench/par2.sh 1 5 10
```

Results are saved as CSV files in `bench/results/` named after your hostname
and the current date, so runs from different machines can be collected and
compared.

---

## Scripts

| Script | What it measures | Compares against |
|--------|-----------------|-----------------|
| `yenc.sh` | yEnc encoding throughput (MB/s) | `node-yencode` |
| `par2.sh` | PAR2 creation throughput (MB/s) | `parpar`, `par2cmdline` |

### `bench/yenc.sh`

Runs the `yenc-bench` Rust example (built from `examples/yenc-bench.rs`) on
sparse test files. If `node` and the `bench_yencode.js` script are present,
it also runs `node-yencode` for comparison.

**Requirements:**
- `cargo build --release` (builds `target/release/examples/yenc-bench`)
- `node` + `yencode` npm package (optional; comparison skipped if absent)

**What it prints:**

```
BENCHMARK: yEnc encoding throughput
  CPU   : Intel Core i7-10700K @ 3.80GHz
  Cores : 16 logical
  SIMD  : ssse3 avx2
  Date  : 2026-05-24 14:30 UTC

  Line length : 128 bytes
  Sizes       : 100 MB
────────────────────────────────────────────────────────────────────────
── 100 MB ──
  pesto             0s      2143.7 MB/s  (2.09 GB/s)
  node-yencode      0s       312.1 MB/s  (0.30 GB/s)

SUMMARY
| Size   | pesto (MB/s) | node-yencode (MB/s) | speedup |
|--------|-------------|---------------------|---------|
| 100 MB | 2143.7      | 312.1               | +586.8% |
```

### `bench/par2.sh`

Runs `parmesan` (the PAR2 engine inside pesto) and optionally `parpar` and
`par2cmdline` on sparse test files at 10% recovery redundancy.

**Requirements:**
- `cargo build --release` (builds `target/release/parmesan`)
- `parpar` in PATH (optional)
- `par2` in PATH (optional)

---

## Adding results to the README

After running on a machine, copy the printed Markdown table and add it to
the "Performance" section of the root `README.md`. Include:
- CPU model and core count
- OS / kernel version
- Which tools were compared (list versions with `parpar --version`, `par2 --version`)

---

## Shared library

`bench/lib.sh` provides helpers used by all scripts:

- Terminal formatting: `bold`, `green`, `red`, `hr`
- System info: `cpu_model`, `cpu_cores`, `simd_flags`, `print_system_info`
- Metrics: `throughput_mbps`, `throughput_gbps`, `ms_to_str`, `speedup_pct`
- I/O: `ensure_sparse_file`, `drop_caches`, `file_size_bytes`
- Output: `csv_header`, `csv_row`, `md_header`, `md_row`

---

## Notes

- Test files are created as **sparse files** (`truncate -s`), so they take no
  real disk space. Reads will return zero bytes — this is intentional: it
  isolates encoder CPU throughput from disk I/O variance.
- `drop_caches` is called before each run on Linux. It requires `sudo`; if
  denied, the script continues without dropping caches (noted in output).
- Raw CSV results in `bench/results/` are gitignored. To share results, paste
  the printed Markdown table directly into a forum post or issue.
