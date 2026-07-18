# parmesan

Fast, standalone PAR2 (Reed-Solomon) creation, verification and repair tool.

`parmesan` encodes input files into PAR2 recovery archives using a
multi-threaded Reed-Solomon implementation with SIMD acceleration (Scalar,
SSSE3, AVX2, AVX2+GFNI, AVX-512+GFNI, ARM NEON), and can verify and repair
files against an existing recovery set. Recovery data is compatible with
other PAR2 tools (verified against `par2cmdline` — see
[Compatibility](#compatibility) below).

It is designed to be used both as a CLI binary and as a library embedded in
[pesto](https://github.com/franzopl/pesto), a fast Usenet poster.

## Usage (CLI)

```
parmesan [create] [OPTIONS] <FILES>...
parmesan verify [OPTIONS] <INDEX.par2>
parmesan repair [OPTIONS] <INDEX.par2>
```

Invoking `parmesan` with no subcommand is an alias for `create`, kept for
backwards compatibility with versions before subcommands existed.

### `create`

```
parmesan create [OPTIONS] <FILES>...
```

| Flag | Default | Description |
|------|---------|-------------|
| `-r`, `--recovery-pct` | `10` | Percentage of recovery data to generate |
| `-s`, `--slice-size` | auto | Manual PAR2 slice size, e.g. `"1 MiB"` |
| `-n`, `--slice-count` | auto | Target number of input slices |
| `--recovery-count` | — | Exact number of recovery blocks instead of a percentage |
| `-m`, `--memory-limit` | `1 GiB` | Maximum RAM used for recovery buffers |
| `-t`, `--threads` | auto | Number of threads for parallel compute |
| `--simd` | `auto` | SIMD path: `auto`, `scalar`, `ssse3`, `avx2`, `avx2-gfni`, `avx512-gfni`, `neon` |
| `-o`, `--out-dir` | `.` | Output directory for `.par2` files |
| `-b`, `--base-name` | first input file | Base name for output `.par2` files |
| `-q`, `--quiet` | off | Suppress progress and geometry output |
| `-O`, `--overwrite` | off | Overwrite existing `.par2` files instead of failing |
| `--no-index` | off | Skip generating the index (`.par2`) file |
| `-R`, `--recurse` | off | Expand directories into their constituent files |
| `-c`, `--comment` | — | Embed a comment in the Creator packet (repeatable) |
| `-e`, `--recovery-offset` | `0` | Exponent of the first recovery block |

### `verify`

```
parmesan verify [OPTIONS] <INDEX.par2>
```

Re-hashes the files described by `INDEX.par2` (scanning its directory for
every volume belonging to the same recovery set) and reports which are OK,
damaged, or missing. Exit code follows the PAR2 convention: `0` = OK,
`1` = damaged but repairable, `2` = damaged beyond what the available
recovery data can fix.

| Flag | Description |
|------|-------------|
| `-q`, `--quiet` | Suppress the per-file report; only the summary line is printed |
| `--json` | Emit a machine-readable JSON report instead of human-readable text |

### `repair`

```
parmesan repair [OPTIONS] <INDEX.par2>
```

Reconstructs damaged or missing slices via Reed-Solomon decoding and writes
them back to disk. Every reconstructed slice's checksum is verified against
the recovery set's IFSC packet *before* anything is written — a mismatch
aborts that file's repair instead of writing corrupted data.

| Flag | Description |
|------|-------------|
| `--dry-run` | Reconstruct and checksum-verify without writing any files |
| `-o`, `--out-dir` | Write repaired files here instead of overwriting originals in place |
| `-q`, `--quiet` | Suppress the per-file report; only the summary line is printed |
| `--json` | Emit a machine-readable JSON report instead of human-readable text |

## Compatibility

Recovery sets are validated bidirectionally against `par2cmdline`:
`parmesan`-created sets are readable and repairable by `par2cmdline`, and
`par2cmdline`-created sets are readable and repairable by `parmesan` — including
multi-file sets, where Reed-Solomon coefficients are assigned by ascending
File ID per the PAR2 spec (not by file order), matching the reference
implementation. See `crates/parmesan/tests/par2cmdline_compat.rs`:

```bash
cargo test -p parmesan-par2 --test par2cmdline_compat -- --ignored
```

## Building

```bash
cargo build --release -p parmesan
```

The optimised binary lands at `target/release/parmesan`.

See [`RELEASING.md`](RELEASING.md) for how versions get published to
crates.io and released on GitHub.

## License

MIT
