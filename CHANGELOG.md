# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.8.2] — 2026-08-20

### Fixed
- Season-mode global PAR2 streams each memory-budget pass to disk instead of concatenating the whole recovery set in RAM (#110).
- CI: Clippy `-D warnings` and aarch64 compile of parmesan 0.5.2.

## [0.8.1] — 2026-08-20

### Performance (parmesan 0.5.1)

This release is a focused performance sprint targeting PAR2 create throughput
and yEnc encode speed. No API or on-disk format changes.

#### PAR2 kernel improvements (AVX-512 + GFNI — e.g. AWS c7i, Xeon Ice Lake)

- **Affine512 packed kernel** (`srcCount=6`, 4 KiB tiles) is now the `smart`
  default on AVX-512+GFNI hardware, replacing the previous `Normal+GFNI-512`
  path. PAR2 create throughput on `c7i.2xlarge movie-1080p`:
  `325 → 424 MiB/s` (initial), then `424 → 518 MiB/s` after dynamic batching.
- **Affine AVX-512+GFNI** and **Affine AVX2+GFNI** kernels added (ParPar
  `gf16_affine` equivalent).
- **Shuffle AVX-512 nibble** kernel added (no-GFNI fallback for AVX-512 CPUs).
- **Affine 16×4 nibble scratch** from ParPar's dependency-table layout.
- **Affine tile packing** (`muladd_multi_packed` equivalent): tiles are now
  packed in source-interleaved layout, reducing L2 pressure.
- **Dynamic batch sizing**: `add_slice` now queues 12 slices per flush on
  Affine512 (matching ParPar's `inputBatchSize`) and 64 on Shuffle2x/Normal.
  This alone accounts for the `424 → 518 MiB/s` jump on c7i.
- Affine shuffle-prepare buffers are reused across flushes (avoids repeated
  allocation in the hot path).
- `smart` auto-selection now verifies that a kernel beats the current best
  before promoting it — prevents regressions on mismatched ISA detection.

#### yEnc encode improvements (pesto 0.8.1)

- **AVX2 yEnc** (nyuu `encoder_avx_base.h` style, VBMI2 `mask_expand` +
  `vpternlog 0xF8`) on non-hybrid CPUs. Measured ~2312 MiB/s on c7i.
- **AVX-512 BW + VBMI2** yEnc path added (available but not default — AVX2
  wins on current hardware).
- **IEEE CRC-32 folded into yEnc encode**: CRC is now computed during the
  encode pass via `crc32fast`, eliminating a second walk over the payload.
- **encode off the POST path**: yEnc encoding now runs on a dedicated pool
  thread with a ready-article queue, fully decoupling CPU-bound encode from
  I/O-bound NNTP posting.
- One yEnc encode thread on CPUs with ≤4 cores to avoid starvation.

#### Hasher

- **SIMD MD5-MB 8/16-wide** (`md5-many` crate): slice and file checksums now
  use multi-buffer MD5 instead of scalar, reducing hasher overhead on the
  input read path.

### Benchmarks (vs 0.8.0 / parmesan 0.5.0)

| Machine | Workload | 0.8.0 | 0.8.1 | Δ |
|---|---|---|---|---|
| c7i.2xlarge (AVX-512+GFNI) | movie-1080p PAR2 create | ~325 MiB/s | **518 MiB/s** | +59% |
| medialab i5-10400 (AVX2) | movie-1080p PAR2 create | ~130 MiB/s | **200 MiB/s** | +54% |
| medialab i5-10400 | movie post-only (0 ms) | ~1050 MiB/s | **1477 MiB/s** | +41% |
| medialab i5-10400 | many-small PAR2 create | — | **291 MiB/s** | leads ParPar |

## [0.8.0] — 2026-08-18

### Added

#### Phase 47 — Season Mode: Global PAR2

- **`--season` mode now generates a global PAR2 recovery set** that covers the entire season uniformly instead of separate rsids per episode
  - Eliminates confusion from multiple PAR2 recovery set IDs in consolidated season NZBs
  - Single coherent rsid ensures compatibility with all Usenet downloaders (SABnzbd, NZBGet, etc.)
  
- **New public API functions** in `pesto::poster`:
  - `pub async fn generate_season_par2()` — generates recovery slices covering all episodes in one pass (single file re-read)
  - `pub async fn generate_and_write_season_par2()` — generates and writes PAR2 volumes to disk
  
- **Automatic integration into `--season` consolidation**:
  - After all episodes post individually with their own PAR2 sets, pesto generates a global PAR2 covering the entire season
  - Posts PAR2 volumes as NNTP articles
  - Includes global PAR2 in consolidated season NZB (filters out per-episode PAR2 sets)
  - Result: single season NZB with unified PAR2 recovery
  
- **Behavior**:
  - Individual episode NZBs remain unchanged (data + local PAR2 per episode)
  - Season NZB contains: all episode data + global PAR2 volumes only (single rsid)
  - Non-fatal: if global PAR2 generation fails, falls back to per-episode PAR2 sets
  
- **Documentation**: Updated `--obfuscate=full-shared` mode in README with indexer compatibility notes

### Fixed

- Season NZB consolidation now correctly filters out per-episode PAR2 sets to avoid multiple rsids
- PAR2 volume posting no longer generates recursive PAR2 for the volume files themselves
- The internal PAR2-only NZB used to post the season's global PAR2 volumes no longer leaks onto disk as an orphaned temp file — it used to be written to a path distinct from the one actually cleaned up on drop, so it could persist and get submitted to an indexer ahead of the real season NZB
- That same internal PAR2-only upload no longer runs the user's configured `post_hooks` — `no_hooks` only ever suppressed the hooks-directory scan, so a configured indexer-submission hook still fired against it
- The season's global PAR2 now covers the files actually posted (the compressed archive, under `--compress`/`--password`) instead of the original, never-posted episode files, whose temp archive used to be deleted before the season PAR2 step could read it
- The season's own PAR2 volumes no longer get wrapped in a password-protected archive themselves when `--compress`/`--password` is active — they're posted as raw, readable `.par2` files like any other PAR2 set

### Tested

- Validated with small season (3 × 5 MB episodes): ✓ single global rsid
- Validated with large season (10 × 10 MB episodes): ✓ 157 segments with single coherent rsid
- Validated with 26-episode season (6.2 GB): ✓ single global rsid with 99 recovery blocks
- Individual episodes still independently recoverable via their own NZBs
- Season-wide recovery via global PAR2 set
- SABnzbd verification: ✓ all files correct, repair not required

### Notes

- **PAR2 File Cleanup**: SABnzbd's automatic PAR2 cleanup (`enable_par_cleanup`) only works for simple downloads (1 file : 1 PAR2). Season mode creates a complex download (multiple files : 1 PAR2 set), so SABnzbd does not delete PAR2 files automatically. Workaround: keep Radarr/Sonarr active — it automatically deletes PAR2 when moving files to the library.
- **Backwards Compatible**: NZBs created before Phase 47 still work correctly with their individual per-episode PAR2 sets.

---

## Historical Versions

Refer to git tags for previous releases: `v0.5.10` and earlier.
