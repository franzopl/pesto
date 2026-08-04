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

---

## Phase 40 — UpaPasta v2 (Rust Rewrite)

**Goal:** Replace the legacy Python version of UpaPasta with a pure Rust implementation using the `pesto` library directly. Focus on excellent UX while leveraging the performance of the Rust engine.

### 40a — Monorepo & Foundation ✅

- [x] Convert repository to Cargo workspace (`crates/pesto`, `crates/parmesan`, `crates/upapasta`)
- [x] Move existing `pesto` code into `crates/pesto/`
- [x] Create `crates/upapasta` with initial TUI skeleton (`ratatui` + `crossterm`)
- [x] Update `CLAUDE.md` with new architecture and development practices
- [x] Refine public API in `pesto`: `post_cancelable(config, files, Arc<AtomicBool>)` — cancellation properly propagates into pesto workers instead of only watching Ctrl-C

### 40b — TUI Core (In Progress)

**Current focus.** Build a clean, responsive, keyboard-driven interface.

- [x] Implement main `App` state machine with multiple screens (Dashboard, Browser, History, Config)
- [x] Create reusable components: `FileTree`, `UploadQueue`, `StatusBar`, `LogPanel`
- [x] Event-driven architecture using `crossterm` event stream + `tokio::sync::mpsc`
- [x] Real-time progress rendering from `pesto::post()` events (full `ProgressEvent` stream)
- [x] Basic navigation and keyboard shortcuts (`q`, `j/k`, `Enter`, `Tab`, `u`, `h`, Backspace, etc.)
- [x] Scrollable & navigable LogPanel (↑/↓, PgUp/PgDn, auto-scroll toggle, search/filter with `/`)
- [x] Live visual progress: accurate segment/byte tracking + speed + ETA from structured ProgressUpdate
- [x] Throughput sparkline (ratatui Sparkline widget) showing recent speed history
- [x] Per-file Gauge progress bars (color-coded by status: pending/active/done/failed)
- [x] Upload controls: cancel current upload (`x` key) using `CancellationToken` — now fully propagated to pesto workers
- [x] Pause/resume upload (`p` key) - UI + stats freeze (full worker pause pending pesto API)
- [x] Queue management: remove items (`d`/Del), clear queue (`c`), reordering (Shift+J/K)
- [x] Graceful error display: ERROR/WARN lines in red/yellow, UploadError event to status bar
- [x] Responsive layout: compact mode < 20 lines, "too small" guard < 40×10
- [x] **Bulk multi-select in Browser**: Space marks/unmarks items with `[x]`/`[ ]` checkboxes; cursor advances; count shown in title and status bar
- [x] **Browser split layout**: queue panel sidebar (35%) appears alongside file tree when queue is non-empty
- [x] **Upload confirmation modal**: `u` opens overlay with files + effective settings (server, groups, from, PAR2, obfuscate, compress, verify); Enter/y confirms, Esc/n cancels
- [x] **Pipeline phase indicator**: progress section shows `Compress → PAR2 Gen → Upload → Verify` tracker with per-phase detail driven by real pesto ProgressEvents
- [ ] Theme support (dark/light + user-configurable colors)
- [ ] Directory-level queuing: Space on a directory marks all files inside recursively
- [ ] Auto-switch to Dashboard when upload starts so user sees progress without pressing Tab
- [ ] Pause support: real worker suspension (requires pesto API — currently only freezes UI stats)

### 40c — Catalog & Persistence

- [x] Persistent catalog (SQLite via `rusqlite` bundled, `~/.local/share/upapasta/catalog.db`)
- [x] Import history from legacy Python JSONL (auto-import on first run, 3914 records)
- [x] Search, filtering and statistics views (History tab: `/` to filter, `s` for stats panel)
- [x] Record each upload to catalog on completion (name, size, duration, group, server, category)
- [x] NZB archive viewer (Enter on History item → overlay popup with file list, segments, bytes; Esc to close)

### 40d — Orchestration & Feature Parity

- [ ] Watch mode with smart rules and move-to-done logic
- [ ] Metadata enrichment (TMDb, improved NFO generation)
- [x] Basic real config loading on startup (from default pesto path)
- [x] Clear visibility of effective upload settings before upload: obfuscation mode, compression+password, PAR2 %, groups, From, article size, verify — shown in Dashboard when queue has files + logged on upload start
- [x] Full configuration UI / editing + profile support (override obfuscation, PAR2, compression etc. from TUI — Config tab with per-session overrides for from, groups, obfuscate, PAR2, article size, verify, passwords; applied at upload time)
- [x] Post-upload hooks (shell + native Rust) — runs config.post_hook via sh -c + executables in ~/.config/pesto/hooks/; same PESTO_* env vars as pesto CLI; output streamed to log panel
- [x] **Persistent upload preferences** — session overrides (obfuscation, PAR2 %, password, groups) saved to `~/.config/pesto/upapasta-prefs.json` on every confirmed upload; reloaded at startup so the confirm panel is pre-filled with last-used settings
- [ ] Wizard for first-time setup

### 40e — Polish, Testing & Release

- [ ] Comprehensive error handling and user feedback
- [ ] Migration path from Python version
- [ ] Performance tuning of TUI during long uploads
- [ ] Build portable binaries
- [ ] Update documentation, man pages and README
- [ ] Retire or archive the old Python codebase

**Pre-commit checklist for upapasta:**

```bash
cargo fmt --all
cargo clippy --all-targets -- -D warnings
cargo check -p upapasta
cargo test -p upapasta
```

---

### Phase 44 — Full pipeline delegation to `pesto::upload::run_upload` ✅

Previously upapasta called `pesto::poster::post_files_with_progress_and_cancel`
directly and reimplemented post-upload steps incompletely (broken NZB write,
no compression, no history, no NFO, broken hooks). This phase centralised
everything in a new `pesto::upload::run_upload()` public API.

- [x] Created `crates/pesto/src/upload.rs` with `run_upload()` — canonical full pipeline: compress → PAR2 → post → NZB write (versioned) → history → notifications → NFO → hooks → cleanup temp dir
- [x] Moved `upapasta/src/hooks.rs` to `pesto/src/hooks.rs` (now public in pesto lib)
- [x] Added `pesto::upload::UploadOutcome` return type with segments, cancelled, had_failures, nzb_path, total_bytes
- [x] Fixed progress bar frozen at 99%: send 100% on `Finished` event without setting `events_done`; post-upload `Status` events (NZB path, hook output) continue to stream to log panel
- [x] Fixed PESTO_NZB always empty: NZB path now falls back to `./stem.nzb` when no `nzb_dir` configured
- [x] Added NFO generation in pipeline (was absent from upapasta path)
- [x] Added post-check STAT pass (`config.check`) after posting — matches pesto CLI behaviour
- [x] `ObfuscateMode` gains `Serialize` for JSON round-trip

---

### Phase 45 — NZB Vault UX improvements ✅

- [x] **Browser scroll fix** — cursor now moves within the visible area before scrolling; pressing ↑ from the last item moves the highlight up the screen without scrolling, fixing the counter-intuitive bounce effect
- [x] **NFO privacy** — `mediainfo` output now has the full filesystem path replaced with just the basename before writing the `.nfo` file
- [x] **NZB vault segregation** — three origin categories with automatic subdirectory creation:
  - `nzb_dir/uploaded/`   — NZBs created by upapasta uploads (badge `↑` cyan)
  - `nzb_dir/downloaded/` — NZBs fetched from Prowlarr/indexers (badge `↓` yellow)
  - `nzb_dir/` and any other subdirectory — manually placed NZBs (badge `m` gray)
- [x] **Fully recursive vault scan** — `collect_nzbs_recursive()` walks all subdirectories at any depth; origin derived from immediate parent folder name

### Phase 46 — Obfuscation overhaul (v0.3.18) ✅

- [x] **NZB filename fix** — `--obfuscate=full` was writing a random name to the `name=` attribute in the `.nzb`, breaking download clients without PAR2. The NZB now always carries the real filename; only the wire subject and yEnc `name=` are obfuscated.
- [x] **NZB groups fix** — the NZB listed all configured groups instead of only the one actually used for posting. `PostOutcome.groups` now surfaces the real posted group.
- [x] **`ObfuscateMode::Subject` removed** — the half-obfuscation mode that only hid the subject line (not the yEnc `name=`) was misleading and has been dropped.
- [x] **Schizo-style randomisation** — obfuscated subjects and yEnc names are now variable-length (10–30 chars) alphanumeric strings (`[A-Za-z0-9]`) instead of a fixed 32-char hex string, eliminating the fingerprint that made obfuscated posts identifiable.
- [x] **Random TLD in `From` header** — the generated sender domain uses a random 2–5 character alphabetic TLD instead of a fixed list of real TLDs.
- [x] **Per-file `From` rotation in `full` mode** — each file in a batch gets a distinct random sender address.
- [x] **`--obfuscate=paranoid` (experimental)** — every individual article gets a unique subject and `From` header, making segment grouping by wire metadata impossible. Hidden from `--help`; requires the NZB to download.

---

## Deferred / Intentionally Not Implemented

### Connection pool reuse across `--each` episodes

**Status:** Deferred — architectural refactor required.  
**Context:** Issue #35 (Case D). In `--each` mode, each episode builds a fresh connection pool
(30 connections × post + check phases = 60 TLS+AUTH cycles per episode). For a 26-episode run
this produces ~1,560 TLS handshakes (~6 min overhead at 230 ms/cycle, ~18% of runtime).

**Root cause:** the connection pool is owned by worker tasks that send `QUIT` when each
episode's channel closes. Reusing connections across episodes requires decoupling the pool
lifecycle from the episode lifecycle so workers can hold open connections and receive a new
work channel for the next episode.

**Complications:**
- `--jobs N` runs episodes in parallel with independent pools — sharing adds contention.
- Each episode creates its own `Shared` struct (buffer pool, cancel flag, result store);
  connections are embedded in workers that are tightly coupled to that `Shared`.
- A "connection broker" layer would centralise pool management but is a significant refactor
  of the hot path.

**Benefit when implemented:** reduces TLS handshakes from ~60/episode to ~30 for the entire
sequential run; eliminates most TLS+AUTH overhead in `--each` mode.

**Note:** the keepalive mechanism (Phase 35, `[server] keepalive`) already eliminates the
*spurious* reconnect bursts caused by server idle timeouts. Case D is about the *intentional*
teardown/rebuild, which remains.

---

### Subject file counter `[N/M]`

**Status:** Implemented as `--file-counter` / `[posting] file_counter`, on by default for `--obfuscate none` and
`full-shared`, off by default for `full`/`paranoid`. `--no-file-counter` (or `file_counter = false`/`true`
explicitly in the config file) overrides the mode-based default either way.

**Background:** Tools like `nyuu` prefix subjects with `[filenum/total]` (e.g. `[1/5] "movie.mkv" yEnc (1/3)`) — a
release-wide file counter, distinct from the per-file segment counter `(part/total)` that `pesto` always emits.
This entry used to say the total file count (data files + PAR2 index + volumes) couldn't be known before the
first article posts, since PAR2 files are generated asynchronously and appended to the queue after data files
begin posting — making a two-pass approach (compute PAR2 first, then post) seem necessary and too costly.

That rationale conflated two different things: *computing the PAR2 recovery-set geometry* (how many volumes
`plan_volumes` will produce) versus *actually running the Reed-Solomon encoder over file content*. Only the
second is expensive and must stay asynchronous. The first is pure metadata — file sizes plus config
(`--article-size`/`--par2`/`--slice-size`/`--recovery-count`), no I/O — and `pesto` already computed it up front
for the progress bar (`par2_geometry`, kept deliberately in sync with the encoder's own geometry calculation in
`producer`). Since `parmesan::layout::plan_volumes` is a pure function of the recovery-block count, the exact
number of volume files (and hence the release's grand total file count) is knowable before the first byte of
the first file is ever posted — no two-pass encode needed.

**Implementation:** `--file-counter` computes `total_files = data_files + 1 (PAR2 index) + plan_volumes(recovery_count).len()`
once, before any worker spawns (`post_files_with_progress_and_cancel`), and assigns each file a stable
`file_index` (1-based release position: data files in final posting order, then the PAR2 index, then the
volumes in `plan_volumes` order). Both values are denormalized onto `FileMeta`, `PostedSegment` and `FailedTask`
so every place that rebuilds a subject — the main post path, `--check` reposts, the end-of-run retry pass, and
`.nzb` generation — produces the identical `[filenum/total]` prefix. Toggling it (explicitly or via a changed
`--obfuscate` mode) between runs of the same `--resume` state invalidates the state (`resume::RunFingerprint`),
the same way `--obfuscate`/`--par2` changes already do, since it changes every subject in the release.

**Why the default is split by obfuscation mode:** `full` and `paranoid` exist specifically to prevent an
observer from correlating files/segments by wire metadata (independently-random names/`From` per file, or per
article for `paranoid`) — a stable, sequential `[filenum/total]` shared across the whole release is exactly the
kind of correlation vector those modes are designed to deny, so the counter stays off there by default. `none`
and `full-shared` already accept that correlation as part of their own design (a bare real filename, or a shared
prefix/`From` across the release) — see `full-shared`'s doc comment, which trades away `full`'s per-file
isolation for indexer compatibility in the first place — so adding the counter introduces no new correlation
vector for them, and it's on by default there.

Whether any indexer's grouping heuristic actually keys off the counter is otherwise unconfirmed either way — see
GitHub issue #68, which was closed as indexer-side (NZBIndex/Binsearch key their "complete set" grouping off the
`.volNNN+MMM.par2` filename pattern, not the subject's file counter) and re-closed after a live `--file-counter`
test against NZBIndex showed no change in that specific grouping gap. The flag stays available (and now on by
default for `none`/`full-shared`) for compatibility with any other indexer/tool that does read it.

**Update — the actual grouping gap was found, and it's unrelated to `--file-counter`:** the bare PAR2 index file
(and any other single-segment file posted alongside multi-segment siblings) had a subject shaped differently
from the rest of the release — `"name" yEnc`, no `(part/total)` trailer, per the yEnc spec's own allowance for
omitting it on a single-article file. `default_subject()` now always emits `(part/total)`, including `(1/1)`.
Confirmed live against Binsearch: a `--compress`/`--par2` release went from 8 of 9 files grouped (bare `.par2`
excluded) to 9 of 9 once every file's subject had the same shape. See the CHANGELOG `[Unreleased]` entry for
issue #68.

**Follow-up — a second, size-correlated symptom, and `--par2-before-upload`:** issue #68 was reopened again after
reports that large releases (~40 GB+) intermittently fail to group even with the subject-shape fix above, in a
different way (a data file or volume left out, not just the bare index). `producer` (`crates/pesto/src/poster/mod.rs`)
splits PAR2 generation into multiple read passes over the source files whenever the recovery data needed exceeds
`--memory-limit`; only the first pass posts data articles, so on a memory-constrained host the later passes — pure
re-reads, nothing posted — can open a real wall-clock gap between the release's last data article and its last PAR2
article. Confirmed the mechanism itself fires as documented with a local, network-free `--dry-run` test (small file,
tiny `--memory-limit` to force several passes): `split into N passes` appears in the log, and each extra pass posts
nothing.

Checked the open-source indexer software this ecosystem is built on (nZEDb → NNTmux/newznab-tmux) for a concrete,
sourced mechanism a gap like this could trip: `ProcessReleasesSettings` (`app/Support/Data/ProcessReleasesSettings.php`)
defines `collectionDelayTime` (DB key `delaytime`, default **2 hours**) — not a fixed deadline from the first article,
but an *idle-gap* threshold (`ReleaseProcessingService.php`: `COALESCE(last_seen_at, dateadded, added) < now() -
delaytime hours`). A collection that goes quiet for longer than that is finalized with whatever arrived so far;
anything posted after is a new collection — matching [NNTmux/newznab-tmux#1227](https://github.com/NNTmux/newznab-tmux/issues/1227),
the same bug class already linked above. Multi-pass PAR2 generation can produce exactly that kind of idle gap. As
before, NZBIndex/Binsearch's own codebase is unconfirmed (proprietary) — this is precedent from an open-source
indexer in the same family, not proof for those two specifically.

Rather than ask users to tune `--memory-limit` per release, `--par2-before-upload` (`posting.par2_before_upload`,
default off) makes pesto avoid the gap itself: when set, `producer` suppresses all data-article and PAR2-file
posting during the encode pass(es) — every pass just reads and encodes — then posts the data files followed by the
already-generated index/volumes once generation is fully done, so the whole release goes out back to back with no
gap regardless of how many passes generation needed. The default (interleaved) pipeline is unchanged for the common
single-pass case. No special `--resume` handling was needed: `producer` already regenerates PAR2 deterministically
from scratch on every resumed run (same bytes, given the same input/config and the already-reused `archive_stem`/
`release_prefix`), and the existing per-article resume check at the worker level — not `producer` — is what skips
already-posted segments, generically, regardless of when in the run they get queued.

References:
- yEnc draft v1.3: <http://www.yenc.org/yenc-draft.1.3.txt>
- Mirror: <https://github.com/caronc/newsreap/blob/master/docs/yenc-draft.1.3.txt>
