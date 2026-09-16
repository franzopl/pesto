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

---

## Phase 27 — Stable High-Level Creation API (#188)

`parmesan-par2` can create a complete recovery set, but that orchestration is
currently implemented by the CLI. Consumers embedding the crate must assemble
packets, volumes, hashing, and encoder passes themselves. This phase exposes a
small, stable library API that performs the same work without spawning the
`parmesan` binary.

**Scope and principles:**

- The CLI and the public API use one creation engine; neither grows a
  separate implementation.
- The normal API accepts filesystem paths and returns structured output. The
  low-level encoder and packet APIs remain available for expert callers.
- The library never prints to stdout or stderr. Progress is reported through
  structured events.
- Output is preflighted and staged so failures and cancellation do not leave a
  partial recovery set in the destination directory.
- A library operation does not configure Rayon globally. Its requested thread
  count applies only to that operation.

**Explicit non-goals for this phase:** generic `Read` inputs, remote/object
storage backends, custom volume layouts, an async callback framework, and
manual SIMD/layout selection in the high-level API. Each would expand the
public contract without helping the primary embedding use case.

### 27a — Public API contract (Complexity: Low)

- [x] Design `parmesan::create` around `CreateRequest`, `Recovery`,
      `CreateReport`, `CreateEvent`, and a non-exhaustive public error type.
- [x] Make percentage recovery and exact recovery-block counts mutually
      exclusive in the type model.
- [x] Define default output naming, directory expansion, overwrite policy,
      cancellation behavior, and the report's stable fields before exposing
      the API.
- [x] Add compile-tested API examples to lock in ergonomic use by a separate
      Rust application.

### 27b — Shared creation engine (Complexity: High)

- [x] Extract file discovery, canonical File ID ordering, geometry planning,
      multi-pass encoding, packet construction, and volume writing from
      `main.rs` into a library-owned creation engine.
- [x] Make `parmesan create` a thin Clap-to-request adapter over that engine,
      retaining its existing observable behavior and output names.
- [x] Preserve current correctness behavior for empty files, exact-multiple
      slices, slice windows, recovery offsets, and memory-limited passes.

### 27c — Transactional output handling (Complexity: Medium)

- [x] Resolve every final output path and refuse collisions before expensive
      input reads when overwrite is disabled.
- [x] Write index and recovery volumes into a same-filesystem staging area;
      publish only after successful completion.
- [x] Remove staged files on error or cancellation and return the actual
      published paths in `CreateReport`.
- [ ] Test an I/O failure path in addition to the existing output-collision,
      cancellation, and cleanup coverage.

### 27d — Progress, cancellation, and resource isolation (Complexity: Medium)

- [x] Emit phase, pass, byte-read, and completed-volume events without
      terminal output from library code.
- [x] Support cooperative cancellation at file-read boundaries and guarantee
      staging cleanup.
- [x] Run the operation on a private Rayon pool so `threads` never mutates or
      depends on the host process's global Rayon configuration.

### 27e — Public API and compatibility tests (Complexity: Medium)

- [x] Publish `create()` and optional progress-aware entry points using the
      extracted engine.
- [x] Add integration tests that use only public API types for single and
      multiple files, recursive directories, empty files, memory-limited
      passes, recovery offsets, and output reporting.
- [x] Verify that API and CLI outputs are identical for representative
      fixtures, then retain the existing optional `par2cmdline`
      interoperability matrix.

### 27f — Documentation and release (Complexity: Low)

- [x] Add a runnable library example to the README and crate rustdoc.
- [x] Document API stability, error behavior, resource ownership, and the
      distinction between the high-level and expert-level APIs.
- [x] Update the changelog and release a compatible incremental crate version
      after `fmt`, Clippy, tests, and compatibility checks pass.
