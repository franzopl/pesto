# pesto benchmark results

**medialab** — 2026-08-21T10:47:26Z

| | |
|---|---|
| CPU | Intel(R) Core(TM) i5-10400 CPU @ 2.90GHz |
| Cores | 12 logical / 6 physical |
| SIMD | `ssse3 avx2` |
| Kernel | Linux 6.8.0-111-generic (x86_64) |
| Governor | performance, boost on |
| Corpus FS | btrfs |
| Repetitions | 3 (median reported) |
| Page cache | warm, primed before each comparison |

Tool versions: pesto `pesto dev`, parmesan `parmesan dev`,
parpar `0.4.5`, par2cmdline `par2cmdline version 0.8.1`,
nyuu `0.4.2`, ngPost `v4.16`.

## PAR2

All tools were given the *same* geometry — identical slice size, input slice
count and recovery block count — so each row represents the same amount of
GF(2^16) arithmetic. See `bench/lib/tools.sh` for the flag mapping. `GF madd`
is `input_bytes × recovery_blocks / time`, the redundancy-independent rate.

| workload | case | parmesan | par2cmdline | parpar | best vs parmesan | noise |
|---|---|---|---|---|---|---|
| many-small | create-r10 | 308 MiB/s | 34 MiB/s | 257 MiB/s | parmesan fastest | 4.0% |
| many-small | repair | 104 MiB/s | 68 MiB/s | – | parmesan fastest | 2.6% |
| many-small | verify | 446 MiB/s | 343 MiB/s | – | parmesan fastest | 0.6% |
| movie-1080p | create-r10 | 308 MiB/s | 34 MiB/s | 371 MiB/s | parpar +20.4% | 4.9% |
| movie-1080p | repair | 426 MiB/s | 174 MiB/s | – | parmesan fastest | 1.7% |
| movie-1080p | verify | 460 MiB/s | 181 MiB/s | – | parmesan fastest | 0.9% |

## End-to-end uploads

Posted to the local mock NNTP server, so no network variance and no account.
`latency` is the artificial per-response delay the mock adds: 0 ms measures
encode-and-write throughput, 30 ms measures pipelining under a realistic
round-trip time.

Cases:

- `post-only-*` — no PAR2 anywhere. A pure poster comparison at identical
  article size, line length, connection count and group.
- `full-streaming-*` — pesto's default pipeline, recovery generated while the
  data articles are already going out. No competitor has this shape, so the
  row has one cell by design.
- `full-two-phase-*` — generate everything, then post: `pesto
  --par2-before-upload` against `parpar + nyuu` (both phases timed as one) and
  `ngPost --gen_par2`. This is the like-for-like comparison.

Article counts are cross-checked from each tool's own NZB. On `post-only`
rows they must match, and a ⚠ means the row is not comparable. On
`full-*` rows a difference is expected: implementations split recovery data
into volumes differently, so the same recovery block count lands in a
different number of files and articles. There the flag is a reminder that the
rows carry the same *payload*, not the same article count.

| workload | case | pesto | ngPost | nyuu | parpar+nyuu | best vs pesto | noise |
|---|---|---|---|---|---|---|---|
| many-small | full-streaming-l0 | 255 MiB/s | – | – | – | pesto fastest | 1.1% |
| many-small | full-streaming-l30 | 30 MiB/s | – | – | – | pesto fastest | 0.0% |
| many-small | full-two-phase-l0 | 244 MiB/s | 23 MiB/s | – | 161 MiB/s | pesto fastest | 5.9% ⚠ article counts differ |
| many-small | full-two-phase-l30 | 27 MiB/s | 14 MiB/s | – | 27 MiB/s | pesto fastest | 0.6% ⚠ article counts differ ⚠ 1 failed rep(s) |
| many-small | post-only-l0 | 1761 MiB/s | 657 MiB/s | 506 MiB/s | – | pesto fastest | 3.5% |
| many-small | post-only-l30 | 32 MiB/s | 31 MiB/s | 31 MiB/s | – | pesto fastest | 0.2% ⚠ 1 failed rep(s) |
| movie-1080p | full-streaming-l0 | 261 MiB/s | – | – | – | pesto fastest | 4.2% |
| movie-1080p | full-streaming-l30 | 83 MiB/s | – | – | – | pesto fastest | 0.2% |
| movie-1080p | full-two-phase-l0 | 266 MiB/s | 34 MiB/s | – | 283 MiB/s | parpar+nyuu +6.7% | 4.4% ⚠ article counts differ |
| movie-1080p | full-two-phase-l30 | 67 MiB/s | 25 MiB/s | – | 68 MiB/s | parpar+nyuu +2.1% | 0.8% ⚠ article counts differ |
| movie-1080p | post-only-l0 | 2226 MiB/s | 944 MiB/s | 1339 MiB/s | – | pesto fastest | 2.2% ⚠ article counts differ |
| movie-1080p | post-only-l30 | 92 MiB/s | 92 MiB/s | 92 MiB/s | – | pesto fastest | 0.0% ⚠ article counts differ |


---

## Reproducing this

```bash
git clone https://github.com/franzopl/pesto && cd pesto
cargo build --release
./bench/run.sh --scale 1.0 --reps 3
```

The corpus is generated from fixed seeds, so the input bytes are identical on
any machine at the same `--scale`. Verify with `./bench/run.sh --verify-data`.

Raw per-repetition data: `20260821T104726Z/raw.csv`.
Machine-readable summary: `20260821T104726Z/results.json`.

**Limitations.** Read `bench/README.md` before quoting any of these numbers —
in particular, the end-to-end figures are against a local mock server, which
removes real-provider behaviour (per-account concurrency caps, propagation
delay, TLS) from the measurement by design.
