# Changelog — parmesan

All notable changes to `parmesan` are documented here.
Format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

---

## [Unreleased]

## [0.5.2] — 2026-08-20

### Fixed
- Clippy `-D warnings` and aarch64 build: `flush_avx512_affine2x` is gated on `x86_64` (it called a work function that only exists on that arch), `Md5State` copies no longer use `clone`, and constant-size `chunks_exact` loops use `as_chunks`.

## [0.5.1] — 2026-08-20

### Performance

- **Affine512 packed kernel** (`srcCount=6`, 4 KiB tiles) is now the `smart` default on AVX-512+GFNI hardware. PAR2 create on `c7i.2xlarge movie-1080p`: 325 → 518 MiB/s after dynamic batching.
- Affine AVX-512+GFNI and Affine AVX2+GFNI kernels; Shuffle AVX-512 nibble fallback; dynamic batch sizing (12 slices Affine512, 64 Shuffle2x/Normal).
- SIMD MD5-MB 8/16-wide (`md5-many`) for slice and file checksums.

## [0.5.0] — 2026-08-18

### Added
- **`ops::ingest_files_with`** — same ingestion as `ingest_files`, with optional cancel-at-file-boundary
  and an `after_file` hook so callers such as `pesto` can drive progress without reimplementing the reader.
- **`RecoveryDecoder::reconstruct` now parallelises across missing slices with `rayon`**, the same pattern
  `RecoveryEncoder` already used for creation. Sub-linear but real: `mac()` over a slice is bandwidth-bound, so
  threads contending for memory bandwidth don't scale linearly, but wall-clock repair still improved 20% on a
  large single file and 53% on many small files in the reference benchmark — `many-small` repair now beats
  `par2cmdline` instead of losing to it. Every reconstructed slice is still re-verified against its checksum
  before writing, so a correctness bug here would have surfaced as a hard failure, not a silently wrong number.
- **`ops::ingest_files` skips the per-file channel/task-spawn round-trip for files that fit in one read.** Every
  file previously paid a fresh `tokio::sync::mpsc::channel` plus a `spawn_blocking` reader task and a channel
  round-trip regardless of size — with thousands of small files, that ceremony dominated wall time (profiling
  showed threads parking and waking far more than they computed). Files at or under the existing streaming
  chunk size (8 MiB) now read in one `std::fs::read` inside a single `block_in_place` instead. `many-small`
  create: 72.3 → 178.3 MiB/s (+147%), reversing a 47%-behind-parpar result into 31% ahead of it. Large-file
  behavior is unchanged (still the streaming path) and byte-for-byte identical output either way.
- **`RecoverySet::load_metadata`** indexes recovery blocks on disk as `(path, offset, len)` without reading their
  bodies into memory, and **`load_recovery_blocks(max_blocks)`** loads only what repair actually needs — the
  existing eager `load` (metadata + every block's body) is unchanged and still the right call when a caller
  genuinely wants everything. `verify`, `health`, and deobfuscation only ever needed the file list, not gigabytes
  of recovery data resident just to answer "does a recovery set exist"; `repair` now loads only
  `total_bad_slices()` blocks instead of the whole set regardless of how much damage was actually found.
- **`encoder::altmap_kernel_available()` / `encoder::shuffle2x_kernel_available()`** — whether this CPU has the
  kernel behind each specialized buffer layout, i.e. whether `new_altmap`/`new_shuffle2x` will keep the layout
  or fall back to the portable one. Only the encoder knows the exact condition (ALTMAP needs AVX2 *without*
  GFNI; Shuffle2x runs on GFNI hardware too), and callers that want to *measure* a specific kernel need to know
  before they time it.

### Fixed
- **`calculate_geometry` no longer overflows when a set has more files than the 32 768-slice
  spec ceiling.** Growing the slice cannot merge files; the loop now stops if the count
  does not drop (or if `slice_size` saturates) and returns the existing "too many slices"
  error instead of panicking.
- **`Par2Worker` no longer drops recycled slice buffers on every Reed-Solomon flush.** The encoder
  returns up to 128 buffers at a time; the recycle path used a bounded `sync_channel` of depth 64
  and `try_send`, so half of each flush was discarded and the producer allocated new slice-sized
  `Vec`s from the OS. The recycle channel is now unbounded — live buffers were already bounded by
  the encoder queue.
- **`parmesan create` could exhaust virtual address space on high-core machines independent of
  `--memory-limit`**, panicking well inside the configured budget on hosts with a restrictive `RLIMIT_AS`
  (`ulimit -v`, a real shared-host/HPC/container pattern). Not the recovery buffer itself — `ingest_files`
  already streams input in chunks and only allocates the current pass's recovery buffer — but thread fan-out
  that ran before either of those: `#[tokio::main]`'s default one-worker-thread-per-core runtime, combined with
  glibc's up-to-`8×ncores` per-thread malloc arenas (each reserving tens of MiB of address space, counted in
  full against `RLIMIT_AS`, never returned). Fixed by capping the tokio runtime to a fixed worker-thread count
  and calling `mallopt(M_ARENA_MAX, 2)` before any thread exists (matching the value `pesto` itself already
  uses for the same reason); the CPU-bound rayon pool is deliberately left sized to physical cores, since it's
  the only one of the two doing real throughput-sensitive work. An 11× `VmPeak` reduction on a 12-core dev box;
  reproduced and confirmed fixed on the original 128-core/`RLIMIT_AS` box the crash was reported on.
- **A malformed or adversarial season PAR2 input could overflow the GF(2^16) exponent space instead of failing
  with an actionable error.** Added an explicit bounds check on the slice index against the calculated
  `total_slices`, with a message carrying the actual vs. expected counts, instead of an out-of-bounds panic.
- **`RecoveryEncoder` could silently return all-zero recovery blocks.** Three code paths reacted to a
  buffer layout whose SIMD kernel was unavailable by draining the queued input slices *without processing
  them* and carrying on, so `finish()` handed back parity that no PAR2 client can repair with — no error,
  no warning, no panic. Reproduced end to end: `par2cmdline` reports "Repair is possible" and then
  "Repair Failed", because the recovery data it was given is zeros.
  - `new_altmap()`/`try_new_altmap()` on any GFNI-capable CPU. `build_dep_tables()` returns `None` there
    (GFNI uses a different kernel), which left the ALTMAP flush path inactive. This is what
    `pesto`'s `altmap_path_generates_valid_par2_repaired_by_par2cmdline` had been failing on.
  - `new_shuffle2x()`/`try_new_shuffle2x()` on x86_64 without AVX2, and both constructors on any
    non-x86_64 target, where neither kernel is compiled in at all.
  - A manual `SimdPath` override (`pesto --simd …`) applied to a specialized layout. This one was
    reachable in production: `try_new_smart()` builds a Shuffle2x encoder on AVX2-without-GFNI hardware
    (Haswell through Comet Lake — most pre-Ice-Lake Intel), so `--simd scalar` ran a Normal-layout kernel
    against Shuffle2x buffers and wrote corrupt parity. `--simd ssse3`/`avx2`/`gfni` panicked instead of
    corrupting, since those kernels assert the layout. `--simd auto`, the default, was never affected.

  Layout-specific constructors now fall back to the portable layout when their kernel is absent — the
  recovery data is identical either way, only throughput differs — and `flush()` only honours a manual
  `SimdPath` for Normal-layout buffers, falling through to auto-detection otherwise (the same behaviour an
  unavailable path already had). The three silent-drain arms are now hard failures carrying the invariant
  they broke, so a future regression cannot go quiet again.
- **A malformed IFSC packet whose slice count didn't match the file's actual length was accepted anyway.**
  Length/slice-count agreement is now checked at load time; `verify` flags trailing junk data past a file's
  recorded length instead of silently ignoring it; repair `set_len`s a reconstructed file to its recorded length
  rather than leaving it however long the last written slice happened to make it; and slice byte-writes now
  saturate instead of panicking on a length that doesn't divide evenly.

### Security
- **A PAR2 File Description name could escape the intended destination directory.** `RecoverySet::load` (and
  `load_metadata`) parses file names out of PAR2 packets — data that, by construction, comes from whatever wrote
  the `.par2` file, not from this process. A name containing `..`, an absolute path, or a drive prefix was
  joined onto the destination directory unchanged, and nothing stopped the join from landing outside it. Names
  are now sanitized on load, and the join itself goes through a new `contained_path(base, name)` that
  hard-fails if the result isn't actually under `base` — belt-and-braces in case a future caller skips
  sanitization. `pesto`'s `--check` repost path and `penne`'s deobfuscation both read PAR2 file names through
  this same function, so the fix covers every consumer, not just `parmesan`'s own CLI.

### Changed
- `new_altmap_produces_correct_recovery_data` no longer skips GFNI hardware — that skip was hiding the bug
  above. Added `layout_constructors_agree_with_the_portable_encoder` and
  `manual_simd_path_never_corrupts_a_specialized_layout`, which assert every constructor × every
  `SimdPath` matches the portable encoder byte for byte on whatever CPU the tests run on.

## [0.4.1] — 2026-08-05

### Added
- **Fallible allocation API for `RecoveryEncoder` and `Par2Worker`.** Added
  `try_new`, `try_new_altmap`, `try_new_shuffle2x`, `try_new_smart`, and
  `try_take_buffer` methods, all returning `Result<T, TryReserveError>`.
  These allow callers to distinguish allocation failures from other errors
  and respond gracefully (e.g., with an actionable message) instead of
  panicking. Existing infallible methods remain as thin wrappers over the
  new `try_*` versions for backward compatibility.

### Fixed
- **`build_dep_tables()` had undefined behavior on allocation failure.** It
  called raw `std::alloc::alloc_zeroed` and constructed a `Box` without
  checking the returned pointer for null, which is undefined if that 2 MiB
  allocation ever fails. Switched to `Vec::try_reserve_exact` for a
  checkable, well-defined failure path.

## [0.4.0] — 2026-07-24

### Fixed
- **`Par2Worker`'s internal producer→hasher→encoder pipeline channels
  defaulted to a depth of 256 slices.** Each in-flight slot holds a full
  `par2_slice_size` buffer, so at the slice sizes common on large files
  (tens of MB) that alone could hold several GB across the three pipeline
  stages — entirely invisible to and uncapped by whatever memory budget
  the caller sized the encoder's own buffers against. Depth is now a
  caller-supplied parameter (`Par2Worker::spawn` takes a `channel_depth`
  argument) with a small default (`DEFAULT_CHANNEL_DEPTH = 4`) — enough to
  let the async reader race a little ahead of the RS/hash threads without
  becoming its own unbounded memory sink.
- **The hasher thread ran fire-and-forget**: if it panicked before sending
  its final result, the channel closed silently and the caller saw only
  an empty hash list — surfacing downstream as a confusing "worker
  returned fewer hashes than non-empty files" with no hint that a panic
  (the real cause) had happened. Its `JoinHandle` is now kept and joined,
  so a real panic propagates with its original message instead of being
  swallowed.

## [0.3.0] — 2026-07-20

### Added
- **`verify::verify_with_progress`**: same as `verify`, but calls a
  callback after every slice is read and checksummed (a missing file's
  slices are accounted for in one step), so a caller driving a long
  verify pass over a large release can show live progress instead of
  going silent until the whole pass finishes. `verify` itself is
  unchanged — it now just calls `verify_with_progress` with a no-op
  callback. New `VerifyProgress` struct carries the current file name plus
  `slices_done`/`total_slices` counted across the whole recovery set, so a
  single overall progress bar needs no per-file bookkeeping of its own.

## [0.2.0] — 2026-07-18

### Added
- **`parmesan verify <index.par2>`**: re-hashes files against an existing
  PAR2 recovery set (scanning its directory for every volume belonging to
  the same recovery set, matched by recovery-set ID) and reports which
  files are OK, damaged, or missing, with exit codes matching the PAR2
  convention (`0`/`1`/`2`). `--quiet` and `--json` output modes.
- **`parmesan repair <index.par2>`**: reconstructs damaged or missing slices
  via Reed-Solomon decoding and writes them back to disk. `--dry-run`
  reconstructs and checksum-verifies without writing; `--out-dir` writes
  repaired files elsewhere instead of overwriting in place. Every
  reconstructed slice's checksum is verified against the recovery set's
  IFSC packet *before* anything is written. Cross-validated bidirectionally
  against `par2cmdline` (`crates/parmesan/tests/par2cmdline_compat.rs`,
  run with `--ignored`).
- Explicit `create`/`verify`/`repair` subcommands; bare invocation
  (`parmesan <files>...`) still aliases to `create`.

### Fixed
- **Multi-file recovery sets used the wrong Reed-Solomon block order**:
  input slices were fed to the encoder in command-line/directory order
  instead of the ascending-File-ID order the PAR2 spec requires, making
  recovery data for any multi-file set silently incompatible with
  third-party PAR2 readers even though `parmesan`-only round-trips worked
  by coincidence. Files are now hashed (first 16 KiB) and sorted by File ID
  before slice indices are assigned. Single-file recovery sets were never
  affected. **This is a breaking change for recovery data generated by
  prior `parmesan` versions on multi-file inputs** — regenerate `.par2`
  sets after upgrading.
- **`-r` was bound to both `--recovery-pct` and `--recovery-count`**,
  which made `clap` panic while building the CLI parser. Nothing previously
  called `Cli::parse()` in a test, so this was never caught; found while
  wiring up the `verify`/`repair` subcommands. `--recovery-count` no longer
  has a short flag.
- **Windows MSVC build failure (`LNK1181`)**: the `asm` feature pulled in
  `md5-asm`, which uses inline assembly that fails to link under MSVC. Removed
  the feature in favor of the pure-Rust `md-5` implementation, which is
  portable across all targets.
- **Encoder panic on GFNI-capable CPUs using the ALTMAP path**:
  `build_dep_tables` returns `None` on GFNI CPUs (GFNI uses a different
  kernel and never builds dependency tables), but `flush_avx2_altmap` was
  still called unconditionally and unwrapped it. The ALTMAP path is now
  skipped when `dep_tables` is absent, and its test skips on GFNI hardware
  where the path is inactive instead of failing.

## [0.1.0] — 2026-05-23

### Added
- Multi-threaded Reed-Solomon PAR2 encoder with SIMD dispatch:
  - Scalar (pure Rust, no SIMD)
  - SSSE3 (128-bit shuffles, x86/x86_64)
  - AVX2 (256-bit shuffles, x86_64)
  - AVX2+GFNI (Intel Ice Lake+)
  - AVX-512+GFNI (Intel Ice Lake+, production-enabled after Ice Lake Xeon
    validation on AWS m6i)
  - ARM NEON (AArch64)
- Automatic SIMD path selection at runtime (`--simd auto`); override via
  `--simd <path>` flag.
- PAR2 geometry calculation: auto slice-size from file set, configurable via
  `--slice-size` and `--num-slices`.
- Full PAR2 v2 packet generation: `FileDesc`, `IFSC`, `Main`, `RecvSlic`,
  `Creator`.
- `walkdir`-based directory ingestion: pass files or whole directories.
- Progress bar via `indicatif`.
- `tracing` + `tracing-subscriber` logging with `RUST_LOG` override.
- Library API (`lib.rs`) exposing encoder, layout, ops and packet modules for
  embedding in `pesto`.
- `bench-internals` feature to expose per-path flush functions for
  micro-benchmarking.
- `par2-avx2-gfni-unsafe` feature to expose AVX2+GFNI and AVX-512+GFNI paths
  for explicit testing.
