# Roadmap — `pesto`

Fast, lean Usenet poster in Rust. Inspired by `nyuu`, with only the essentials.
Each phase must leave the program in a working, testable state.

## Phase 0 — Foundation ✅

- [x] `cargo init` with `main.rs` + `lib.rs`
- [x] Module skeleton (`config`, `yenc`, `nntp`, `article`, `nzb`)
- [x] CLI parsing with `clap`
- [x] TOML config loading + merge with flags
- [x] Basic CI (`fmt`, `clippy`, `test`)

## Phase 1 — yEnc encoding ✅

- [x] yEnc encoder following the specification (escaping of `=`, NUL, CR, LF)
- [x] `=ybegin` / `=yend` lines with CRC32
- [x] File segmentation into parts (`=ypart` for multi-part)
- [x] Tests against known yEnc vectors

## Phase 2 — NNTP client ✅

- [x] TCP + TLS connection (`rustls`) on port 563
- [x] Handshake and `AUTHINFO USER/PASS` authentication
- [x] `POST` command (article upload, handling of 240/441 responses)
- [x] Article assembly: headers (`Subject`, `From`, `Newsgroups`,
      `Message-ID`) + yEnc body
- [x] Unique `Message-ID` generation per segment

## Phase 3 — Parallel posting ✅

- [x] Pool of N concurrent TLS connections (`tokio`)
- [x] Work queue: segments distributed across connections
- [x] Retry of failed segments
- [x] Progress bar / throughput in the terminal
- [x] Flags `--connections`, `--ssl`, `--groups`

## Phase 4 — `.nzb` generation ✅

- [x] Collect `Message-ID`s, sizes and CRCs of posted segments
- [x] Write a valid `.nzb` XML file (nzb DTD)
- [x] Flag `--out` for the `.nzb` path

## Phase 5 — MVP polish ✅

- [x] Actionable error messages (network, auth, I/O)
- [x] Ctrl-C handling / clean shutdown
- [x] `README` with usage examples
- [x] End-to-end integration test (mock NNTP)

**The MVP is complete.** `pesto` posts files to Usenet and writes an `.nzb`.

## Phase 6 — Posting obfuscation ✅

- [x] `--obfuscate` flag and `obfuscate` config option
- [x] Random subject and yEnc file name per file
- [x] Real file name preserved in the `.nzb` `<file name>` attribute
- [x] Tests for obfuscated-name generation and `.nzb` output

## Phase 7 — PAR2 generation

Own pure-Rust PAR2 creator — no `par2cmdline` / `parpar` dependency. Parity is
computed in the *same single read pass* used for posting: each slice, as it is
read and yEnc-encoded for upload, is also accumulated into the Reed-Solomon
recovery buffers. The PAR2 slice size is aligned with the yEnc article size,
so one read block is one article and one input slice.

### 7a — GF(2^16) field and Reed-Solomon matrix

- [x] GF(2^16) arithmetic (generator `0x1100B`), log/antilog tables
- [x] PAR2 input-constant and recovery-exponent generation, bit-exact with
      `par2cmdline`
- [x] Tests cross-checked against known `par2cmdline` constants

### 7b — PAR2 packet format

- [x] Packet framing, MD5 packet hashes, recovery set ID
- [x] Main, File Description, Input File Slice Checksum, Recovery Slice and
      Creator packets
- [x] Volume-split layout (index + `volNNN+MMM` files, exponential counts)

### 7c — Streaming Reed-Solomon encoder

- [x] Accumulate input slices one at a time into N recovery buffers
- [x] Per-slice MD5 + CRC32 and per-file MD5 computed while streaming
- [x] Validate generated PAR2 with `par2cmdline` (verify + repair)

### 7d — Pipeline integration

- [x] Refactor the poster into a single-reader producer feeding the posting
      pool through a bounded channel
- [x] Compute parity during the read pass; post PAR2 articles after the data
- [x] Include the PAR2 files in the `.nzb`
- [x] `--par2 <percent>` flag and config option; 10% default

### 7e — Performance ✅

- [x] SIMD GF multiply (AVX2 `pshufb` GF(2^16)), scalar fallback
- [x] Recovery buffers partitioned across threads (`rayon`)
- [x] `block_in_place` so the CPU-bound encoder does not stall the runtime

### 7f — Generation modes ✅

- [x] `--par2-only` flag: write parity files next to the source, no posting
- [x] `--dry-run` flag: process files without touching the network

## Phase 8 — `upapasta` integration

- [ ] Stabilize the public API of `lib.rs`
- [ ] Document integration points
- [ ] Adapt the `upapasta` posting flow to use `pesto`

## Post-MVP (future ideas)

- Compression / RAR creation before posting
- Resume of interrupted posts
- Rate limiting
- Multiple servers / failover
