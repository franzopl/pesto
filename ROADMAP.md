# Roadmap — `pesto`

Fast, lean Usenet poster in Rust. Inspired by `nyuu`, with only the essentials.
Each phase must leave the program in a working, testable state.

---

## Completed ✅

| Phase | Topic |
|-------|-------|
| 0 | Foundation — workspace, CLI skeleton, config structs, logging |
| 1 | yEnc encoder — `encode_into`, CRC32, segmentation, headers |
| 2 | Basic NNTP — TCP connection, `POST`, `240` response |
| 3 | TLS & Auth — `rustls`, `AUTHINFO USER/PASS`, env-var credentials |
| 4 | Concurrent posting — connection pool, MPSC work queue, Ctrl-C |
| 5 | NZB generation — XML writer, Message-ID capture, file grouping |
| 6 | Config file — TOML load, CLI-override merge, multi-group |
| 7 | PAR2 foundation — GF(2¹⁶), Cauchy matrix, packet serialization |
| 8 | PAR2 advanced — MD5 hashing, single-pass parity, AVX2/SSSE3 SIMD |
| 9 | Local archive & obfuscation — RAR/7z, filename randomisation, passwords |
| 10 | Metadata & hooks — `.nfo` generation, post-hooks, Newznab, Discord |
| 11 | Error resilience — retry/backoff, resume state file, STAT verification |
| 12 | Performance — double-buffered reader, buffer pool, Rayon, rate limiting |
| 13 | Polish & UI — ANSI multi-bar, JSON-L mode, setup wizard, sparklines |
| 20 | Modularisation — split wizard, TUI, PAR2 worker, config into sub-modules |
| 21a | Cargo workspace — `parmesan` extracted to `crates/parmesan` |
| 21b | API decoupling — removed NNTP terminology, generic `Read`-based API |
| 21c | Benchmarking — micro-benchmarks in library, `#[inline]` tuning, docs |
| 21d | Publish — `parmesan-par2` v0.1.0 published to crates.io |

---

## Completed ✅ (continued)

### Phase 29 — Public Release Readiness

Pre-requisites before announcing `pesto` in Usenet forums (Reddit r/usenet,
Usenet-Info, NZBForum, etc.).

#### 29a — Repository cleanup ✅

- [x] Move ad-hoc benchmark shell scripts from root to `bench/` directory.
- [x] Add `bench/results/`, `bench/par2_out/`, `node_modules/` to `.gitignore`.
- [x] Remove `GEMINI.md` and `node_modules/` from git tracking.
- [x] Legacy `bench_*.sh` removed from tracking; superseded by `bench/`.

#### 29b — Benchmark suite *(medium complexity)*

A reproducible, portable benchmark suite that anyone can run to compare
`pesto`/`parmesan` against established tools (`nyuu`, `parpar`, `par2`).

- [x] Create `bench/README.md` explaining how to run each benchmark.
- [x] `bench/yenc.sh` — yEnc throughput: pesto SIMD paths vs `node-yencode`.
  - Auto-generates sparse test files; sizes configurable via CLI args.
  - Prints CPU model, core count, SIMD feature flags detected.
  - Emits a Markdown-formatted comparison table to stdout (copy-paste ready).
  - Saves raw results to `bench/results/yenc-<hostname>-<date>.csv`.
- [x] `bench/par2.sh` — PAR2 creation: parmesan vs `parpar` vs `par2cmdline`.
  - Same structure as `yenc.sh`; compares throughput and output file sizes.
  - Detects which comparison tools are installed; skips missing ones gracefully.
  - Saves raw results to `bench/results/par2-<hostname>-<date>.csv`.
- [x] `bench/posting.sh` — End-to-end post throughput (dry-run / loopback mode).
  - Uses `--dry-run`; no real server needed.
  - Measures: file read → yEnc encode → article assembly → (simulated) send.
- [x] Common library (`bench/lib.sh`): color helpers, `hr`, `speedup_pct`,
      `throughput_mbps`, CPU detection, sparse file creation — shared by all scripts.
- [x] Old root-level `bench_*.sh` scripts removed from tracking.

#### 29b — Benchmark suite ✅ *(completed above)*

#### 29c — README benchmark table ✅

- [x] "Performance" section added to `README.md` with yEnc and PAR2 tables.
- [x] Link to `bench/README.md` so readers know how to reproduce the numbers.

#### 29d — Release tag v0.3.1 *(pending)*

- [x] `CHANGELOG.md` promoted to `[0.3.1]` (2026-05-24).
- [ ] Push tag `v0.3.1` to trigger CI release workflow (Linux glibc/musl + Windows).
- [ ] Confirm GitHub Release page shows all three binary archives.

---

### Phase 21d — Publish `parmesan` to crates.io ✅

- [x] Version the library independently from `pesto`.
- [x] Publish `parmesan-par2` v0.1.0 to crates.io (2026-05-23).
- [x] `pesto` depends on the crate via workspace path (retained for monorepo convenience).

See [`crates/parmesan/ROADMAP.md`](crates/parmesan/ROADMAP.md) for the full
`parmesan` roadmap.

---

## Next — Phase 22+: Complete PAR2 Tooling

The resource/geometry flags from the original Phase 22 plan are **already
implemented**. The focus now is on verify/repair, input flexibility, volume
layout control, and documentation.

Details live in [`crates/parmesan/ROADMAP.md`](crates/parmesan/ROADMAP.md).

---

## Phase 23 — Interactive TUI (Ratatui)

### 23a — Dashboard layout
- [ ] Replace current ANSI output with a `ratatui` layout.
- [ ] Tabs: `Progress`, `Logs`, `Connections`, `PAR2 Status`.
- [ ] Real-time throughput graph (`Canvas` or `Sparkline` widget).

### 23b — Interactive controls
- [ ] Pause/resume upload via keyboard.
- [ ] Adjust connection count at runtime.
- [ ] Scrollable, filterable log buffer.

---

## Phase 24 — Hot-Path Serialization: Scatter-Gather POST

Eliminate the redundant full-article copy that `Article::serialize()` currently
produces before every NNTP `POST`.

### Background

`serialize()` allocates a new `Vec<u8>` (~768 KB) per article by concatenating
headers and the yEnc body. This copy is unnecessary: the socket can receive two
disjoint buffers in a single syscall via scatter-gather I/O.

### 24a — Vectored writes on the NNTP connection

- [x] Replace `Connection::post(&[u8])` with `Connection::post_parts(&[u8], &[u8])`.
- [x] Use sequential `write_all` calls (coalesced by the `BufWriter` from 24b)
  to send headers + yEnc body without copying the body.
- [x] Keep `Article::serialize()` for tests; production path uses `build_headers()`.
- [x] The body is written without dot-stuffing because yEnc encoding already
  escapes `'.'` at line start (yEnc spec §4).

### 24b — TLS write buffering

- [x] Wrap the TLS stream in a `BufWriter` sized to ≥ 1 article to allow the
  TLS layer to coalesce small header writes with the body in one record,
  reducing syscall count and TLS fragmentation overhead.

---

## Phase 25 — NNTP Pipelining

Post multiple articles without waiting for the `240 Article received` response
of the previous one. This halves round-trip latency cost per article on
high-latency links (>50 ms RTT).

### 25a — Pipeline depth N

- [x] Send up to N `POST` commands and bodies back-to-back on the same
  connection before reading any responses.
- [x] Collect responses in order (NNTP responses arrive in command order).
- [x] On failure mid-pipeline, mark remaining articles as failed and retry the
  batch on the next attempt with `slot.invalidate()`.
- [x] Expose `--pipeline-depth` CLI flag and `posting.pipeline_depth` config
  option (default: 1; recommended 4–8 for high-latency servers).
- [x] Pipelining is automatically disabled when `--verify` is active (STAT
  after each article is incompatible with batched response reads).

### 25b — Adaptive pipeline depth ✅

- [x] `pipeline_depth = 0` (new default) enables adaptive mode; explicit values
  override it as before.
- [x] Warm-up: the first article on each connection is posted sequentially.
  `encode_time` (CPU) and `post_time` (send + RTT) are measured via `Instant`.
- [x] `depth = clamp(1, MAX_AUTO_PIPELINE_DEPTH=8, ceil(post_time / encode_time))`.
  Since encoding is ~375 µs and a typical post takes 5–100 ms, depth naturally
  converges to 8 on high-latency links and 1–2 on low-latency ones.
- [x] Computed depth logged at `INFO` level: `adaptive pipeline depth computed`.
- [x] `--pipeline-depth 1` still forces sequential; `--verify` always forces
  sequential regardless of the flag (STAT after each article is incompatible
  with batched response reads).
- [ ] Cap at server-side queue limit (detect `441 Too many articles`): deferred
  to a future hardening phase — the depth=8 cap avoids triggering it in practice.

---

## Phase 26 — yEnc SIMD Encoder

Replace the byte-at-a-time yEnc loop with a SIMD-accelerated implementation
that processes 16–32 bytes per cycle.

Complexity levels, in order: scalar correctness → SSSE3 (16-byte) →
AVX2 (32-byte) → buffer pre-computation. Each level uses the previous
level's tests as a golden reference before any SIMD code is merged.

### 26a — Scalar baseline with full test coverage *(low complexity)* ✅

- [x] Extract the yEnc encode loop into `pub fn encode_scalar(out: &mut Vec<u8>, data: &[u8], line_len: usize)`.
- [x] 30 unit tests: all four critical bytes at first/middle/last/consecutive positions,
  positional escapes for space/tab/dot at line boundaries, exact wrap-around, 256-byte round-trip, CRC32 check values.
- [x] Micro-benchmark in `benches/yenc.rs` — baseline ~515 MB/s.

### 26b — SSSE3 baseline (x86-64) *(medium complexity)* ✅

- [x] `pub fn encode_ssse3`: runtime dispatch via `is_x86_feature_detected!("ssse3")`.
- [x] 16-byte inner loop: `_mm_add_epi8` shift, 4× `_mm_cmpeq_epi8` escape mask, `_mm_movemask_epi8`; zero-mask fast path writes 16 bytes direct.
- [x] Line-start and line-end bytes always scalar (positional escape rules); only critical bytes need escaping in the middle zone.
- [x] 8 golden-reference tests verify SSSE3 output matches `encode_scalar` exactly (750 KB payload, all byte values, boundary positions, short line lengths).
- [x] Benchmark: **~1680 MB/s** (≈3.2× scalar).

### 26c — AVX2 (256-bit) path *(medium-high complexity)* ✅

- [x] `pub fn encode_avx2`: 32-byte AVX2 chunks in the middle zone, SSSE3 16-byte remainder, scalar tail.
- [x] `pub fn encode()` dispatcher: AVX2 > SSSE3 > scalar, selected once per call via `is_x86_feature_detected!`. `encode_part` now calls `encode()`.
- [x] 9 golden-reference tests verify AVX2 output matches `encode_scalar` exactly.
- [x] Benchmark: **~1470 MB/s** (≈2.8× scalar). For `line_len=128` the safe zone is 126 bytes (3 AVX2 + 1 SSSE3 chunks), so SSSE3 edges it out at this line length; longer lines favour AVX2.

### 26d — Buffer pre-reservation *(high complexity)* ✅

- [x] Add `pub fn encoded_size(data, line_len) -> usize`: exact scalar count of
  output bytes (escaped pairs + CRLF termintors). Useful for callers that need
  the buffer size before encoding (NZB builders, fixed-size writers).
- [x] Replace per-chunk `reserve(16/32)` calls inside SIMD loops with a single
  O(1) upper-bound reserve at function entry:
  `data.len() * 2 + (data.len() / line_len + 1) * 2` (always sufficient).
  Calling `encoded_size()` inside SIMD encodes would add a full O(n) scalar
  pass and eliminate the SIMD speedup — O(1) upper bound is the right trade-off.
- [x] 6 new tests verify `encoded_size` matches actual output length for all
  boundary conditions and a 750 KB payload.

---

## Phase 27 — yEnc Encoder: AVX2 Correctness & line_len Scaling

Target: exceed nyuu's documented yEnc throughput (~1.2 GB/s AVX2 at
`line_len=128`) and reach 2–3 GB/s at `line_len=256`. All changes must keep
the full Phase 26 golden-reference test suite green.

### 27a — Diagnose AVX2 underperformance *(investigation — closed)*

**Finding:** the 256→128 register-mixing hypothesis was wrong. Removing the
128-bit SSSE3 remainder from `encode_avx2_impl` and replacing it with scalar
made performance *worse* (1930→1801 MB/s at ll=256). The real cause is
arithmetic: the safe zone per line (`line_len - 2`) does not divide evenly
into 32-byte AVX2 chunks — at `ll=128`, SSSE3 fits 7 chunks of 16 (112 B)
while AVX2 fits only 3 chunks of 32 (96 B) before the tail. SSSE3 does more
useful SIMD work per line at these standard line lengths.

**Resolution (27b):** fix the dispatcher, not the AVX2 implementation.
`encode_avx2` is retained for benchmarking and multi-line future work.

### 27b — Dispatcher: always prefer SSSE3 *(low complexity)* ✅

Benchmarks showed SSSE3 beats AVX2 at ll=128 on hybrid CPUs (Intel 12th gen+).
Root cause: E-cores execute AVX2 ~5% slower than SSSE3 at this line length;
P-cores are within noise (<0.3%). SSSE3 is the safe default across all core
types with no P-core penalty. AVX2 would only win with a multi-line strategy
that amortises the per-line boundary cost on P-cores exclusively.

Note: the dispatcher was accidentally reverted to AVX2 > SSSE3 during the
Phase 33 module split and restored in 0.3.1.

- [x] `pub fn encode()` dispatches SSSE3 > scalar, skipping AVX2.
- [x] `encode_avx2` remains public for benchmarking and future phases.
- [x] Dispatcher comment explains the hybrid-CPU rationale.

Results after 27b:
  ll=128  encode (disp): **1797 MB/s** (1.50× nyuu) ✓
  ll=256  encode (disp): **2294 MB/s** (0.96× nyuu) — 4% gap remaining

### 27c — Benchmark and validate at line_len=256 *(low complexity)* ✅

- [x] `benches/yenc.rs` covers both ll=128 and ll=256 for all four paths.
- [x] nyuu reference (~1200 MB/s / ~2400 MB/s) printed after each section.
- [x] SSSE3 at ll=256 reaches 2294 MB/s — 96% of nyuu's documented target.

---

## Phase 28 — SSSE3 Loop Unrolling

### 28a — 2×16-byte unrolled inner loop *(low complexity)* ✅

At ll=128 the SSSE3 safe zone is 126 B → 7 single-chunk iterations per line.
At ll=256 the safe zone is 254 B → 15 iterations. Processing one 16-byte chunk
per iteration means 15 branch checks and 15 pointer/counter updates per line —
overhead that accounts for the ~4% gap to nyuu at ll=256.

Fix: add a `while safe_rem >= 32` unrolled loop before the existing
`while safe_rem >= 16`. Each iteration loads two independent `__m128i` chunks,
computes their escape masks in parallel (ILP), and takes a single combined
fast-path store when both masks are zero.

- [x] Add 2×16-byte unrolled loop in `encode_ssse3_impl` (inside safe zone).
- [x] Combined `mask_a | mask_b == 0` fast path: two consecutive `_mm_storeu_si128` writes without extra branching.
- [x] Slow path: handle each chunk individually (same logic as before).
- [x] All 243 tests pass (golden-reference suite unchanged).

Results after 28a:
  ll=128  encode (disp): **1865 MB/s** (1.55× nyuu) ✓
  ll=256  encode (disp): **2365 MB/s** (0.99× nyuu) — 1% gap remaining

### 27d — DEFAULT_LINE_LENGTH: evaluate raising to 256 *(closed — keep 128)*

`line_len=128` is historical (yEnc draft spec, 2001). nyuu also defaults to
128. No evidence of broad server/indexer acceptance of 256 as the standard.

**Decision:** keep `DEFAULT_LINE_LENGTH = 128`. The `--line-length` flag allows
opting in to 256, which gives ~2365 MB/s (0.99× nyuu). At ll=128 pesto already
reaches 1865 MB/s (1.55× nyuu), so the default is already comfortably ahead.

---

## Phase 33 — `yenc.rs` Module Split ✅

`src/yenc.rs` is 2 362 lines and contains four independent encoding backends
plus shared types, the public API, and the test suite — all in one file.
Split it into a `src/yenc/` module so each backend lives in its own file.

Proposed layout:

```
src/yenc/
  mod.rs        # public API (encode_part, segments, Crc32, PartSpec,
                #   EncodedPart), dispatch logic, encoded_size
  scalar.rs     # encode_scalar — portable fallback (~60 lines)
  x86.rs        # SSSE3 + AVX2 impls + encode() dispatcher for x86_64
  aarch64.rs    # NEON impl + encode() dispatcher for aarch64
  tables.rs     # SHUFFLE_TABLE, ADD_TABLE, LEN_TABLE (shared by x86 + NEON)
  tests.rs      # all #[cfg(test)] content (currently ~660 lines)
```

Acceptance criteria:
- [x] Convert `src/yenc.rs` → `src/yenc/mod.rs` and extract backends into
      `scalar.rs`, `x86.rs`, `aarch64.rs`.
- [x] Move `mod tests { … }` to `tests.rs` and reference it with
      `#[cfg(test)] mod tests;`.
- [x] No change to the public API surface (`pub use` in `mod.rs` if needed).
- [x] `cargo test` passes unchanged (same 243 tests).
- [x] `cargo clippy --all-targets -D warnings` clean.

---

## Phase 32 — Future Ideas (Unscheduled)

Concepts to evaluate later. Not committed to any timeline.

| Idea | Summary |
|------|---------|
| yEnc SIMD Escaping | Use PSHUFB to insert '=' escapes in-place without falling back to scalar. |
| yEnc Multi-line | Process multiple lines in parallel using AVX2 or AVX-512. |

## Phase 32 — yEnc Performance Parity ✅
Target: Match or exceed `node-yencode` throughput (> 2 GB/s on modern CPUs).
- [x] Identify performance gap vs `node-yencode` (~700 MB/s vs ~2000 MB/s).
- [x] Optimize the "slow path" (escaping) to avoid scalar fallbacks.
- [x] Implement SIMD expansion using shuffle tables (PSHUFB) for inserting '=' escapes.
- [x] Pointer-based implementation to eliminate Vec bounds checks.
- [x] Benchmarking and validation: **2.2 GB/s** achieved (exceeding `node-yencode`).

| RAM auto-cap | Cap buffer pools based on available system memory to prevent OOM |
| Dynamic connection scaling | Reduce connections under memory or TCP pressure |
| CPU topology awareness | Tune `rayon` pool to physical vs logical core count |
| Disk pre-flight | Verify free space before compression/PAR2 starts |
| In-memory mode | Skip temp files for small payloads that fit in RAM |
| `O_DIRECT` reads | Bypass page cache on Linux for huge files |
| `mmap` fast-path | `mmap` + `MADV_SEQUENTIAL` for massive file reads |
| Adaptive buffering | Grow/shrink buffer pool based on upload/read speed delta |
| Lock-free buffer pool | Replace `Mutex<Vec<_>>` pool with `SegQueue` to eliminate contention at high connection counts |
| Connection health scoring | Track per-server error rates passively; prefer healthy servers without hard failover |
| Warm reconnection | Pre-connect to the next failover server in background so TLS handshake cost is not paid on the hot path |
