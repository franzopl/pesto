# Roadmap — `parmesan`

Standalone, high-performance PAR2 creation library and CLI.
Used internally by [`pesto`](../../ROADMAP.md) and published as an independent crate.

---

## Completed ✅

| Item | Summary |
|------|---------|
| GF(2¹⁶) algebra | Galois field arithmetic for Reed-Solomon |
| Reed-Solomon encoder | Cauchy matrix generation, single-pass parity |
| PAR2 packets | Main, File Description, IFSC, Creator serialization |
| SIMD dispatch | Scalar / SSSE3 / AVX2 / AVX2+GFNI / AVX-512+GFNI / ARM NEON |
| Volume layout | Exponential split (`vol000+001`, `vol001+002`, …) |
| Cargo workspace | Extracted from `pesto` into `crates/parmesan` |
| Generic API | `std::io::Read`-based encoder, no NNTP/Usenet coupling |
| Benchmarks | Micro-benchmarks inside the crate, `#[inline]` tuning |
| `--recovery-pct` | Redundancy as a percentage of input size (default 10 %) |
| `--slice-size` | Manual PAR2 slice size, e.g. `"1 MiB"` |
| `--slice-count` | Target number of input slices |
| `--recovery-count` | Exact recovery block count instead of a percentage |
| `--memory-limit` | Cap RAM used for recovery buffers |
| `--threads` | Override Rayon thread pool size |
| `--simd` | Force a SIMD path (`auto`/`scalar`/`ssse3`/`avx2`/…) |
| `--out-dir` | Output directory for all `.par2` files |
| `--base-name` | Override output base name |
| `--quiet` | Suppress progress and geometry output |
| `--overwrite` | Overwrite existing `.par2` files instead of aborting |
| `--no-index` | Skip the `.par2` index file |
| `--recurse` | Expand directories into their constituent files |
| `--comment` | Embed comment strings in the Creator packet (repeatable) |
| `--recovery-offset` | Set the exponent of the first recovery block |
| Published to crates.io | `parmesan-par2` v0.1.0 released 2026-05-23 |

---

---

## Phase 22 — Verify & Repair

The highest-impact missing feature: without verify/repair, `parmesan` can only
*create* PAR2 sets — it cannot check or restore damaged files, which is a core
use case for any complete PAR2 tool.

**Design decisions** (see `crates/parmesan/src/encoder.rs`, `gf16.rs` for the
existing encoder this phase builds on):

- **Revised during implementation:** decode does *not* extract the encoder's
  SIMD kernels. It uses a fresh, independent scalar multiply-accumulate
  kernel (`gf16_mac.rs`) instead. Extracting `RecoveryEncoder::flush_*` was
  the original plan and is still the right end state for performance, but it
  is the one step in this phase that touches existing production code, and
  doing it blind (all six backends, 4200+ lines of `unsafe` SIMD) before
  proving the repair *algorithm* itself correct was the wrong order of
  operations. Getting a correct, fully-tested scalar repair path shipped
  first — with zero risk to the existing encoder — was judged more valuable
  than the performance work. SIMD extraction is now tracked as its own
  follow-up (still called out below; no longer a dependency of 22d.3).
- Matrix inversion (GF(2¹⁶) Gauss-Jordan) lives in its own module
  (`matrix.rs`), independent of the encoder — the encoder never inverts
  anything today. The multiplicative inverse it needs was added to
  `Gf16::inverse` in `gf16.rs` (pure field arithmetic, not matrix-specific).
- No dependency on the external `rust-par2` crate: its SIMD coverage stops at
  AVX2/SSSE3 (missing AVX2+GFNI, AVX-512+GFNI, NEON, which `parmesan` already
  has for encoding) and its license is unconfirmed. Decode is implemented
  natively to keep one GF(2¹⁶) codebase for the whole PAR2 lifecycle.
- No public API of the existing encoder changes; all new work lands in new
  modules (see table below).

**A prerequisite bug found and fixed along the way:** the PAR2 spec assigns
Reed-Solomon coefficients to input slices in ascending File ID order (the
same order the Main packet lists them in), not in command-line/directory
order. `parmesan create` fed slices to the encoder in command-line order,
which made recovery data for any *multi-file* set silently incompatible with
third-party PAR2 readers — repair here would have reconstructed the wrong
bytes without any error, since the math "succeeds" with the wrong
coefficients. Fixed in `ops::sort_files_by_file_id` (see 22d.3 below).
Single-file recovery sets were never affected. `.par2` sets created by
`parmesan` before this fix should be regenerated.

**Effort legend:** S = 0.5–1 day · M = 1–3 days · L = 3–7 days · XL = 1–3 weeks.

| Step | Depends on | Effort |
|------|------------|--------|
| 22a  | —                    | S  |
| 22b.1 | —                   | M  |
| 22b.2 | 22b.1               | M  |
| 22b.3 | 22b.1               | S  |
| 22c.1 | 22b.2, 22b.3        | S  |
| 22c.2 | 22c.1               | S  |
| 22d.1 | —                   | M  |
| 22d.2 | — (touches `encoder.rs`; own branch) | L |
| 22d.3 | 22b.2, 22d.1        | L  |
| 22d.4 | 22c.1, 22d.3        | M  |
| 22e   | 22d.2, 22d.3        | M  |
| 22f   | 22d.3               | M  |
| 22g   | 22a, 22c.2, 22d.4   | S  |
| 22h   | runs alongside all steps above | XL |
| 22i   | everything above stable | S  |

22b.2/22c.1 and 22d.1 can start in parallel — they have no mutual dependency.
22d.3 no longer depends on 22d.2 (see the revised design decision above):
it uses its own scalar kernel, so it only needed 22d.1 and 22b.2 to start.
22d.2 (SIMD kernel extraction) remains the item to be most careful with
whenever it *is* picked up — it's still the only one touching existing
production code — but it no longer blocks the rest of the phase.

### 22a — Subcommand refactor (Complexity: Low) ✅ Done

Currently the CLI has a single implicit `create` action. Restructure it into
explicit subcommands to make room for `verify` and `repair`.

- [x] Rename the current entry point to `parmesan create <files>`.
- [x] Keep all existing flags under `create`; adjust README and shell completions.
- [x] Alias bare invocation (no subcommand) to `create` for backwards compatibility.

While wiring this up, found and fixed a pre-existing, unrelated bug: `-r` was
bound to both `--recovery-pct` and `--recovery-count`, which made clap panic
while building the parser (only visible once something actually called
`Cli::parse()` — no test or CI step had ever exercised that path). Dropped the
short flag from `--recovery-count`; `-r`/`--recovery-pct` is unchanged and
matches the documented README flag table.

### 22b.1 — PAR2 packet reader (Complexity: Medium) ✅ Done

New module `packet_reader.rs`, the byte-level inverse of `packet.rs`.

- [x] Deserialise all packet types: Main, File Description, IFSC, Recovery,
      Creator (64-byte header + body, per `packet.rs`'s documented layout).
- [x] Validate magic bytes and recompute the packet MD5 (bytes 32 onward);
      discard packets that fail either check instead of aborting the file.
- [x] Treat the header's `total length` field as untrusted: bound-check it
      against the actual file size before allocating, so a forged length
      cannot cause an out-of-memory abort. This module is the only part of
      the crate that parses externally-sourced bytes (downloaded `.par2`
      files), so it is the target of the fuzzing work in 22h.

Covered by unit tests today (round trip, corrupted-hash skip, forged length,
truncated input, garbage input); `cargo-fuzz` harness itself is still open,
tracked under 22h.

### 22b.2 — Recovery set assembly (Complexity: Medium) ✅ Done

New module `recovery_set.rs`.

- [x] Scan the index file's directory for all `.par2` / `*.volNNN+MMM.par2`
      files, independent of naming scheme — cannot assume `layout.rs`'s
      `plan_volumes()`, which only governs how *this* encoder writes volumes.
- [x] Aggregate packets by `recovery_set_id`; index available recovery blocks
      by exponent and input slices by file.
- [x] Public type `RecoverySet` with `RecoverySet::load(index_path)`.

`RecoverySet::files` is ordered by ascending File ID per the PAR2 spec (see
the module doc comment) — this is the canonical order a compliant reader must
use, independent of the encoder-side ordering issue tracked under 22d.3.

### 22b.3 — Packet validation & deduplication (Complexity: Low) ✅ Done

- [x] Deduplicate packets that appear in more than one volume file (common in
      practice), keeping the first validly-hashed occurrence.
- [x] Handle out-of-order packets and missing volumes gracefully — a
      `RecoverySet` must be usable even when some volumes are absent.

### 22c.1 — Verify pipeline (Complexity: Low) ✅ Done

New module `verify.rs`.

- [x] Re-hash input files and compare MD5-16k / MD5-full against File
      Description packets, reusing `encoder::FileHasher` unchanged.
- [x] Compare per-slice CRC32 + MD5 against IFSC packets, reusing
      `encoder::slice_checksum` unchanged.
- [x] Stream files with the same double-buffered read pattern as
      `ops::ingest_files`.

Streams each file in `slice_size` chunks rather than reading it whole, so
verification of large (movie-sized) files stays memory-bounded.

### 22c.2 — Verify report & exit codes (Complexity: Low) ✅ Done

```
parmesan verify <index.par2>
```

- [x] `VerifyReport`: per-file status (OK / damaged / missing), recoverable
      slice count vs. available recovery blocks.
- [x] Exit codes matching the PAR2 spec (0 = OK, 1 = repairable, 2+ = fatal).
- [x] `--quiet` and `--json` output modes.

Manually verified end-to-end: `create` → `verify` (OK), byte-corrupted file →
`verify` (`DAMAGED`, exit 1, repairable), missing file → `verify` (`MISSING`,
exit 2 when damage exceeds available recovery blocks), `--json` output.

### 22d.1 — GF(2¹⁶) matrix module (Complexity: Medium) ✅ Done

New module `matrix.rs`, independent of the encoder.

- [x] Build the reduced `m×m` matrix from selected missing/available block
      indices via `gf16::Gf16::pow` (a submatrix of a Vandermonde-like
      matrix — always invertible over the field).
- [x] Gauss-Jordan elimination in GF(2¹⁶) with pivoting; a zero pivot from a
      bad block selection returns `Err(SingularMatrix)`, never panics.

Also added `Gf16::inverse` to `gf16.rs` (the multiplicative inverse
Gauss-Jordan needs) — pure field arithmetic, so it lives alongside `mul`/
`pow`/`exp`, not in `matrix.rs`.

### 22d.2 — Extract the SIMD MAC kernel (Complexity: Very High) ⏸ Deferred

**Not started, and no longer blocking the rest of this phase** — see the
revised design decision above. The only step of this phase that would touch
existing code (`crates/parmesan/src/encoder.rs`); still deserves its own
branch/PR and a byte-for-byte before/after proof whenever it's picked up.

- [ ] Move the multiply-accumulate logic out of `flush_scalar` / `flush_ssse3`
      / `flush_avx2` / `flush_avx2_gfni` / `flush_avx512_gfni` /
      `flush_neon_clmul` into free functions, parameterised by
      `(coefficient, source buffer, destination buffer)` instead of reading
      `self.buffers` directly, and have `gf16_mac.rs` dispatch to them
      instead of its current scalar-only implementation.
- [ ] Preserve every `#[target_feature]` annotation and the existing runtime
      dispatch logic from `flush()` unchanged in behavior.
- [ ] Prove byte-for-byte identical output before/after the refactor over a
      fixed corpus, in addition to the existing regression tests
      (`simd_recovery_matches_scalar`, `gfni_recovery_matches_scalar`, …)
      already in `encoder.rs`.
- [ ] Update `RecoveryEncoder::flush_*` to call the extracted functions; no
      change to any public type or signature in `encoder.rs`.

### 22d.3 — RecoveryDecoder (Complexity: Very High) ✅ Done

New modules `gf16_mac.rs` and `decoder.rs`.

- [x] `gf16_mac::mac(gf, dst, src, coeff)`: a fresh, independent scalar
      multiply-accumulate kernel (`dst ^= coeff * src`) — not extracted from
      the encoder (see the revised design decision above).
- [x] Select available recovery blocks for the missing input slices (lowest
      exponents first when more blocks than needed are available).
- [x] Subtract the contribution of every known input slice from all selected
      recovery blocks in one pass over the known slices — the caller's
      `known_slice` callback is invoked exactly once per known index, not
      once per missing block, so reading from disk pays for one sequential
      pass over the surviving data. (A fully incremental Gauss-Jordan
      merged with that same read pass, matching par2cmdline more closely, is
      further optimisation — deferred alongside 22d.2/22e.)
- [x] Invert the reduced matrix via `matrix.rs` and reconstruct missing slice
      data via `gf16_mac`.
- [x] Public API ended up callback-based rather than incremental
      (`add_recovery_block`/`has_enough` as originally sketched):
      `RecoveryDecoder::new(slice_size, total_input_slices, missing)`,
      `.missing()`, `.reconstruct(known_slice_fn, &recovery_blocks)`. This
      keeps the decoder decoupled from file I/O entirely — `repair.rs`
      supplies `known_slice` as a closure that reads from disk.
- [x] Round-trip proven against the real encoder: `RecoveryEncoder` produces
      recovery blocks, slices are dropped, `RecoveryDecoder` reconstructs
      them bit-exact, across single/multiple/surplus-recovery-block/
      reconstruct-everything cases and a not-enough-recovery-blocks error
      case.

### 22d.4 — Repair orchestration (Complexity: High) ✅ Done

New module `repair.rs`.

```
parmesan repair <index.par2>
```

- [x] Identify damaged/missing input slices from the verify pass (22c.1) via
      `VerifyReport::files[_].bad_slice_indices` (verify.rs was extended to
      expose *which* slices are bad, not just a count).
- [x] Determine whether enough recovery blocks exist to reconstruct them
      (`VerifyReport::is_repairable`).
- [x] Drive `RecoveryDecoder` to reconstruct the missing slices.
- [x] Write reconstructed data back to the original file paths (or `--out-dir`,
      which copies damaged files whole before patching, and creates missing
      files fresh).
- [x] Re-verify restored files against their checksums — done *before*
      writing anything (every reconstructed slice's checksum is checked
      against its IFSC entry first; a mismatch aborts that file's repair
      with no data written), which is a stronger guarantee than checking
      after the fact.
- [x] `--dry-run` flag: reconstructs and checksum-verifies without writing.

Proven end-to-end with the real CLI binary: multi-file `create`, corrupt one
file + delete another entirely, `verify` (reports both), `repair --dry-run`,
`repair`, `verify` again (clean), MD5 of both files matches the originals
exactly.

### 22e — SIMD parity for decode (Complexity: Medium) — Blocked on 22d.2

- [ ] Once 22d.2 lands, benchmark all six SIMD backends in the decode path,
      confirming runtime dispatch picks the same backend decode would pick
      for encode on the same CPU.
- [ ] Until then, decode runs scalar-only. `gf16_mac::mac` processes one
      `u16` word (2 bytes) per loop iteration with no vectorisation — this is
      the performance gap that keeps `parmesan repair` from being
      competitive with `par2cmdline-turbo` yet.

### 22f — Streaming memory model (Complexity: Medium)

- [ ] Process reconstruction in column chunks: the inverted matrix is reused
      across every word position of a slice, never rebuilt per chunk. Cap
      memory to `m × chunk_size` rather than `m × slice_size`, mirroring the
      existing `flush_limit_bytes` mechanism in the encoder. Can be deferred
      past the initial MVP if memory pressure isn't observed in practice.
      Today `RecoveryDecoder` and `repair.rs` hold every selected recovery
      block and every in-flight reconstructed slice fully in memory at once.

### 22g — CLI wiring (Complexity: Low) — Mostly done

- [x] `verify` subcommand: `--quiet`, `--json`.
- [x] `repair` subcommand: `--dry-run`, `--out-dir`, `--quiet`.
- [x] `--simd` inherited from `create` (repair doesn't have its own SIMD
      choice to make yet — see 22e).
- [ ] `--json` on `repair` (currently `verify`-only).
- [ ] Progress bar via `indicatif` for `repair`, same pattern as `create`
      (not yet wired; repairs in the tested size range finish fast enough
      that this hasn't been a priority).

### 22h — Compatibility & test suite (Complexity: Very High) — In progress

Unit-level correctness is covered today: 89 tests across the new modules,
including full round-trips through the real `RecoveryEncoder` and the real
CLI binary (multi-file create → corrupt/delete → verify → repair → verify,
confirmed byte-identical via MD5). What's still open:

- [ ] **Cross round-trip**: create with `parmesan`, corrupt slices, repair
      with real `par2cmdline` → byte-identical to the original, and the
      reverse (create with `par2cmdline`, repair with `parmesan`).
- [ ] **Fixture corpus**: small set of real `.par2` files (varying slice
      sizes, volume counts, Unicode names) versioned under
      `crates/parmesan/tests/fixtures/`, exercised in every CI run without
      requiring an external binary.
- [ ] **Optional CI job with the real binary**: non-blocking job (runs on
      `main`) installing `par2cmdline-turbo` for the cross round-trip above
      against dynamically generated fixtures.
- [ ] **Fuzzing**: `cargo-fuzz` target on `packet_reader.rs` (must never
      panic, over-allocate, or read out of bounds on arbitrary bytes);
      `proptest` on `matrix.rs` (any valid missing/available index set must
      invert, and `A · A⁻¹ == I`).
- [ ] **Property tests**: encode → drop N random slices (N ≤ available
      recovery blocks) → decode → output identical to the original, for N
      from 1 up to the repairable maximum; all six SIMD backends agree with
      the scalar reference.
- [ ] **Benchmarks**: `criterion` throughput (MB/s) per SIMD backend across
      loss fractions (1%, 10%, 50%, at the repairable limit); direct
      comparison against `par2cmdline-turbo` on the same machine; isolated
      benchmark of matrix inversion cost for `m` from 10 to 5000 to calibrate
      the practical limit before algebra cost dominates the MAC cost.

### 22i — Documentation (Complexity: Low)

- [ ] Rustdoc on every new public item in `packet_reader`, `recovery_set`,
      `matrix`, `gf16_mac`, `verify`, `decoder`, `repair` (extends Phase 26's
      documentation audit to the new modules).
- [ ] README/CHANGELOG updates for the `verify`/`repair` subcommands.
- [ ] Repair algorithm section in `INTERNALS.md` (Phase 26c), citing the PAR2
      spec sections it implements.

---

## Phase 23 — Input Flexibility

### 23a — File list input (Complexity: Low)

- [ ] `-i / --input-file <path>` — read newline-separated file paths from a text
      file; pass `-` to read from stdin.
- [ ] `-0 / --input-file0` — null-character separated variant (safe with arbitrary
      filenames).
- [ ] Support piped process output via `proc://cmd` descriptor style (parpar
      convention).

### 23b — Symlink & path handling (Complexity: Low)

- [ ] `-L / --skip-symlinks` — ignore symbolic links when recursing.
- [ ] `-f / --filepath-format [basename|keep|common|outrel]` — control how input
      file paths are stored in PAR2 File Description packets.
      - `basename` (default): strip all directory components.
      - `keep`: preserve the path as given.
      - `common`: strip the longest common prefix from all paths.
      - `outrel`: make paths relative to `--out-dir`.

---

## Phase 24 — Volume Layout Control

### 24a — Uniform and custom split schemes (Complexity: Medium)

Currently the only layout is exponential (1, 2, 4, 8, …). Add alternatives:

- [ ] `--volume-scheme [pow2|uniform|files]` — select the split strategy.
  - `pow2` (current default): exponentially growing volumes.
  - `uniform`: equal-sized recovery volumes.
  - `files`: exact number of volumes via `--recovery-files N`.
- [ ] `--slices-per-file N` — cap the maximum slices in each recovery volume.
- [ ] `--slices-first-file N` — override the slice count of the first volume only.
- [ ] `--recovery-files N` — target a fixed number of recovery volumes.

### 24b — Naming scheme (Complexity: Low)

- [ ] `--naming [parpar|par2cmdline]` — toggle between the two common naming
      conventions.
  - `parpar` (default): `base.vol12-22.par2` (range notation).
  - `par2cmdline`: `base.vol012+010.par2` (offset+count notation, current).

---

## Phase 25 — Output & UX Polish

### 25a — Structured output (Complexity: Low)

- [ ] `--json` — emit machine-readable JSON lines: geometry, per-volume progress,
      final summary. Useful for integration with `pesto` and `upapasta`.
- [ ] `--progress [auto|stderr|stdout|none]` — explicit control over where the
      progress indicator is written (default: `auto` = stderr when a TTY, none
      otherwise).

### 25b — Packet metadata (Complexity: Low)

- [ ] `--packet-redundancy N` — number of copies of critical packets (Main, File
      Description, IFSC) written into each recovery volume (default: 1).
- [ ] `--unicode` — force generation of Unicode filename packets alongside ASCII
      ones (the PAR2 spec allows both; useful for non-ASCII filenames on Windows).

### 25c — Write safety (Complexity: Low)

- [ ] `--write-sync` — call `fsync` on each output file before exit, ensuring
      data is on disk even if the OS crashes immediately after.

---

## Phase 26 — Documentation

A complete PAR2 tool needs documentation as polished as its code. All docs live
inside the repository; nothing requires a separate site.

### 26a — API documentation (Complexity: Low)

- [ ] Audit every public item in `lib.rs`, `encoder.rs`, `ops.rs`, `packet.rs`,
      `layout.rs`, and `gf16.rs` for missing or incomplete rustdoc comments.
- [ ] Add a module-level doc comment to each public module explaining its role
      and the invariants callers must uphold.
- [ ] Write at least one runnable `# Examples` block in `lib.rs` showing the
      full create flow: `InputFile` → `CreateOptions` → `Par2Worker` → output
      files.
- [ ] Enable `#![deny(missing_docs)]` in `lib.rs` and fix any remaining gaps.
- [ ] Verify that `cargo doc --no-deps -p parmesan` produces zero warnings.

### 26b — CLI reference (Complexity: Low)

- [ ] Expand the `README.md` flag table to cover all flags across all subcommands
      (`create`, `verify`, `repair`), including defaults and valid value ranges.
- [ ] Document exit codes and their meaning.
- [ ] Add an "Examples" section with real invocations (basic create, directory
      recursion, verify, repair, `pesto` integration via `--json`).
- [ ] Generate a Unix man page with `clap_mangen` and install it via the build
      script so `man parmesan` works out of the box.

### 26c — Algorithm notes (Complexity: Medium)

- [ ] Write a `INTERNALS.md` covering the Reed-Solomon implementation: GF(2¹⁶)
      construction, Cauchy matrix, SIMD dispatch strategy, and the multi-pass
      memory model.
- [ ] Document the PAR2 packet format sections that `parmesan` implements,
      citing the relevant sections of the PAR2 spec.
- [ ] Explain the volume layout algorithm and the rationale behind exponential
      sizing.

### 26d — Changelog discipline (Complexity: Low)

- [ ] Adopt the [Keep a Changelog](https://keepachangelog.com) format in
      `CHANGELOG.md`.
- [ ] Document the rules in a `CONTRIBUTING.md` so contributors know to update
      the changelog with every user-visible change.
