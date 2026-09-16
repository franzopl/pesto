# `parmesan` roadmap

Active work for the PAR2 creation, verification and repair library. The
completed phase history is preserved in
[`docs/roadmap-history/parmesan-ROADMAP-legacy.md`](../../docs/roadmap-history/parmesan-ROADMAP-legacy.md).

## Current status

Create, verify and repair are implemented with scalar, SSSE3 and AVX2 paths,
memory-bounded encoding, CLI subcommands and PAR2 compatibility tests. The
remaining work is portability, streaming under extreme memory pressure, CLI
coverage and documentation.

## Priority 1 — Portability and memory

- [ ] Add and validate GFNI, AVX-512 and NEON decode paths on representative
      hardware or CI targets.
- [ ] Reconstruct damaged data in column chunks so repair memory scales with a
      bounded chunk size rather than the complete slice size.
- [ ] Add a repair progress bar where the operation duration justifies it.

## Priority 2 — Compatibility and robustness

- [ ] Add a checked-in fixture corpus of real `.par2` files covering slice
      sizes, volume counts and Unicode names.
- [ ] Add an optional non-blocking CI job that runs compatibility tests with a
      real `par2cmdline` implementation.
- [ ] Add a true `cargo-fuzz` harness for `packet_reader` when nightly tooling
      is available.
- [ ] Revisit the duplicated multiply-accumulate implementation only if a
      measurable maintenance or performance benefit justifies the refactor.

## Priority 3 — CLI capabilities

- [ ] Support newline- and NUL-separated input file lists and, if still useful,
      piped process input.
- [ ] Add explicit symlink handling and configurable stored path formats.
- [ ] Add configurable recovery-volume schemes, volume counts and naming
      conventions beyond the current default.
- [ ] Add structured JSON/progress output and optional write synchronization.
- [ ] Add optional packet redundancy and explicit Unicode packet controls.

## Priority 4 — API and documentation

- [ ] Audit the public API, add module documentation and enable
      `#![deny(missing_docs)]` without warnings.
- [ ] Add runnable library examples and complete the CLI flag/exit-code
      documentation, including man-page generation if adopted.
- [ ] Keep `INTERNALS.md` current with the Reed-Solomon, packet, SIMD and
      memory-model explanations.
- [ ] Document changelog-update rules for contributors.

## Completion criteria

Every change needs correctness coverage, architecture-specific validation where
applicable, and updated documentation. Run:

```bash
cargo fmt --check
cargo clippy -p parmesan-par2 --all-targets -- -D warnings
cargo test -p parmesan-par2
```

## References

- [`crates/parmesan/README.md`](README.md)
- [`crates/parmesan/INTERNALS.md`](INTERNALS.md)
- [`crates/parmesan/CHANGELOG.md`](CHANGELOG.md)
- [`workspace roadmap`](../../ROADMAP.md)
