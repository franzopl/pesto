# Changelog

All notable changes to `pesto` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased]

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
