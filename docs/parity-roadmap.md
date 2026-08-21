# Performance parity vs parpar + nyuu

Completed performance-parity plan on branch `dev`. Release-validation binaries
built from this branch report version **`dev`** until the release bump.

Issue: [#159](https://github.com/franzopl/pesto/issues/159).
Related: #148 (closed; GFNI large-file gap reopened by the 0.8.0 AWS pair).

## Target

Match or beat `parpar`+`nyuu` on:

| Platform | Example | Release validation (2026-08-21) |
|---|---|---|
| AVX2 desktop, 6c | medialab i5-10400 | movie create −17%; post-only pesto 1.66x Nyuu |
| AVX2/AVX-512, 4c, no GFNI | c5.2xlarge | movie create −57%; post-only nyuu +60% |
| AVX-512+GFNI, 4c | c7i.2xlarge | movie create **−4.3%** (553.2 vs 577.8); target met |

`many-small` already wins on AVX2 and GFNI. Do not regress it.

## What the competitor actually does

**Parpar** (`gf16/`, `hasher/`, `lib/par2gen.js`):

- Prepare-once Shuffle2x / **affine2x** layout; RS never sees Normal bytes.
- Fuses 2 (AVX2) / 6 (GFNI) / 12 (AVX-512) input slices per dest store.
- 4–8 KiB tiles; threads split the **slice**, not only recovery blocks.
- Slice-chunk *or* recovery-pass when RAM is tight.
- SIMD MD5×2 (file+slice) + CRC on the input pass; MD5-MB on recovery.

**Nyuu**: one encode thread into a buffer pool; connections pull. Eight parallel
SIMD encodes on four cores is the Xeon post-only loss, not yEnc quality.

Do **not** copy OpenCL, RVV/SVE, XOR-JIT, or “just enable AVX-512”
(AVX-512+GFNI ≈ AVX2+GFNI in FINDINGS).

## Phases

### P0 — Shuffle2x pack (this branch, first)

- [x] Roadmap + `DISPLAY_VERSION = "dev"`
- [x] Prepare each queued input **once** (`shuffle2x::to_shuffle2x`)
- [x] Fuse `srcCount=2` in `flush_avx2_shuffle2x_work` (4-recovery group)
- [x] Tiles 8 KiB (was 32 KiB)
- [x] `cargo test -p parmesan-par2` layout/compat tests green

Acceptance: medialab `movie-1080p` create ≥ parpar; `many-small` not slower.

### P1 — Affine2x on GFNI

- [x] Affine2x kernel exists behind `new_affine2x` only (not `smart`)
- [x] Affine AVX2 (parpar default without 512) is the GFNI `smart` path
- [x] Affine AVX-512 (parpar default on c7i) — `smart` when 512+GFNI (c7i movie 424 vs parpar 626)
- [x] Kernel: 2× `gf2p8affine` + lane swap, `srcCount=6`, 4 KiB tiles
- [x] Scratch = 16×4 matrices, not 65k dep tables

Acceptance: c7i `movie-1080p` create within ~10% of parpar — **met** after
progressive output-tile prefetch and batched SIMD slice MD5: **553.2 vs
577.8 MiB/s (4.3% gap)**. `many-small` is **464.3 vs 234.9 MiB/s (1.98x)**.

### P1b — Slice-chunk memory

- [x] When `recovery × slice > memory_limit`, cut the **slice** (re-read) like
      parpar `chunks`, not only recovery `passes`

### P2 — Fused hasher

- [x] MD5 slice + CRC in one memory pass (`slice_checksum`)
- [ ] SIMD MD5×2 (file+slice) on the input stream
- [ ] Recovery MD5-MB (8-wide AVX2 / 16-wide AVX-512)

### P3 — Poster encode concurrency

- [x] Encode off the POST path; ready-article queue
      (`articleQueueBuffer` = min(round(conns×0.5)+2, 25))
- [x] Encode workers = `min(cores, connections)` (1 encoder missed 0.9× on c7i)
- [x] Prefer AVX2 yEnc on non-hybrid CPUs (CPUID Hybrid bit); SSSE3 on P+E
- [x] Article body pool / `encode_part_into` (nyuu `encodeTo`)

Acceptance: c7i post-only movie ≥ 0.9× nyuu; medialab post-only not worse.
Medialab: **1.11×**. c7i `20260820T102216Z`: **1.10×** (1899 vs 1726). Met.

### P4 — AVX-512 affine2x only if P1 still leaves >10%

Not required: the Affine512 packed path reached the acceptance target. Keep
Affine2x-512 deferred; this release does not reopen kernel work for another
1–2%.

## Release validation

The official local run used three repetitions, governor `performance`, warm
page cache, and the complete `par2 e2e correctness` suites for `many-small`
and `movie-1080p`. Post-only movie was **2226.1 vs 1339.4 MiB/s** (Pesto
1.66x Nyuu) at 0 ms and **92.2 vs 92.0 MiB/s** at 30 ms. Like-for-like movie
two-phase was within 6.3% of ParPar+Nyuu at 0 ms and within 2.1% at 30 ms;
Pesto's streaming pipeline reached 82.8 MiB/s at 30 ms, 1.21x the competing
two-phase pipeline. All nine cross-tool correctness checks passed.

See the publishable tables and committed artifacts in
[`bench/README.md`](../bench/README.md).

## Measure

Every phase: `./bench/run.sh par2 e2e --workload many-small --workload movie-1080p --yes`
on medialab **and** a GFNI box (`INSTANCE_TYPE=c7i.2xlarge ./bench/aws-run.sh`).

Do not close a phase on a single-machine create row (#148).
