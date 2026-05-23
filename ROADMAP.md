# Roadmap — `pesto`

Fast, lean Usenet poster in Rust. Inspired by `nyuu`, with only the essentials.
Each phase must leave the program in a working, testable state.

## Phase 0 — Foundation ✅

- [x] `cargo init` with `main.rs` + `lib.rs` (pesto-poster)
- [x] Basic CLI with `clap` (derive)
- [x] Structs for NNTP server config
- [x] Minimal logging with `tracing-subscriber`

## Phase 1 — yEnc Encoder ✅

- [x] Core `encode_into` function with CRC32 calculation
- [x] Unit tests for escaping (special characters, `.` at start of line)
- [x] Segment split logic (multi-part yEnc)
- [x] Article header/footer generation (`=ybegin`, `=ypart`, `=yend`)

## Phase 2 — Basic NNTP Protocol ✅

- [x] TCP stream connection
- [x] Text-based protocol parser (handshake, `POST`, `240` response)
- [x] Article transmission over TCP
- [x] Simple synchronous "post one file" proof of concept

## Phase 3 — Authentication & TLS ✅

- [x] `rustls` integration for secure connections
- [x] NNTP `AUTHINFO USER / PASS` implementation
- [x] Environment variable support for credentials
- [x] Verify SSL with standard providers

## Phase 4 — Concurrent Posting Engine ✅

- [x] Connection pooling (multi-threaded workers)
- [x] MPSC channel for work distribution (producer-consumer)
- [x] Basic progress bar (indicative)
- [x] Graceful shutdown (Ctrl-C)

## Phase 5 — NZB Generation ✅

- [x] XML writer for `.nzb` format
- [x] Capture Message-IDs from the engine
- [x] Capturing metadata (filename, size, groups)
- [x] Group segments into `<file>` entries

## Phase 6 — Config File Support ✅

- [x] `toml` configuration file (`config.toml`)
- [x] Merging logic (CLI flags override config file)
- [x] Configuration for servers, auth, and posting defaults
- [x] Support for multiple newsgroups

## Phase 7 — PAR2 Creation Foundation ✅

- [x] Algebra of Galois (GF(2^16)) implementation
- [x] Reed-Solomon Cauchy matrix generation
- [x] PAR2 packet serialization (Main, File Description, IFSC)
- [x] Volume layout (split into multiple `.par2` files)

## Phase 8 — Advanced PAR2 Features ✅

- [x] MD5 of files (Full and 16K)
- [x] Single-pass parity generation (while reading for yEnc)
- [x] Optimized SIMD (AVX2/SSSE3) for RS multiplication
- [x] Integration of PAR2 files into the `.nzb`

## Phase 9 — Local Archive & Obfuscation ✅

- [x] Local RAR/7z compression (calling external binaries)
- [x] Filename obfuscation (randomized filenames in headers)
- [x] Support for encrypted archives (password in metadata)

## Phase 10 — Metadata & Hooks ✅

- [x] `.nfo` file auto-generation
- [x] Post-hook support (execution of shell scripts after upload)
- [x] Newznab indexer integration (upload of NZB)
- [x] Notification support (Discord/Apprise webhooks)

## Phase 11 — Error Resilience & Resume ✅

- [x] Retry logic with exponential backoff for NNTP failures
- [x] Resume state file (`.pesto-state`) to continue interrupted uploads
- [x] Verification pass (deferred STAT checks)
- [x] Detailed error reporting and actionable logs

## Phase 12 — Performance & Buffer Management ✅

- [x] Double-buffered reader (read next segment while posting current)
- [x] Buffer pool to minimize allocations/GC pressure
- [x] Rayon integration for parallel PAR2 compute
- [x] Rate limiting support (token bucket)

## Phase 13 — Polish & UI ✅

- [x] Visual terminal panel (ANSI escape codes, multi-bar progress)
- [x] JSON-L output mode for integration with other tools
- [x] Interactive setup wizard (`pesto --config`)
- [x] Sparklines for throughput history

---

## Phase 20 — Codebase Modularization ✅

Reduce the size of monolithic files by splitting them into logical sub-modules. This
improves maintainability, reduces merge conflicts, and clarifies internal APIs.

### 20a — Split Setup Wizard from `main.rs` (Complexity: Low) ✅

- [x] Create `src/ui/wizard.rs` (or `src/config/wizard.rs`).
- [x] Move the interactive configuration wizard logic out of `main.rs`.
- [x] Clean up `main.rs` to focus strictly on CLI entry and orchestration.

### 20b — Separate TUI Rendering from `progress.rs` (Complexity: Medium) ✅

- [x] Create `src/ui/terminal.rs` for all ANSI/terminal rendering logic.
- [x] Keep `src/progress.rs` focused on the `ProgressEvent` definitions and
      event-bus logic.
- [x] Prepare the architecture for the future `ratatui` integration (Phase 21j).

### 20c — Isolate PAR2 Pipeline Worker from `poster.rs` (Complexity: Medium) ✅

- [x] Create `src/poster/par2_worker.rs`.
- [x] Move the `Par2Worker` struct and its complex multi-threaded pipeline logic
      (MD5 hashing + RS encoding coordination) out of the main poster module.
- [x] Simplify `src/poster.rs` to focus on the high-level upload loop.

### 20d — Modularize `config.rs` (Complexity: Medium) ✅

- [x] Convert `src/config.rs` into a module directory `src/config/`.
- [x] Split into `types.rs` (struct definitions), `parse.rs` (TOML/CLI merging),
      and `validation.rs`.
- [x] Reduce the 1300+ line count of the current single file.

---

## Phase 21 — PAR2 Library Separation ✅

Decouple the high-performance PAR2 encoder from the Usenet-specific logic. This
turns the core of `pesto` into a standalone asset that can be used by other
projects (like `upapasta` directly) and improves build times.

### 21a — Cargo Workspace setup (Complexity: Low) ✅

- [x] Convert the repository into a Cargo Workspace.
- [x] Move `src/par2` to its own crate directory (e.g., `crates/pesto-par2`).
- [x] Update `Cargo.toml` in the root to manage both the binary and the new crate.
- [x] Ensure `cargo test` and `cargo build` still work across the workspace.

### 21b — API Decoupling and Cleanup (Complexity: Medium) ✅

- [x] Remove all Usenet/NNTP/NZB specific terminology from the PAR2 crate.
- [x] Redesign the `RecoveryEncoder` API to be generic over any source of bytes
      (e.g., `std::io::Read` or a custom trait), not tied to the `pesto` reader.
- [x] Extract SIMD detection and dispatch logic into the library so it works
      standalone.
- [x] Provide a clean `prelude` or high-level API for third-party consumers.

### 21c — Performance Isolation and Benchmarking (Complexity: Medium) ✅

- [x] Move internal micro-benchmarks (`bench-internals`) into the library crate.
- [x] Ensure that moving the code doesn't introduce performance regressions due
      to cross-crate optimization boundaries (use `#[inline]` where necessary).
- [x] Add library-specific documentation and examples for standalone usage.

### 21d — Independent Publication (Complexity: High)

- [ ] Version the library independently from the `pesto` binary.
- [ ] Publish `pesto-par2` to crates.io.
- [ ] Update `pesto` (the binary) to depend on the published crate (or workspace
      path) instead of local modules.

---

## Phase 22 — Generic PAR2 Tooling Expansion

Transform `pesto-par2` and the `pesto` CLI into a general-purpose high-performance
PAR2 creation tool, matching the flexibility of `parpar`.

### 22a — Manual Resource Control (Complexity: Low)

- [ ] Add `--threads N` flag to manually override the Rayon pool size.
- [ ] Add `--memory-limit SIZE` CLI flag (overriding config) for multi-pass
      tuning.
- [ ] Expose internal SIMD path selection via `--simd [auto|avx512|avx2|ssse3|neon|scalar]`.

### 22b — Explicit Geometry Control (Complexity: Medium)

- [ ] Add `--slice-size SIZE` (or `--block-size`) to manually set the PAR2 slice
      size, bypassing the auto-selection logic.
- [ ] Add `--slice-count N` to target a specific number of input slices.
- [ ] Add `--recovery-count N` to specify the exact number of recovery blocks
      instead of a percentage.
- [ ] Validate manual inputs against PAR2 spec limits (32k/65k) and provide
      helpful errors.

### 22c — Advanced Volume & Output Mapping (Complexity: Medium)

- [ ] Add `--out-dir PATH` to specify where PAR2 files should be saved.
- [ ] Support `--filepath-format` style templates for naming recovery volumes.
- [ ] Implement volume-splitting schemes beyond the default (e.g., power-of-2
      sizes).

### 22d — Standalone `parmesan` CLI (Complexity: High)

- [ ] Create a dedicated minimal binary crate `crates/parmesan`.
- [ ] This binary focuses strictly on file parity, with zero network/Usenet
      dependencies.
- [ ] Use `parmesan` for direct performance comparisons with `parpar` and `par2cmdline`.

---

## Phase 23 — Interactive Visuals & UX (Ratatui)

Complete terminal user interface with interactive elements, real-time logging,
and better monitoring.

### 23a — Layout & Dashboard (Complexity: Medium)

- [ ] Implement the main dashboard layout with `ratatui`.
- [ ] Tabs for `Progress`, `Logs`, `Connections`, and `PAR2 Status`.
- [ ] Real-time throughput graph using Ratatui `Canvas` or `Sparkline`.

### 23b — Interactive Features (Complexity: High)

- [ ] Allow pausing/resuming the upload loop via keyboard.
- [ ] Dynamic adjustment of connection count during runtime.
- [ ] Scrollable log buffer with levels/search.

---

## Phase 32 — Future Ideas & Brainstorming (To Be Evaluated)

*A collection of concepts to improve resilience, extreme-environment performance, pipelining, visual feedback, and open-source composability. Kept here for future selection.*

### A. Extreme Environments & Resource Management
1. **Auto-Detect RAM limits:** Automatically cap buffer pools based on total system RAM to prevent OOM errors on low-memory machines.
2. **Dynamic Connection Scaling:** Reduce the number of active NNTP connections on-the-fly if memory pressure is high or TCP buffers fill up.
3. **CPU Topology Awareness:** Adjust the `rayon` thread pool dynamically based on available physical cores versus total logical CPUs.
4. **Disk Space Pre-flight:** Check if the temp directory has enough free space *before* starting heavy compression or PAR2 generation.
5. **In-Memory Mode:** For files smaller than available RAM, avoid writing temporary archives to disk completely (bypassing I/O bottlenecks).
6. **Direct I/O (`O_DIRECT`):** On Linux, bypass the OS page cache for huge files to prevent thrashing system memory.
7. **Memory Mapping (`mmap`):** Alternative fast-path for reading massive files using `madvise(MADV_SEQUENTIAL)`.
8. **Adaptive Buffering:** Grow or shrink `Shared::acquire_buffer` pools based on the delta between network upload speed and disk read speed.
