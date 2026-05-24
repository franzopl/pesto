# Changelog

All notable changes to `pesto` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased]

## [0.3.1] — 2026-05-24

### Added
- `bench/` benchmark suite: `yenc.sh`, `par2.sh`, `posting.sh` with shared
  `lib.sh`; produces copy-paste–ready Markdown tables and CSV results per host.
- `bench/yencode.js` moved from root into `bench/` (was `bench_yencode.js`).
- Performance section in `README.md` with yEnc and PAR2 throughput tables vs
  `node-yencode` and `parpar`.

### Changed
- `node_modules/`, `bench_*.sh`, `GEMINI.md` removed from git tracking.
- `.gitignore` updated to cover `node_modules/`, `bench/results/`,
  `bench/par2_out/`, and legacy `bench_*_out/` directories.

## [0.2.24] — 2026-05-23

### Fixed
- Optimized SIMD paths to avoid register spills, improving performance on Intel mobile CPUs (Raptor Lake) from 500 MB/s to 2.1 GB/s.

## [0.2.23] — 2026-05-23

### Performance
- **yEnc Performance Parity (>2.2 GB/s)**:
  - SIMD-accelerated yEnc escaping using `PSHUFB` expansion tables.
  - Refactored encoder to use direct pointer writes, eliminating vector bounds checks.
  - Optimized AVX2 path for 32-byte chunks.
  - Benchmarked to exceed `node-yencode` throughput (reaching ~2200 MB/s on modern CPUs).

### Added
- Comprehensive yEnc benchmark suite vs `node-yencode` (`bench_pesto_yenc_vs_node.sh`).

## [0.2.22] — 2026-05-23

### Changed
- **`parmesan` extracted as a versioned crate (`parmesan-par2 v0.1.0`)**: the
  PAR2 sub-crate now has its own `CHANGELOG.md`, `README.md`, and full
  crates.io metadata. Published independently as `parmesan-par2` (the name
  `parmesan` was already taken on crates.io).

## [0.2.21] — 2026-05-23

### Fixed
- Silenced `dead_code` warnings in `par2/encoder.rs` that appeared after the
  workspace restructure.

### Added
- `GEMINI.md`: Gemini-specific agent guide for contributors using the Gemini
  CLI.

## [0.2.20] — 2026-05-22

### Performance
- **Shuffle2x AVX2 path (`flush_avx2_shuffle2x_work`)**: new default AVX2
  dispatch replaces the plain nibble-shuffle kernel; measured improvement on
  i5-10400 and Intel Ice Lake.
- **8-way unroll for AVX2+GFNI / 4-way for AVX-512+GFNI** (Phase 25e):
  reduces loop overhead on wide SIMD paths.
- **Background flush worker** (Phase 25f): PAR2 I/O no longer blocks the
  encoder pipeline.
- **`--par2-only` bypass** (Phase 25g): article pipeline is skipped entirely
  when only PAR2 generation is requested, cutting startup latency.
- Fixed 210 ms fixed delay on startup caused by `sysinfo::System::new_all()`
  — replaced with a scoped, lazy call.

## [0.2.19] — 2026-05-22

### Changed
- **AVX-512+GFNI PAR2 path enabled in production**: `flush_avx512_gfni` is now
  active by default on CPUs that report AVX-512F + AVX-512BW + GFNI (Intel Ice
  Lake and newer). Previously gated behind the `par2-avx2-gfni-unsafe` feature;
  validated via `gfni_recovery_matches_scalar` on Intel Ice Lake Xeon (AWS m6i).

## [0.2.18] — 2026-05-22

### Added
- **`--no-hooks`**: skip all post-upload hooks for a single run (both
  `--post-hook` commands and scripts in `~/.config/pesto/hooks/`).
  Useful when testing or reposting without triggering indexer notifications.

### Fixed
- **Season NZB folder name**: the consolidated season `.nzb` now includes
  `<meta type="name">` set to the season stem (e.g.
  `Show.S01.1080p.WEB-DL`) so that SABnzbd and NZBGet name the download
  folder after the season pack instead of the first episode.

## [0.2.17] — 2026-05-21

### Added
- **Verbose mode & diagnostics (Phase 26)**
  - `-v` / `--verbose` flag (repeat for more detail: `-v` = INFO, `-vv` = DEBUG,
    `-vvv` = TRACE); backed by `tracing` + `tracing-subscriber` with `RUST_LOG` override
  - `--log-file FILE` redirects log output to a file; terminal panel stays active
  - System info logged at startup: OS, architecture, CPU features (AVX2+GFNI, SSSE3, NEON)
  - NNTP network trace at DEBUG/TRACE: every command sent and response received,
    with per-command round-trip time in milliseconds
  - TLS handshake and server greeting logged at DEBUG
  - Worker pool events at INFO: connecting, authenticated, connection invalidated
  - Upload plan at INFO: file count, segment count, PAR2 geometry, connection pool size
  - Reed-Solomon SIMD path selection logged at INFO (`simd=avx2+gfni threads=6`)
  - Retry and failover decisions logged at WARN with attempt/error details
  - Compression command logged at DEBUG with password arguments masked as `[MASKED]`
  - Per-phase timing: each phase emits `elapsed_ms` + `phase` on completion
    (compress, par2_compute, par2_write, post, check)
  - One-line timing summary at upload completion
  - Network performance summary: segments posted, failed, total retries
  - Terminal panel suppressed automatically at `-vv` when logs share stderr
  - Credentials never appear in log output (`AUTHINFO PASS [MASKED]`)
- **README**: SIMD acceleration table added under PAR2 recovery data section,
  with dispatch chain, benchmark numbers and `--features bench-internals` usage

## [0.2.16] — 2026-05-20

### Added
- **High-Performance PAR2 Encoder**:
  - Implemented AVX-512 + GFNI, AVX2, and SSSE3 optimized paths for Reed-Solomon encoding.
  - 2D parallelization (chunks × recovery blocks) with hybrid CPU detection.
  - Hardware-accelerated CRC32 and memory prefetching.
- **Terminal UI Restructure**:
  - New multi-panel layout with dedicated PAR2 encoding progress bars.
  - Grouped status matrix and sparkline throughput history.
  - Detailed final upload summaries (average speed, total elapsed time).

### Fixed
- Corrected recovery data generation in AVX2/GFNI paths.
- Fixed progress counter resets between files.
- Improved ETA stability by clamping high-bound ranges.

## [0.2.5] — 2026-05-19

### Fixed
- `rust-version` corrected from `1.75` to `1.87` (code already used APIs
  stable since 1.87; clippy MSRV check was failing in CI)
- Removed useless `PathBuf::from()` wrapping in `config.rs` history_dir map
  (clippy `useless_conversion` warning)
- Applied `cargo fmt` to `config.rs`, `history.rs`, `main.rs`, `progress.rs`
  (CI format check was failing)

### Docs
- Published to crates.io as `pesto-poster` (binary name remains `pesto`)
- README: rust-version badge updated to 1.87+; `cargo install pesto-poster`
  documented in Installing section

### Changed
- History catalog location decoupled from the `upapasta` directory; configurable
  via `output.history_dir` (default: `~/.config/pesto`; set to
  `~/.config/upapasta` to share with upapasta).

### Docs
- `config.example.toml` now documents all implemented sections and fields
  (`[notify]`, `[output.indexer]`, `posting.date`, `posting.no_archive`, etc.).
- README: new Prerequisites section, expanded All Flags table (9 missing flags
  added), Installing section with pre-built binary links.
- ROADMAP: Phase 22 (public release preparation) added.

---

## [0.2.4] — 2025

### Added
- **Phase 21 — Terminal UX overhaul**
  - Smooth sub-character progress bars (`▏▎▍▌▋▊▉█`).
  - Color-coded connection status matrix (green = uploading, yellow =
    authenticating, red = retrying, grey = idle). Suppressed when `NO_COLOR`
    is set or stderr is not a TTY.
  - Sparkline throughput history (10-sample rolling graph).
  - Confidence-based ETA: single value when throughput is stable; range with
    instability marker (`~`) when it fluctuates.
  - Directory tree preview printed before upload starts.
  - `--quiet` / `-q` flag and `output.quiet` config key for single-line
    minimal output (spinner + percentage + ETA).
  - `--bell` flag and `output.bell` config key — writes ASCII BEL on
    completion.
  - Buffer pool visualizer in the panel (shown only under memory pressure).
  - Adaptive panel refresh rate: backs off from 200 ms to 500 ms when
    rendering is slow.

---

## [0.2.3] — 2025

### Fixed
- Password not propagated to the consolidated season NZB when using
  `--season --password`.

---

## [0.2.2] — 2025

### Added
- Pre-built binaries for Linux (glibc and musl) and Windows via GitHub Actions
  release workflow.

### Fixed
- NZB/NFO filename truncating release tag on re-post (versioned suffix
  `.v2.nzb` instead of `.nzb.v2.nzb`).
- Race condition in obfuscated directory tests.

---

## [0.2.1] — 2025

### Fixed
- NZB/NFO filename truncating release tag when the upload name contained dots.

---

## [0.2.0] — 2025

### Added
- **Phase 18 — Post-upload hooks and NFO generation**
  - `--nfo` flag and `output.nfo` config key: generates a `.nfo` text file
    using `mediainfo` (optional) or a directory listing fallback.
  - `--post-hook <CMD>` flag and `output.post_hook` config key: run a shell
    command after each successful upload with `PESTO_*` environment variables.
  - Hooks directory: any executable in `~/.config/pesto/hooks/` runs
    automatically after each upload.
  - Bundled examples: `print-vars.sh` and `curupira.sh`.
- **Phase 16 — Observability**
  - Per-phase progress panel covering compression, PAR2 computation, and
    PAR2 volume writing — not only the posting step.
  - `--output-format json` (`--no-nfo` accepted as no-op for `upapasta`
    compatibility).
  - Upload history log written to `~/.config/pesto/history.jsonl` (same format
    as upapasta's catalog); NZB archived to `~/.config/pesto/nzb/`.
    `--history` / `--no-history` flag and `output.history` config key.
  - Completion notifications via `[notify]` config section: Discord/Slack/
    Telegram webhooks and ntfy.sh. `--notify` / `--no-notify` flags.
- **Phase 15 — NZB metadata**
  - `--nzb-name`, `--nzb-password`, `--nzb-category` flags and corresponding
    `output.*` config keys; written as `<meta>` elements in the `.nzb`.
  - Automatic NZB upload to Newznab-compatible indexers via `[output.indexer]`
    config section. `--no-upload` flag skips it for a single run.
- **Phase 14 — Posting features**
  - `--date` flag and `posting.date` config key (`now`, `random`, RFC 2822).
  - `--no-archive` flag and `posting.no_archive` — adds `X-No-Archive: yes`.
  - `--message-id-domain` flag and `posting.message_id_domain`.

### Fixed
- `--obfuscate=full` leaking real filenames in NZB and NFO.
- Spurious "rar" note printed even when compression was not in use.
- NZB `<head>` block always emitted (was missing when no metadata was set).

---

## [0.1.0] — 2025 — MVP

### Added
- **Phase 0** — Project scaffold: `main.rs`, `lib.rs`, module skeleton,
  `clap` CLI, TOML config loading, basic CI.
- **Phase 1** — yEnc encoder following the specification (escaping, CRC32,
  `=ybegin` / `=ypart` / `=yend` lines). Tests against known vectors.
- **Phase 2** — NNTP client: TCP + TLS (`rustls`), `AUTHINFO`, `POST` command,
  article assembly with `Message-ID` generation.
- **Phase 3** — Parallel posting: pool of N concurrent TLS connections,
  work queue, retry on failure, progress bar.
- **Phase 4** — `.nzb` generation from posted `Message-ID`s.
- **Phase 5** — MVP polish: actionable error messages, Ctrl-C / clean
  shutdown, integration test with mock NNTP.
- **Phase 6** — Posting obfuscation: `--obfuscate` flag with `none`,
  `subject`, and `full` modes.
- **Phase 7** — PAR2 generation: pure-Rust GF(2^16) Reed-Solomon encoder,
  PAR2 packet format, streaming encoder computing parity in the same pass as
  posting. AVX2 SIMD acceleration with scalar fallback. `--par2 <percent>` and
  `--par2-only` flags.
- **Phase 8** — Configuration UX: XDG config path, random `From` identity by
  default, interactive setup wizard (`pesto --config`), orientation screen.
- **Phase 9** — Directory uploads: recursive walk, structure-preserving PAR2
  and NZB, obfuscation for directories.
- **Phase 10** — `upapasta` integration: stable `lib.rs` API, JSON event
  stream for subprocess integration.
- **Phase 11** — Reliability: multiple NNTP servers with failover
  (`[[servers]]`), upload resume (`.pesto-state` sidecar), post-verification
  via `STAT`, rate limiting.
- **Phase 12** — Performance: double-buffered I/O, buffer pool.
- **Phase 13** — Compression: `--compress` (7z/zip/rar via external tools),
  `--password` (random or explicit); password stored in NZB metadata.
- **Phase 14-pre** — Batch and watch modes: `--each`, `--season`, `--jobs`,
  `--watch`.
- **Phase 19** — Test coverage: unit tests for all modules; mock-NNTP
  integration tests for retry and resume logic.

[Unreleased]: https://github.com/franzopl/pesto/compare/v0.2.5...HEAD
[0.2.5]: https://github.com/franzopl/pesto/compare/v0.2.4...v0.2.5
[0.2.4]: https://github.com/franzopl/pesto/compare/v0.2.2...v0.2.4
[0.2.3]: https://github.com/franzopl/pesto/compare/v0.2.2...v0.2.3
[0.2.2]: https://github.com/franzopl/pesto/compare/v0.2.1...v0.2.2
[0.2.1]: https://github.com/franzopl/pesto/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/franzopl/pesto/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/franzopl/pesto/releases/tag/v0.1.0
