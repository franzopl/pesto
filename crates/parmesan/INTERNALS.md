# Internals — `parmesan`

How `parmesan` actually computes PAR2 recovery data and reconstructs damaged
files: the field arithmetic, the matrix, the SIMD dispatch strategy, and the
packet format. Written for someone modifying the crate, not just using it —
see `README.md` for usage and `ROADMAP.md` for what's planned/in progress.

This document folds together Phase 26c ("Algorithm notes") and Phase 22i
("Documentation") from `ROADMAP.md`: the encode side was already stable when
this was written; the decode/repair side is what Phase 22 added.

---

## 1. The field: GF(2¹⁶)

PAR2 computes recovery data over the Galois field GF(2¹⁶) with the
primitive polynomial `0x1100B` (`x¹⁶ + x¹² + x³ + x + 1`) and primitive
element `2`. This is fixed by the [Parity Volume Set Specification
2.0](https://parchive.sourceforge.net/docs/specifications/parity-volume-spec/article-spec.html)
— every detail here has to match `par2cmdline` bit-for-bit, or recovery
data isn't portable between tools.

`gf16.rs` builds two lookup tables of size `ORDER = 65535` (the order of
the multiplicative group):

- `antilog[i] = 2^i`
- `log[v]` = the `i` such that `antilog[i] == v`

Multiplication is then `mul(a, b) = antilog[(log[a] + log[b]) % ORDER]`
(zero handled as a special case). Every other field operation is built on
these two tables:

| Operation | Implementation |
|---|---|
| `pow(base, e)` | `antilog[(log[base] * e) % ORDER]` |
| `exp(e)` | `antilog[e % ORDER]` (i.e. `2^e`) |
| `discrete_log(v)` | `log[v]` |
| `inverse(v)` | `exp(ORDER - log[v])`, since every non-zero `v` satisfies `v^ORDER == 1` |

`inverse` is the one addition Phase 22 made to this module — matrix
inversion (§4) needs a multiplicative inverse and nothing before Phase 22
did.

## 2. Assigning coefficients to input blocks

Each input (source) block gets a **base constant**: `antilog(logbase)`,
where `logbase` is the block's index into the sequence of non-negative
integers coprime with 65535 (`65535 = 3·5·17·257`, so `input_logbases`
just skips multiples of 3, 5, 17, and 257). The **coefficient** applied to
input block `i` when forming the recovery block with **exponent** `e` is
`base_i ^ e` — a power of the block's own base constant. This is a
Vandermonde-style structure: it guarantees every square submatrix is
invertible (§4), which is what makes Reed-Solomon repair possible at all.

**Block order matters.** The spec assigns `logbase` 0 to the first input
block, 1 to the second, and so on — but "first" and "second" are defined by
**ascending numeric order of File ID**, the same order the Main packet
lists them in (`packet::main_body` sorts file IDs for exactly this reason).
This is not the order files happen to be listed on the command line, and
getting it wrong doesn't produce an error — it produces recovery data that
silently uses the wrong coefficients for every multi-file set, repairable
only by coincidence (if at all) by another tool. `ops::sort_files_by_file_id`
exists solely to get this order right before `create` starts encoding: it
hashes each file's first 16 KiB (all `compute_file_id` needs), sorts, and
only then hands the file list to the encoder.

## 3. Encoding

`encoder::RecoveryEncoder` holds one accumulator buffer per recovery
block. Feeding it input slice `j` (via `add_slice`) computes, for every
recovery block `e` currently being generated:

```
recovery[e] ^= coeff(j, e) · input_j        (word by word, GF(2¹⁶))
```

Slices are queued and processed in batches (`flush()`) rather than one at a
time, so the SIMD kernels can precompute a multiplication table once per
`(recovery block, input slice)` pair and reuse it across the whole slice
instead of doing a field multiply per word.

### SIMD dispatch

`flush()` picks the fastest kernel the CPU supports, checked at runtime via
`std::is_x86_feature_detected!`, in this order: AVX-512+GFNI → AVX2+GFNI →
AVX2 → SSSE3 → scalar (ARM: NEON, mandatory on AArch64). Two data layouts
(`altmap.rs`, `shuffle2x.rs`) rearrange the recovery buffers into
bit-plane or lane-separated form so the AVX2 kernels can operate with
fewer shuffle instructions; both convert back to normal layout in
`finish()`, so callers never see the difference.

Every kernel computes the *same* mathematical operation — multiply a
buffer by a GF(2¹⁶) coefficient and XOR-accumulate into an output buffer —
just at different vector widths and via different techniques (nibble-table
lookups on SSSE3/AVX2, `GF2P8AFFINEQB` on GFNI, polynomial multiply +
Barrett reduction on NEON). This is why decode's SIMD kernels (§5) could be
written independently without touching the encoder: it's the same
primitive, just needed with a different calling shape.

## 4. The reduced matrix (repair only)

`matrix.rs`. Given `m` missing input blocks and `m` available recovery
blocks, define the `m×m` matrix `A` where `A[r][c] = coeff(missing_c,
exponent_r)` — row `r` is a chosen recovery block's exponent, column `c` is
a missing input block's base constant. Because coefficients are powers of
distinct base constants (§2), `A` is a submatrix of a generalized
Vandermonde matrix, and **any** square submatrix of such a matrix over a
field is invertible — the maximum-distance-separable (MDS) property that
makes Reed-Solomon codes work. Concretely: it means *any* `m` recovery
blocks are enough to reconstruct *any* `m` missing input blocks; which
specific blocks were lost or which specific recovery blocks are available
never matters, only the count.

`Gf16Matrix::invert` computes `A⁻¹` via Gauss-Jordan elimination over
GF(2¹⁶). Unlike floating-point Gaussian elimination, there's no numerical
stability to worry about — GF(2¹⁶) arithmetic is exact — so the only
failure mode is a genuinely singular matrix, which for a correctly-built
`A` should be unreachable (`SingularMatrix` exists to fail loudly instead
of panicking if that invariant is ever violated by a caller bug).

## 5. Decoding

`decoder::RecoveryDecoder` (with `gf16_mac.rs` and `matrix.rs`) implements:

1. **Select** `m` recovery blocks for the `m` missing input blocks — the
   lowest available exponents, an arbitrary but deterministic choice (any
   `m` work, per §4).
2. **Subtract** every known input block's contribution from all `m`
   selected recovery blocks:
   `adjusted[r] = recovery[exponent_r] ⊕ Σ (coeff(j, exponent_r) · known_j)`
   over known `j`. Implemented as one pass over the known blocks — each is
   read once and XOR-accumulated into every `adjusted[r]` it contributes
   to — not one pass per missing block, so a caller streaming from disk
   only reads the surviving data once regardless of how much is missing.
3. **Invert** the reduced matrix `A` (§4).
4. **Reconstruct**: `missing_c = Σ_r A⁻¹[c][r] · adjusted[r]`.

Steps 2 and 4 are both "multiply a buffer by a coefficient and
XOR-accumulate" — `gf16_mac::mac(gf, dst, src, coeff)` — the exact same
primitive the encoder's SIMD kernels compute (§3), just called once per
`(source, coefficient)` pair instead of amortized across a whole flush.
`mac` dispatches AVX2 → SSSE3 → scalar at runtime, using the same
nibble-lookup decomposition as the encoder's SSSE3/AVX2 kernels
(`NibbleTables` in `gf16_mac.rs`) but implemented independently — written
fresh rather than extracted from `encoder.rs`, so decode never had to touch
existing production code to get SIMD acceleration. GFNI, AVX-512, and NEON
paths aren't implemented yet (tracked in `ROADMAP.md` Phase 22e).

`RecoveryDecoder` itself never touches a filesystem: `reconstruct` takes a
`known_slice` callback and a `recovery_blocks` map, and returns
`(global_index, bytes)` pairs. `repair.rs` is what maps those global slice
indices to files and byte offsets, and is also where **every reconstructed
slice's checksum is verified against its IFSC entry before anything is
written to disk** — a decode bug or a bad block selection turns into a
clean error on the affected file, not corrupted output.

## 6. PAR2 packet format

Every packet (`packet.rs`, `packet_reader.rs`) is a 64-byte header — magic
`PAR2\0PKT`, total length, an MD5 covering everything from offset 32
onward, a recovery-set ID, and a 16-byte type tag — followed by a body
padded to a multiple of 4 bytes. `parmesan` implements:

| Packet | Purpose |
|---|---|
| Main | Slice size + the recovery set's File IDs, sorted ascending (§2) |
| File Description | One file's ID, full-file MD5, first-16 KiB MD5, length, name |
| IFSC (Input File Slice Checksum) | Per-slice MD5 + CRC32 for one file |
| Recovery Slice | One recovery block's exponent + data |
| Creator | Free-text identifying the tool that made the set |

`packet_reader::read_packets` is the only part of the crate that parses
externally-sourced bytes (a downloaded `.par2` file could be anything), so
it treats every length field as untrusted: a packet's declared length is
bounds-checked against the buffer actually in hand before it's used for
anything, and a packet whose stored MD5 doesn't match is skipped rather
than treated as fatal — a corrupted packet shouldn't take down parsing of
everything after it.

`recovery_set::RecoverySet::load` finds every packet belonging to one
recovery set (matched by recovery-set ID, not file name) by scanning every
`.par2` file in the index file's directory — it doesn't assume any
particular volume naming scheme, since a set might have been produced by a
different tool with different naming conventions than `layout.rs`'s.

## 7. Volume layout (write side only)

`layout.rs` splits recovery blocks across `<base>.volFIRST+COUNT.par2`
files with exponentially growing counts — 1, 2, 4, 8, … blocks per volume
— so a downloader only needs to fetch as many volumes as the amount of
damage actually requires, rather than all-or-nothing. This is purely a
`create`-side decision: nothing on the read side (`recovery_set.rs`)
depends on it, since volumes are discovered by scanning the directory and
matching recovery-set ID, not by parsing file names.

## 8. Where the numbers come from

The SIMD-vs-scalar speedup claims in `ROADMAP.md` and `CHANGELOG.md` are
measured, not estimated — `cargo test --release -p parmesan-par2 --lib
gf16_mac::tests::throughput_scalar_vs_dispatched -- --ignored --nocapture`
reproduces them locally, and `cargo bench -p parmesan-par2`
(`benches/decode_throughput.rs`) gives the same numbers with `criterion`'s
statistical rigor, plus the matrix-inversion cost curve referenced in §4:
`O(m³)` scaling is not just theoretical — it's directly visible in the
measured numbers (`m=500 → 351 ms`, `m=1000 → 2.93 s`, an 8.3× cost
increase for a 2× size increase, matching the 8× the exponent predicts).
Cross-tool compatibility claims are similarly backed by
`crates/parmesan/tests/par2cmdline_compat.rs` (`--ignored`, requires a
`par2` binary on `PATH`), not just self-consistency within this crate.
