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
| A10 | **Affine AVX2** | GFNI+AVX2 | Have (`smart` on GFNI) | parpar default without 512 |
| A11 | Affine AVX10 | AVX10 | Missing | detect + 256-bit EVEX |
| A12 | **Affine AVX-512** | GFNI+AVX512VL/BW | Have (`smart` when 512+GFNI) | parpar **default on c7i** |
| A13 | Affine2x GFNI/AVX2/AVX10/512 | GFNI… | AVX2 kernel only, not `smart` | Keep for invert/experiments |
| A14 | XOR SSE2 | SSE2 | Missing | |
| A15 | XOR-JIT SSE2 / AVX2 / AVX-512 | W^X + JIT | Missing | last on x86 after Affine |
| A16 | CLMul NEON | AArch64 | Have | `flush_neon_clmul` |
| A17 | Shuffle NEON | NEON | Missing | they pick this when `inputs` is small |
| A18 | CLMul SHA3 | ARMv8.2 SHA3 | Missing | |
| A19 | Shuffle-128 SVE / SVE2 / 512 SVE2 / Shuffle2x SVE2 | SVE | Missing | |
| A20 | CLMul SVE2 | SVE2 | Missing | |
| A21 | Shuffle-128 RVV / CLMul RVV+Zvbc | RISC-V | Missing | |
| A22 | Packed multi-source (`srcCount` 3/6/12) | all Affine/Shuffle2x | Partial | Affine AVX2=3; Affine512=6; Shuffle2x=2; Affine2x=6 |
| A23 | Slice-chunk threading | — | Partial | P1b windows; not thread-split of one slice |
| A24 | Nibble scratch (16×4) | GFNI Affine | Missing | still build 8×8 per pair |
| A25 | MD5×2 + CRC SIMD on input | SSE/AVX/NEON | Partial | one scalar pass only |
| A26 | Recovery MD5-MB 8/16-wide | AVX2 / AVX-512 | Missing | |
| A27 | OpenCL | GPU | Missing | last; optional crate feature |

`smart` today: Shuffle2x if AVX2 && !GFNI; else Normal + GFNI/AVX2/SSSE3/NEON.

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
| B6 | AVX-512 VL/BW (`AVX3`) | AVX-512 | Have (worker default on non-hybrid AVX-512) |
| B7 | **VBMI2** | ICL / SPR | Have (`mask_expand_epi8` escape store) |
| B8 | NEON | AArch64 | Have |
| B9 | RVV | RISC-V | Missing |
| B10 | `encodeTo` one-pass + CRC fold | PCLMUL / VPCLMUL | Partial (`encode()` returns CRC via crc32fast/PCLMUL then yEnc; no caller walk) |
| B11 | Decode SSSE3/AVX2/AVX-512/NEON/RVV | — | Partial (we have a decoder; not full ISA matrix) |

Default poster path stays **encode on the NNTP worker**. No encode-ahead
queue unless an explicit opt-in flag is added later (not in this list’s
critical path).

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
