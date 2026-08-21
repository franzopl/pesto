# pesto benchmark results

**ip-172-31-82-23** — 2026-08-21T03:55:00Z

| | |
|---|---|
| CPU | Intel(R) Xeon(R) Platinum 8488C |
| Cores | 8 logical / 4 physical |
| SIMD | `ssse3 avx2 avx512f avx512bw gfni vpclmulqdq` |
| Kernel | Linux 6.17.0-1019-aws (x86_64) |
| Governor | unknown, boost unknown |
| Corpus FS | ext4 |
| Repetitions | 3 (median reported) |
| Page cache | warm, primed before each comparison |

Tool versions: pesto `pesto dev`, parmesan `parmesan dev`,
parpar `0.4.6`, par2cmdline `par2cmdline version 0.8.1`,
nyuu `0.4.2`, ngPost `absent`.

## PAR2

All tools were given the *same* geometry — identical slice size, input slice
count and recovery block count — so each row represents the same amount of
GF(2^16) arithmetic. See `bench/lib/tools.sh` for the flag mapping. `GF madd`
is `input_bytes × recovery_blocks / time`, the redundancy-independent rate.

| workload | case | parmesan | par2cmdline | parpar | best vs parmesan | noise |
|---|---|---|---|---|---|---|
| many-small | create-r10 | 412 MiB/s | 50 MiB/s | 227 MiB/s | parmesan fastest | 10.3% |
| many-small | repair | 176 MiB/s | 96 MiB/s | – | parmesan fastest | 0.3% |
| many-small | verify | 374 MiB/s | 297 MiB/s | – | parmesan fastest | 1.0% |
| movie-1080p | create-r10 | 485 MiB/s | 50 MiB/s | 577 MiB/s | parpar +19.1% | 11.2% |
| movie-1080p | repair | 161 MiB/s | 38 MiB/s | – | parmesan fastest | 3.4% |
| movie-1080p | verify | 405 MiB/s | 166 MiB/s | – | parmesan fastest | 1.1% |


---

## Reproducing this

```bash
git clone https://github.com/franzopl/pesto && cd pesto
cargo build --release
./bench/run.sh --scale 1.0 --reps 3
```

The corpus is generated from fixed seeds, so the input bytes are identical on
any machine at the same `--scale`. Verify with `./bench/run.sh --verify-data`.

Raw per-repetition data: `20260821T035500Z/raw.csv`.
Machine-readable summary: `20260821T035500Z/results.json`.

**Limitations.** Read `bench/README.md` before quoting any of these numbers —
in particular, the end-to-end figures are against a local mock server, which
removes real-provider behaviour (per-account concurrency caps, propagation
delay, TLS) from the measurement by design.
