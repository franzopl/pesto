# Workspace roadmap

Active development plan for the `pesto` workspace. This file contains only
unfinished or intentionally deferred work. Completed releases are documented in
the crate changelogs and the previous phase-by-phase roadmap is preserved in
[`docs/roadmap-history/ROADMAP-legacy.md`](docs/roadmap-history/ROADMAP-legacy.md).

## Current focus

The posting pipeline is mature. The current product focus is UpaPasta v2,
followed by downloader completion, API stabilization and release engineering.

Architecture principles:

- `pesto` stays small and optimized for posting.
- `upapasta` owns UX, orchestration and cataloguing.
- `penne` owns downloading, verification, repair and extraction.
- `sugo` remains a separate web UI built on `penne`.
- Shared behavior is integrated through library APIs, not subprocesses.

## Priority 1 — UpaPasta v2

Detailed implementation notes live in [`crates/upapasta/ROADMAP.md`](crates/upapasta/ROADMAP.md).

- [ ] Metadata enrichment: TMDb lookup and improved NFO generation.
- [ ] First-run setup wizard.
- [ ] Directory-level queue selection.
- [ ] Theme support with configurable colors.
- [ ] TUI performance tuning during long uploads.
- [ ] Consistent error handling and user feedback.
- [ ] Define the migration path from the Python version, then retire or archive it.

## Priority 2 — `penne` and `sugo`

`sugo` follows the downloader capabilities exposed by `penne`.
See [`crates/penne/ROADMAP.md`](crates/penne/ROADMAP.md) for phase details.

- [ ] Fetch extra PAR2 volumes on demand instead of downloading all volumes.
- [ ] Add a double-buffered writer or buffer pool to the assembly path.
- [ ] Investigate incremental archive extraction.
- [ ] Benchmark the complete pipeline against a real indexer/provider pair.

## Priority 3 — API stability and releases

- [ ] Define and freeze the supported `pesto` public API.
- [ ] Add `#![deny(missing_docs)]` and runnable examples to `parmesan`.
- [ ] Expand the test strategy with third-party PAR2 fixtures, property tests and
      optional `par2cmdline` verification.
- [ ] Complete portable packaging for Linux, Windows and macOS, including
      aarch64, and generate man pages where appropriate.
- [ ] Complete flag, exit-code and architecture documentation for every crate.

## Priority 4 — Evidence-driven engine work

- [ ] Detect server-side `441 Too many articles` limits and adapt pipeline depth.
- [ ] Revisit season PAR2 from spooled slices only if measurements show that the
      additional read pass is a meaningful cost.

## Deferred / not scheduled

These ideas are intentionally not commitments. Promote one to the active list
only after a concrete use case, owner and acceptance criteria exist:

- Status/health endpoint for headless posting.
- Compressed NZB output.
- Non-seekable input and process-pipeline support.
- Server compatibility profiles.

## Completion criteria

Before marking a task complete:

- update the relevant crate roadmap and changelog;
- add or update tests without real network, hooks or external side effects;
- run `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` and
  `cargo test`;
- remove the item from this file if no follow-up work remains.

## References

- [`crates/pesto/CHANGELOG.md`](crates/pesto/CHANGELOG.md)
- [`crates/parmesan/ROADMAP.md`](crates/parmesan/ROADMAP.md)
- [`crates/penne/ROADMAP.md`](crates/penne/ROADMAP.md)
- [`crates/upapasta/ROADMAP.md`](crates/upapasta/ROADMAP.md)
- [`docs/roadmap-history/ROADMAP-legacy.md`](docs/roadmap-history/ROADMAP-legacy.md)
