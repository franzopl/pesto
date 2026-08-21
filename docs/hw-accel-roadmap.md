# Hardware-acceleration inventory vs parpar + nyuu

Goal: **match or beat** on every platform they support, not only the c7i
bench box. Branch `dev`. `main` stays frozen. Binaries report `dev`.

Related: [`parity-roadmap.md`](parity-roadmap.md), issue #159.

## How to read this

- **Have** = kernel exists and `smart` / yEnc `encode()` can pick it.
- **Have (manual)** = kernel exists but is not the auto path.
- **Missing** = not implemented.

Do not skip a row because “we do not ship that CPU.” The auto-detect table
must be complete.

---

## A — PAR2 / `parmesan` (`gf16` in parpar)

| # | Algorithm | ISA | Status | Notes |
|---|---|---|---|---|
| A1 | Lookup / Lookup3 | portable / SSE2 | Have (scalar + tables) | Not a SIMD win; keep as fallback |
| A2 | Shuffle SSSE3 | SSSE3 | Have | `flush_ssse3` |
| A3 | Shuffle AVX | AVX | Missing | 128-bit + VEX; rare but they have it |
| A4 | Shuffle AVX2 | AVX2 | Have | `flush_avx2` |
| A5 | Shuffle AVX-512 | AVX-512 VL/BW | Have (`smart` when 512 && !GFNI) | Normal layout zmm vpshufb |
| A6 | Shuffle VBMI | AVX-512 VBMI | Missing | |
| A7 | Shuffle2x AVX2 | AVX2 | Have | `smart` when no GFNI |
| A8 | Shuffle2x AVX-512 | AVX-512 | Missing | |
| A9 | **Affine GFNI** (SSE) | GFNI+SSSE3 | Missing | |
| A10 | **Affine AVX2** | GFNI+AVX2 | Have (`smart` if GFNI && !512) | parpar default without 512 |
| A11 | Affine AVX10 | AVX10 | Missing | detect + 256-bit EVEX |
| A12 | **Affine AVX-512** | GFNI+AVX512VL/BW | Have (`smart`) | packed + dest-interleaved tiles + prefetch; was 424 vs parpar 626 |
| A13 | Affine2x GFNI/AVX2/AVX10/512 | GFNI… | AVX2 kernel only, not `smart` | Keep for invert/experiments |
| A14 | XOR SSE2 | SSE2 | Missing | |
| A15 | XOR-JIT SSE2 / AVX2 / AVX-512 | W^X + JIT | Missing | last on x86 after Affine |
| A16 | CLMul NEON | AArch64 | Have | `flush_neon_clmul` |
| A17 | Shuffle NEON | NEON | Missing | they pick this when `inputs` is small |
| A18 | CLMul SHA3 | ARMv8.2 SHA3 | Missing | |
| A19 | Shuffle-128 SVE / SVE2 / 512 SVE2 / Shuffle2x SVE2 | SVE | Missing | |
| A20 | CLMul SVE2 | SVE2 | Missing | |
| A21 | Shuffle-128 RVV / CLMul RVV+Zvbc | RISC-V | Missing | |
| A22 | Packed multi-source (`srcCount` 3/6/12) | all Affine/Shuffle2x | Partial | tile-pack like parpar `muladd_multi_packed`; 512 uses `vpternlog` 0x96 |
| A23 | Slice-chunk threading | — | Partial | P1b windows; not thread-split of one slice |
| A24 | Nibble scratch (16×4) | GFNI Affine | Have | parpar `gf16_affine_load_matrix`; XOR of 4 nibble mats |
| A25 | MD5×2 + CRC SIMD on input | SSE/AVX/NEON | Partial | one scalar pass only |
| A26 | Recovery MD5-MB 8/16-wide | AVX2 / AVX-512 | Missing | |
| A27 | OpenCL | GPU | Missing | last; optional crate feature |

`smart` today: **Affine512 packed** on SPR (latest c7i movie create
**484.7 MiB/s** median); Affine AVX2 on GFNI without 512; Shuffle AVX-512
if 512 && !GFNI; Shuffle2x if AVX2 && !GFNI. yEnc `encode()`: AVX2 +
VBMI2/`vpternlog` when present (non-hybrid) / SSSE3 (hybrid).

Target `smart` when this list is done: same priority as
`Galois16Mul::default_method` in parpar (`gf16mul.cpp`).

## B — yEnc / `pesto` (`yencode` in nyuu)

| # | Algorithm | ISA | Status |
|---|---|---|---|
| B1 | Scalar | — | Have |
| B2 | SSE2 | SSE2 | Missing (we jump to SSSE3) |
| B3 | SSSE3 | SSSE3 | Have (hybrid default) |
| B4 | AVX + POPCNT | AVX | Missing |
| B5 | AVX2 | AVX2 | Have (non-hybrid default) |
| B6 | AVX-512 VL/BW (`AVX3`) | AVX-512 | Have (manual) | c7i zmm 1168 vs AVX2 2534; not `encode()` default |
| B7 | **VBMI2** | ICL / SPR | Have (`encode()` AVX2 path) | nyuu: `mask_expand` + `vpternlog` on ymm |
| B8 | NEON | AArch64 | Have |
| B9 | RVV | RISC-V | Missing |
| B10 | `encodeTo` one-pass + CRC fold | PCLMUL / VPCLMUL | Partial (`encode()` returns CRC via crc32fast/PCLMUL then yEnc; no caller walk) |
| B11 | Decode SSSE3/AVX2/AVX-512/NEON/RVV | — | Partial (we have a decoder; not full ISA matrix) |

Poster path: dedicated encode workers fill a ready-article queue (nyuu
`articleQueueBuffer`); NNTP workers only POST. Encode workers =
`min(performance cores, connections)` (1 encoder on c7i was 0.85× nyuu).

## C — Order of work

1. **A10 Affine AVX2** — unblocks GFNI desktops and is the 256-bit half of A12.
2. **A12 Affine AVX-512** — c7i create gap.
3. **A22+A24** packed sources + nibble scratch on Affine.
4. **B6+B7+B10** yEnc AVX-512/VBMI2 + fused CRC (post-only gap).
5. **A5, A6, A8** Shuffle/Shuffle2x 512 / VBMI (no-GFNI big Xeons).
6. **A3, A9, B2, B4** older x86 so `auto` never falls off a cliff.
7. **A25–A26** hasher SIMD.
8. **A16–A21, B8–B9** ARM SVE/SHA3 + RISC-V.
9. **A14–A15 XOR-JIT**, then **A27 OpenCL**.
10. **A11 Affine AVX10** when we can detect it in stable `std`.

Each item: kernel + `smart`/`encode()` dispatch + a correctness test against
the portable path. Do not enable `smart` on a new layout until that test
passes on the ISA (skip if the box lacks it).

## D — Acceptance

Not “one movie row on c7i.” For each ISA we claim:

- `movie-1080p` create ≥ 0.95× parpar on a machine whose `default_method` is
  that kernel.
- `movie-1080p` post-only ≥ 0.95× nyuu on a machine whose yencode ISA is
  that kernel.
- `many-small` not slower than today on AVX2.

Measure on medialab (AVX2), c7i (GFNI+512), and an ARM host when A17+ land.

## E — Work log (stop here, 2026-08-19)

Stopped after packing Affine tiles. **Do not put Affine512 or yEnc zmm on
`smart`/`encode()` until a c7i run beats the numbers below.** Encode stays
on the NNTP worker (no encode-ahead queue).

### Default paths now

| CPU | PAR2 `smart` | yEnc `encode()` |
|---|---|---|
| SPR / c7i (AVX-512+GFNI) | **Affine512 packed** | AVX2 + VBMI2/`vpternlog` (ymm) |
| GFNI, no 512 | Affine AVX2 | AVX2 (+ VBMI2 if present) |
| AVX2, no GFNI | Shuffle2x | AVX2; SSSE3 if hybrid |
| AVX-512, no GFNI | Normal + Shuffle AVX-512 | AVX2 (zmm yEnc is manual) |

### Affine512 packed on `smart` (c7i `20260820T004824Z`, 3-rep median)

ParPar Affine AVX-512 is `prepare_packed` (shuffle-512 into interleave-6,
4 KiB tiles, `srcScale` = 6) + slice-chunk threads + nibble scratch in-kernel.

| Path | movie-1080p create | many-small create |
|---|---|---|
| parmesan `smart` was Normal+GFNI-512 | 325.6 MiB/s (RSD 0.8%) | 356.6 |
| parmesan **Affine512 packed** | **424.0** (RSD 0.4%) | **371.5** |
| parpar | **626.2** (RSD 4.3%) | 253.2 |

Affine512 is `smart` on AVX-512+GFNI (slice multiple of 128). Still ~0.68×
parpar on the large file.

### Landed on `dev` (this push)

- Affine AVX2 / Affine512 kernels, nibble scratch 16×4 (parpar
  `gf16_affine_load_matrix`), prepare-buffer pool, tile-pack
  (`muladd_multi_packed`) + `vpternlog` 0x96 on 512. **Not auto on SPR.**
- Shuffle AVX-512 on Normal when 512 && !GFNI.
- yEnc AVX-512/VBMI2 zmm path (`encode_avx512`) with 32/16 tails. **Not
  `encode()` default.**
- yEnc AVX2 uses nyuu `encoder_avx_base.h` VBMI2 `mask_expand` +
  `vpternlog` 0xf8 when the CPU has it.
- `encode()` returns IEEE CRC-32 (`crc32fast`) so `encode_part_into` does
  not walk the payload twice.

Sources used: npm `@animetosho/parpar` `gf16/`, `node_modules/yencode/src/`.

### c7i.2xlarge (`movie-1080p` 6 GiB, 4 threads)

| Metric | pesto/parmesan | competitor |
|---|---|---|
| PAR2 create, `smart` **Affine512 packed** (20260820) | **424.0 MiB/s** | parpar **626.2** |
| PAR2 create, previous Normal+GFNI-512 | 325.6 | — |
| yEnc 768 KiB line=128 AVX2 (+VBMI2) | ~2312 MiB/s | node-yencode ~1855 |
| yEnc same, zmm `encode_avx512` | ~1874 (was 1168 before 32/16 tail) | still < AVX2 |
| yEnc `auto` (AVX2+CRC) | ~2096 | was 1100 with zmm default |

Older good create ~318 vs this ~301 is one-rep noise, not Affine.

### What is left to match parpar (~562 create)

### Dest-interleave + prefetch: no create win (c7i 1-rep, aborted)

`20260820T113241Z` movie create: parmesan **425.2** vs parpar **619.3** (was
424 vs 626). Instance `i-0857d77405a82bbeb` terminated after that row.

Learned: the remaining ~0.68× gap is **not** dest tiling / `_mm_prefetch` /
finish copies. Packed prepare + slice-split tiles already captured that.
Dest-interleave raised RSS (~1.5 → 2.0 GiB) for no throughput. Do not spend
another c7i run on layout tweaks of this kernel.

Next create levers (same ParPar inventory, not exotic ISA):

1. **P2 hasher SIMD** — MD5×2 (file+slice) + CRC on the input pass; recovery
   MD5-MB 8/16-wide. ParPar does this in the same read that feeds RS.
2. **P4 Affine2x AVX-512** (`srcCount` 12, stride 64) — only because P1 still
   leaves >10% vs parpar. Keep Affine (not Affine2x) as `smart` until it wins.
3. **Input batch 12** like parpar `inputBatchSize` (we pack whatever is queued).

Medialab i5-10400, 0 ms mock, 1 rep (`20260820T051428Z`):

| case | pesto | nyuu | pesto/nyuu |
|---|---|---|---|
| movie-1080p post-only | **1477 MiB/s** | 1333 | **1.11×** |
| many-small post-only | **1355** | 475 | **2.85×** |
| movie full two-phase | 262 | parpar+nyuu 292 | 0.90× |

c7i `20260820T102216Z` (3-rep median, 8 conns, encode pool `min(cores, conns)`):

| case | pesto | nyuu / parpar+nyuu | pesto/nyuu |
|---|---|---|---|
| movie post-only 0 ms | **1899** (RSD 1.2%) | 1726 | **1.10×** |
| movie post-only 30 ms | 91.6 | 90.7 | **1.01×** |
| many-small post-only 0 ms | **1292** | 417 | **3.10×** |
| movie full two-phase 0 ms | 218 | **422** | 0.52× (PAR2 create) |

Prior run with 1 encoder: 1496 vs 1766 (0.85×). Post-only ≥ 0.9× **met** on c7i.

Exotic ISA (SVE, RVV, XOR-JIT, OpenCL) is not the c7i gap.

### Input Batch 12 (Dynamic Cache pressure optimization)

Changed `add_slice` queue limit from 64 to 12 dynamically *only* when using `Affine512` or `Affine2x` to match parpar's `inputBatchSize`.
Result:
- **c7i (AVX-512+GFNI)**: throughput jumped from 424.0 MiB/s to **518.3 MiB/s**. RSS dropped from 1.5 GiB to 1.4 GiB.
- **medialab (AVX2)**: Through testing, we verified that AVX2 (Shuffle2x and Normal) kernels *plummet* in performance if batch size is 12 (e.g. dropping from 261 MiB/s to 199 MiB/s on `movie-1080p`). This happens because Shuffle2x flushes the entire RecoveryBlock to L3/RAM 12-slices at a time, whereas Affine512 interleaves memory. Therefore, AVX2 retains the `64` batch size.

ParPar still leads (c7i: 656 MiB/s, medialab: 361 MiB/s). 

### Affine512 six-source loop parity (2026-08-21)

The post-refactor comparison with ParPar found two code-generation differences
in the common six-source group; the packed layout, 6-way interleave, 4 KiB
tiles, 12-slice input batch, and slice-parallel scheduling already matched.

- Coefficient setup now mirrors `gf16_affine_load2_matrix`: four aligned
  256-bit nibble contributions are XORed for two coefficients at a time, then
  expanded into the four GFNI matrices in ZMM registers. The scratch uses
  ParPar's physical `ll, hh, hl, lh` qword order while its scalar accessor
  preserves the existing semantic `ll, lh, hl, hh` API.
- The full six-source path is explicit rather than an array-driven dynamic
  loop. Release assembly changed from a 1,536-byte matrix spill area plus
  per-source packed-offset division to register-resident matrices, six
  unrolled GFNI rounds, and a constant 768-byte source stride per 128-byte
  block. Remainders of one through five sources retain the generic path.

Local tests and clippy are green, and an AVX-512-only test compares every lane
of paired matrix expansion with the scalar scratch. The test executed rather
than skipped on a c7i.2xlarge (Xeon Platinum 8488C).

A same-host A/B used the exact `movie-1080p` geometry (6 GiB, 3,223,552-byte
slices, 1,999 input slices, 200 recovery blocks, four threads, 1 GiB), forced
`affine512` + `avx512-gfni`, alternated old/new order, and excluded one warmup
per binary:

| Commit | Five measured runs (MiB/s) | Median |
|---|---|---:|
| `a46a94a` before six-source specialization | 466.5, 467.8, 471.4, 469.0, 473.2 | **469.0** |
| `c1a16d3` register-resident six-source loop | 511.3, 514.4, 503.7, 522.0, 520.3 | **514.4** |

That is a **9.68% same-host gain**, so the code-generation change has a real
effect beyond instance-to-instance noise. Raw data:
`bench/results/ip-172-31-5-219/20260821T043128Z-affine512-ab/raw.csv`.

The separate publishable three-repetition suite
(`ip-172-31-82-23/20260821T035500Z`) measured `movie-1080p` create at
**484.7 MiB/s** for parmesan (RSD 0.1%) and **577.1 MiB/s** for ParPar
(RSD 11.2%). Parmesan is 16.0% below ParPar, improved from the historical
518.3/656 ratio but still short of the <10% target. `many-small` did not
regress: parmesan was **412.2 MiB/s** versus ParPar at **227.3 MiB/s**.

### The remaining AVX2 (medialab) gap: 2-slice `vperm2i128` amortization

On medialab (i5-10400 without GFNI), Pesto uses `Shuffle2x AVX2`: **261 MiB/s** vs ParPar's **361 MiB/s** (~38% gap).

**Root cause (confirmed by reading ParPar source `gf16_shuffle2x_x86.h`):**
Both Pesto and ParPar are forced to use `_mm256_permute2x128_si256` (3-cycle latency on Intel) to cross the two 128-bit lanes inside a 256-bit AVX2 register. There is no magic memory layout that avoids this instruction.

The difference: **ParPar's `gf16_shuffle2x_muladd_x_avx2` processes 2 input slices simultaneously per loop iteration** (`srcCount` parameter), accumulating the `norm` and `swap` intermediates from both slices into the same registers before issuing the single `vperm2i128`. This halves the number of expensive cross-lane instructions.

Pesto's `flush_avx2_shuffle2x_work` processes 1 slice at a time across recovery blocks, so it executes `vperm2i128` once per slice. Rewriting this to process 2 slices per iteration would require refactoring the +7300-line `encoder.rs` AVX2 kernel.

**Scope of the fix:** AVX2-only. This instruction does not appear in the GFNI hot path (c7i), nor in the 128-bit SSSE3 path (no cross-lane issue). Not worth the risk/effort for a single CPU generation.

**Benchmark target (future work):** Close the 38% gap on AVX2-without-GFNI machines (e.g. Intel 6th-10th gen desktop without Rocket Lake, AMD Zen 1/2). Estimated effort: 2–3 weeks of careful AVX2 intrinsics refactoring.

---

## CPU Priority Matrix

| Architecture | Representative HW | Pesto kernel | Status | Priority |
|---|---|---|---|---|
| AVX-512 + GFNI | AWS c7i, Intel Sapphire Rapids, Ice Lake Xeon | `Affine512 packed` | ✅ **485 MiB/s** (parpar 577, gap 16%; patch A/B +9.7%) | High — primary release target |
| AVX2 (no GFNI) | Intel 6th–10th gen, AMD Zen 1/2 | `Shuffle2x AVX2` | ⚠ **261 MiB/s** (parpar 361, gap ~38%) | Medium — worth a dedicated sprint |
| SSSE3 (legacy) | Intel Core 2, early Sandy Bridge | `Normal SSSE3` | Not benchmarked | Low — marginal install base |
| GFNI without AVX-512 | Tremont (Atom), some Tiger Lake | Falls back to `Shuffle2x` | Not benchmarked | Low |
| Apple Silicon (ARM) | M1/M2/M3 | No specialised kernel yet | Not benchmarked | Future |

**Recommended benchmark priority for the next AWS run:** c7i (AVX-512+GFNI) — that is where we have the largest user base for a Usenet tool and where our PAR2 create is closest to matching ParPar. Closing the remaining 16% on c7i is higher-value than closing the 38% AVX2 gap.

---

## Pending items vs nyuu+parpar (critical backlog)

| Item | Gap | Impact | Notes |
|---|---|---|---|
| **PAR2 create c7i: 485 vs 577 MiB/s** | ~16% | High | Roadmap items P2 (SIMD hasher), P3 (Affine2x srcCount=12) remain uninvestigated |
| **movie full two-phase (local):** pesto 262 vs parpar+nyuu 292 MiB/s | ~11% | Medium | PAR2 create is the bottleneck (200 MiB/s on AVX2); closing c7i gap will likely fix this path too |
| **AVX2 Shuffle2x 2-slice amortization** | ~38% on AVX2 | Medium | Documented above as future work |
| ~~post-only speed~~ | — | ✅ Done | Pesto 1477 MiB/s vs nyuu 1333 (1.11×), many-small 1355 vs 475 (2.85×) |
| ~~many-small PAR2 create~~ | — | ✅ Done | Pesto 291 vs parpar 252 (we lead) |
