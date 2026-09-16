# `penne` roadmap

Active work for the NZB downloader and library. Completed phases and their
implementation history are preserved in
[`docs/roadmap-history/penne-ROADMAP-legacy.md`](../../docs/roadmap-history/penne-ROADMAP-legacy.md).

## Current status

The core downloader is implemented: NZB loading, pooled NNTP retrieval, yEnc
decoding, streaming assembly, resume/cache, availability checks, PAR2
verification/repair, de-obfuscation, extraction and processing modes are
covered by tests. `sugo` is the separate web UI built on this library.

## Active work

### Recovery and visibility

- [ ] Fetch extra PAR2 volumes on demand instead of downloading every listed
      volume before verification.
- [ ] Add live progress reporting for the PAR2 verify phase.

### Performance

- [ ] Investigate a double-buffered writer or buffer pool for assembly, guided
      by real profiling data.
- [ ] Investigate incremental, `DirectUnpack`-style archive extraction.
- [ ] Benchmark the complete pipeline against a real indexer/provider pair.

## Scope boundaries

- The downloader must remain usable as a library; CLI behavior belongs in the
  binary and shared processing belongs in the library modules.
- `sugo` owns web/API behavior and must not be embedded into `penne`.
- No optimization should be accepted without correctness coverage and a
  reproducible measurement.

## Completion criteria

For each item, update the implementation tests and changelog, keep external
services and hooks out of normal tests, and run:

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -p penne
```

## References

- [`crates/penne/README.md`](README.md)
- [`crates/penne/CHANGELOG.md`](CHANGELOG.md)
- [`crates/sugo/README.md`](../sugo/README.md)
