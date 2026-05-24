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

### 22a — Subcommand refactor (Complexity: Low)

Currently the CLI has a single implicit `create` action. Restructure it into
explicit subcommands to make room for `verify` and `repair`.

- [ ] Rename the current entry point to `parmesan create <files>`.
- [ ] Keep all existing flags under `create`; adjust README and shell completions.
- [ ] Alias bare invocation (no subcommand) to `create` for backwards compatibility.

### 22b — PAR2 file parser (Complexity: High)

Reading existing `.par2` files is the foundation for both verify and repair.

- [ ] Implement a packet reader that can deserialise all packet types: Main, File
      Description, IFSC, Recovery, Creator.
- [ ] Validate packet magic, CRC, and recovery-set-ID consistency.
- [ ] Collect the full recovery set by scanning for all `.par2` / `*.vol*.par2`
      files in the same directory as the index file.
- [ ] Handle duplicate and out-of-order packets gracefully.

### 22c — Verify mode (Complexity: Medium)

```
parmesan verify <index.par2>
```

- [ ] Re-hash input files and compare MD5-16k / MD5-full against File Description
      packets.
- [ ] Compare per-slice CRC32 + MD5 against IFSC packets.
- [ ] Report: which files are OK, which are damaged, which are missing entirely.
- [ ] Exit codes matching the PAR2 spec (0 = OK, 1 = repairable, 2+ = fatal).
- [ ] `--quiet` and JSON output modes.

### 22d — Repair mode (Complexity: Very High)

```
parmesan repair <index.par2>
```

- [ ] Identify the set of damaged/missing input slices (from verify pass).
- [ ] Determine whether enough recovery blocks exist to reconstruct them.
- [ ] Apply Reed-Solomon decoding: solve the linear system for the missing slices.
- [ ] Write reconstructed data back to the original file paths (or `--out-dir`).
- [ ] Validate restored files against their checksums before overwriting originals.
- [ ] `--dry-run` flag: report what *would* be repaired without writing files.

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
